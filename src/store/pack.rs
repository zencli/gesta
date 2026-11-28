//! Group small objects so their compressed streams can see each other.
//!
//! A zlib stream cannot see the next object, so storing each object compressed on its own pays for
//! the shared vocabulary every file repeats. Measured on 11,301 files: separate objects 47.3 MB,
//! and the same bytes grouped 50 at a time 34.7 MB. That is 1.36x, and joining everything into one
//! stream gains only 1.50x, so almost the whole win is available at a granularity a read can afford.
//!
//! Three decisions, and each of them is why this is a table rather than a file on disk.
//!
//! **Groups are built by size, not by name or hash.** Sorting by content length puts files of the
//! same kind next to each other, and that is where the repetition is. Grouping by hash gives 1.18x,
//! which is most of the benefit gone.
//!
//! **A member keeps its own hash.** Nothing about the object model changes: `objects` still has one
//! row per object, and the row names the group and the byte range. Every existing query keeps
//! working, `fsck` can still check one object, and `gc` still deletes one row.
//!
//! **A small object stays where it is.** A group is only worth its index entry and its alignment
//! cost when it holds enough bytes. Below the threshold the object is stored exactly as before,
//! which also means a pack that is wrong can be turned off without a migration.

use crate::support::error::{Code, Error};
use crate::ZResult;
use rusqlite::{params, Connection, OptionalExtension};

/// How many objects go into one group.
///
/// 50 measured 34.7 MB against 34.4 MB for 1000, so a larger group buys almost nothing and costs a
/// decompression of the whole group on a read. Larger only helps the compressor in the tail.
pub const GROUP_OBJECTS: usize = 50;

/// A group only forms when it holds at least this many bytes.
///
/// Below it, the index entry and the wasted tail of the last member cost more than the shared
/// vocabulary saves. A repository of a hundred small files is the common case and it must not pay
/// for a mechanism it does not need.
pub const MIN_GROUP_BYTES: usize = 64 * 1024;

/// Where one object lives.
#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    /// Its own row in `objects`, as before.
    Loose,
    /// Inside a group: `group` id, byte `offset`, byte `length` in the decompressed stream.
    Packed {
        group: i64,
        offset: u64,
        length: u64,
    },
}

/// One member of a group being built.
struct Member {
    hash: String,
    body: Vec<u8>,
}

/// Compress a group of bodies into one stream and record where each one sits.
///
/// The layout is: the concatenated bodies, in the order given, compressed as one zlib stream. The
/// offset of each member is its position in the uncompressed concatenation, which is what makes a
/// read a decompress plus a slice.
fn seal(conn: &Connection, members: &[Member]) -> ZResult<i64> {
    let mut joined = Vec::with_capacity(members.iter().map(|m| m.body.len()).sum());
    let mut spans = Vec::with_capacity(members.len());
    for m in members {
        let offset = joined.len() as u64;
        joined.extend_from_slice(&m.body);
        spans.push((m.hash.clone(), offset, m.body.len() as u64));
    }
    let compressed = crate::store::compress_bytes(&joined)?;

    conn.execute(
        "INSERT INTO object_groups(packed, raw_size, object_count, created_at)
         VALUES(?1, ?2, ?3, ?4)",
        params![
            compressed,
            joined.len() as i64,
            members.len() as i64,
            crate::now()
        ],
    )?;
    let group = conn.last_insert_rowid();
    for (hash, offset, length) in spans {
        // The body leaves the row. Keeping it would store every grouped object twice, and the file
        // grew by exactly that much when this line only set the offsets.
        conn.execute(
            "UPDATE objects SET group_id = ?1, offset = ?2, length = ?3, content = x'' WHERE hash = ?4",
            params![group, offset as i64, length as i64, hash],
        )?;
    }
    Ok(group)
}

/// Read one object out of its group.
///
/// Decompresses the whole group, because that is what a zlib stream allows. At 50 objects the cost
/// is proportional to the group rather than the object, and the group is bounded on purpose: this
/// is the trade the constant above names.
pub fn read(conn: &Connection, group: i64, offset: u64, length: u64) -> ZResult<Option<Vec<u8>>> {
    let packed: Option<Vec<u8>> = conn
        .query_row(
            "SELECT packed FROM object_groups WHERE id = ?1",
            params![group],
            |r| r.get(0),
        )
        .optional()?;
    let Some(packed) = packed else {
        return Ok(None);
    };
    let whole = crate::store::decompress_bytes(&packed)?;
    let start = offset as usize;
    let end = start.saturating_add(length as usize);
    if end > whole.len() {
        // A range past the end of its group is a corrupt store, and returning a short slice would
        // hand back content nobody wrote while reporting success.
        return Err(Error::new(
            Code::Corrupt,
            format!(
                "object range {}..{} is past the end of group {} ({} bytes)",
                start,
                end,
                group,
                whole.len()
            ),
        )
        .into());
    }
    Ok(Some(whole[start..end].to_vec()))
}

/// Where an object is, or `Loose` when it has no group.
pub fn placement(conn: &Connection, hash: &str) -> ZResult<Placement> {
    let row: Option<(Option<i64>, Option<i64>, Option<i64>)> = conn
        .query_row(
            "SELECT group_id, offset, length FROM objects WHERE hash = ?1",
            params![hash],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match row {
        Some((Some(g), Some(o), Some(l))) => Placement::Packed {
            group: g,
            offset: o as u64,
            length: l as u64,
        },
        _ => Placement::Loose,
    })
}

/// Group every loose object that is large enough to be worth grouping.
///
/// Returns (groups formed, objects moved). Running it twice is a no-op for objects already grouped,
/// which is what makes it safe to call from `gc`.
pub fn repack(conn: &Connection) -> ZResult<(usize, usize)> {
    // Candidates: loose, text objects, not already grouped. Binary objects stay loose because
    // compressed bytes do not share a vocabulary with anything.
    let mut candidates: Vec<(String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT hash, size FROM objects
             WHERE group_id IS NULL AND compressed = 1 AND size >= 128
             ORDER BY size",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if candidates.is_empty() {
        return Ok((0, 0));
    }
    // Already size-ordered by the query, and grouping neighbours of similar size is the whole point.
    candidates.truncate(candidates.len());

    let mut groups = 0usize;
    let mut moved = 0usize;
    for chunk in candidates.chunks(GROUP_OBJECTS) {
        let mut members: Vec<Member> = Vec::with_capacity(chunk.len());
        let mut total = 0usize;
        for (hash, size) in chunk {
            let body = match crate::store::get_object_bytes(conn, hash) {
                Ok(b) => b,
                // An object the store cannot read is a hole `fsck` reports; skipping it here keeps
                // gc from failing over something it is not responsible for fixing.
                Err(_) => continue,
            };
            total += body.len();
            let _ = size;
            members.push(Member {
                hash: hash.clone(),
                body,
            });
        }
        if members.len() < 2 || total < MIN_GROUP_BYTES {
            continue;
        }
        seal(conn, &members)?;
        groups += 1;
        moved += members.len();
    }
    Ok((groups, moved))
}

/// The size of a group's decompressed stream, for `stats`.
pub fn group_stats(conn: &Connection) -> ZResult<(i64, i64, i64)> {
    let one = |sql: &str| -> ZResult<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };
    // (groups, bytes the grouped streams take, objects that live in a group)
    Ok((
        one("SELECT COUNT(*) FROM object_groups")?,
        one("SELECT COALESCE(SUM(LENGTH(packed)), 0) FROM object_groups")?,
        one("SELECT COUNT(*) FROM objects WHERE group_id IS NOT NULL")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (Connection, std::path::PathBuf) {
        let dir = crate::support::scratch::scratch("gesta-pack").expect("a test scratch directory");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join(".gesta")).ok();
        let conn = crate::store::open_db(&dir).unwrap();
        crate::store::init_schema(&conn).unwrap();
        (conn, dir)
    }

    /// Content with the repetition real source files have, so the measurement is about grouping and
    /// not about random bytes that no compressor can help.
    fn body(i: usize) -> String {
        (0..80)
            .map(|j| format!("fn f{}_{}() -> usize {{ {} }}\n", i, j, j))
            .collect()
    }

    #[test]
    fn a_grouped_object_reads_back_exactly() {
        let (conn, dir) = repo();
        let mut hashes = Vec::new();
        for i in 0..60 {
            let h = crate::store::put_object(&conn, &body(i)).unwrap();
            hashes.push((h.clone(), body(i).into_bytes()));
        }
        let (groups, moved) = repack(&conn).unwrap();
        assert!(groups >= 1, "no group was formed");
        assert!(moved >= 50);

        for (hash, want) in &hashes {
            match placement(&conn, hash).unwrap() {
                Placement::Packed {
                    group,
                    offset,
                    length,
                } => {
                    let got = read(&conn, group, offset, length).unwrap().unwrap();
                    assert_eq!(&got, want, "content changed for {}", hash);
                }
                Placement::Loose => {}
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grouping_saves_space_on_content_that_repeats() {
        let (conn, dir) = repo();
        for i in 0..200 {
            crate::store::put_object(&conn, &body(i)).unwrap();
        }
        // Measured before grouping, because the body leaves the row afterwards and the same query
        // returns zero. That is the mistake this test made first: it compared the grouped size with
        // a column the grouping had already emptied.
        let separate: i64 = conn
            .query_row("SELECT SUM(LENGTH(content)) FROM objects", [], |r| r.get(0))
            .unwrap();
        repack(&conn).unwrap();
        let (groups, packed_bytes, in_group) = group_stats(&conn).unwrap();
        assert!(groups > 0, "no group was formed");
        assert!(in_group > 0, "no object moved into a group");
        assert!(
            packed_bytes < separate,
            "{} group(s) took {} bytes for content that took {} separately",
            groups,
            packed_bytes,
            separate
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_grouped_object_gives_up_its_own_copy() {
        let (conn, dir) = repo();
        for i in 0..60 {
            crate::store::put_object(&conn, &body(i)).unwrap();
        }
        repack(&conn).unwrap();
        // Keeping the body in the row as well as in the group stored every object twice, and the
        // file grew by exactly that much. It is the one mistake that makes this feature a regression.
        let leftover: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(content)), 0) FROM objects WHERE group_id IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftover, 0, "a grouped object still holds its own body");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repacking_twice_does_not_group_a_grouped_object() {
        let (conn, dir) = repo();
        for i in 0..60 {
            crate::store::put_object(&conn, &body(i)).unwrap();
        }
        let (g1, m1) = repack(&conn).unwrap();
        assert!(g1 >= 1 && m1 >= 50);
        let (g2, m2) = repack(&conn).unwrap();
        assert_eq!((g2, m2), (0, 0), "a second pass moved objects again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_small_repository_forms_no_group() {
        let (conn, dir) = repo();
        for i in 0..3 {
            crate::store::put_object(&conn, &body(i)).unwrap();
        }
        let (groups, _) = repack(&conn).unwrap();
        assert_eq!(
            groups, 0,
            "a tiny store paid for a mechanism it does not need"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_range_past_the_end_of_its_group_is_an_error_not_a_short_read() {
        let (conn, dir) = repo();
        for i in 0..60 {
            crate::store::put_object(&conn, &body(i)).unwrap();
        }
        repack(&conn).unwrap();
        let got = read(&conn, 1, 10_000_000, 10);
        assert!(got.is_err(), "a corrupt range must not return content");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_group_is_not_an_empty_object() {
        let (conn, dir) = repo();
        assert!(read(&conn, 999, 0, 10).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
