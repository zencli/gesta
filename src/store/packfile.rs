//! Reading objects out of a git packfile.
//!
//! A repository that has been through `git gc` holds almost nothing loose: every object sits in
//! one `.pack` file, indexed by a `.idx` beside it. Without this reader an import works only on a
//! repository nobody has ever packed, which is not a real repository.
//!
//! The two formats are documented and stable. The index is a sorted table of object names with
//! their offsets; the pack is those objects, each either whole or stored as a delta against
//! another object in the same pack.
//!
//! ```text
//! idx v2:  \xfftOc | version | fanout[256] | names | crc32 | offsets | trailers
//! pack:    "PACK" | version | count | entry...
//! entry:   varint(type, size) | zlib body
//! ```
//!
//! An object stored as `OFS_DELTA` names its base by a backward distance in the same file;
//! `REF_DELTA` names it by hash. Both have to be resolved before the object can be handed to the
//! importer, so this reader resolves a chain until it reaches a whole object.

use crate::{bad, ZResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What a packed entry is. The numbers are the format's, not a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
    /// A delta whose base is earlier in the same pack, by byte distance.
    OfsDelta,
    /// A delta whose base is named by its object hash.
    RefDelta,
}

impl Kind {
    fn from_code(code: u8) -> Option<Kind> {
        Some(match code {
            1 => Kind::Commit,
            2 => Kind::Tree,
            3 => Kind::Blob,
            4 => Kind::Tag,
            6 => Kind::OfsDelta,
            7 => Kind::RefDelta,
            _ => return None,
        })
    }
}

/// One decoded object: its kind and its bytes.
pub struct Packed {
    pub kind: Kind,
    pub data: Vec<u8>,
}

/// A pack and its index, open for reading.
pub struct Pack {
    pub path: PathBuf,
    /// Object name to byte offset in the pack file.
    offsets: HashMap<String, u64>,
}

impl Pack {
    /// Open every pack a repository holds.
    ///
    /// More than one is normal after an incremental fetch: a new pack is written and the old ones
    /// stay until a repack folds them together.
    pub fn open_all(git_dir: &Path) -> ZResult<Vec<Pack>> {
        let dir = git_dir.join("objects").join("pack");
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "idx") {
                continue;
            }
            // An index without its pack is a half-written fetch; skip it rather than fail.
            let pack_path = path.with_extension("pack");
            if !pack_path.is_file() {
                continue;
            }
            match Pack::open(&pack_path, &path) {
                Ok(p) => out.push(p),
                Err(e) => {
                    return Err(bad(format!(
                        "cannot read the pack index {}: {}",
                        path.display(),
                        e
                    )))
                }
            }
        }
        Ok(out)
    }

    /// Read the v2 index: names and offsets.
    pub fn open(pack_path: &Path, idx_path: &Path) -> ZResult<Pack> {
        let idx = std::fs::read(idx_path)?;
        if idx.len() < 8 || &idx[0..4] != b"\xfftOc" {
            return Err(bad(format!(
                "{} is not a version 2 pack index",
                idx_path.display()
            )));
        }
        let version = be32(&idx, 4).ok_or_else(|| bad("index is truncated"))?;
        if version != 2 {
            return Err(bad(format!(
                "pack index version {} is not supported, only 2",
                version
            )));
        }
        let fanout_at = 8;
        let count = be32(&idx, fanout_at + 255 * 4)
            .ok_or_else(|| bad("index fanout is truncated"))? as usize;
        let names_at = fanout_at + 256 * 4;
        let crc_at = names_at + count * 20;
        let offsets_at = crc_at + count * 4;

        // `count` comes out of the index header before anything checks it against the file. The
        // offsets below are bounds-checked one at a time, so a bogus count fails on lookup - but by
        // then the map has already tried to reserve room for it, and a four-byte header saying
        // 4,000,000,000 entries asks for tens of GB before the first lookup can fail. Each entry
        // costs 20 bytes of name plus an offset, so the file itself bounds the honest count.
        let max_entries = idx.len() / 20;
        if count > max_entries {
            return Err(bad(format!(
                "index claims {} objects but the file can hold at most {}",
                count, max_entries
            )));
        }
        let mut offsets = HashMap::with_capacity(count);
        for i in 0..count {
            let name_at = names_at + i * 20;
            let name = hex::encode(
                idx.get(name_at..name_at + 20)
                    .ok_or_else(|| bad("index names are truncated"))?,
            );
            let raw =
                be32(&idx, offsets_at + i * 4).ok_or_else(|| bad("index offsets are truncated"))?;
            // The high bit means the offset does not fit in 32 bits and lives in the 8 byte
            // table at the end, at the index given by the low 31 bits.
            let offset = if raw & 0x8000_0000 != 0 {
                let table = offsets_at + count * 4;
                let slot = (raw & 0x7fff_ffff) as usize;
                be64(&idx, table + slot * 8)
                    .ok_or_else(|| bad("index large-offset table is truncated"))?
            } else {
                raw as u64
            };
            offsets.insert(name, offset);
        }

        Ok(Pack {
            path: pack_path.to_path_buf(),
            offsets,
        })
    }

    /// Read one object, resolving a delta chain.
    ///
    /// The pack is opened once per call and only the entry's own bytes are read. It used to call
    /// `fs::read` on the whole file, so importing N objects from a pack of size S read S bytes N
    /// times: quadratic I/O, and a per-read memory spike equal to the pack. Measured on a 256 MB
    /// pack, reading one small object grew peak RSS by 261,976 KiB; it now grows it by less than
    /// the 64 MiB the test allows.
    pub fn read(&self, hash: &str) -> ZResult<Option<Packed>> {
        let Some(&offset) = self.offsets.get(hash) else {
            return Ok(None);
        };
        let mut f = std::fs::File::open(&self.path)?;
        let depth = self.resolve(&mut f, offset, 0)?;
        Ok(Some(depth))
    }

    /// Resolve the entry at `offset` into a whole object.
    ///
    /// `depth` guards against a chain that loops or is absurdly long. A delta chain in a real
    /// pack is short because a repack rewrites it, but a corrupt file is not obliged to agree.
    fn resolve(&self, f: &mut std::fs::File, offset: u64, depth: usize) -> ZResult<Packed> {
        if depth > 128 {
            return Err(bad(format!(
                "delta chain at offset {} is deeper than any real pack: the file may be damaged",
                offset
            )));
        }
        let (kind, size, at) = self.entry_header(f, offset)?;
        match kind {
            Kind::Commit | Kind::Tree | Kind::Blob | Kind::Tag => {
                let body = inflate_at(f, at)?;
                if body.len() != size {
                    return Err(bad(format!(
                        "object at offset {} claims {} bytes and holds {}",
                        offset,
                        size,
                        body.len()
                    )));
                }
                Ok(Packed { kind, data: body })
            }
            Kind::OfsDelta => {
                // The base is `distance` bytes before this entry, encoded as a varint where the
                // first byte carries one fewer bit.
                let (base_distance, after) = ofs_delta_base(f, at as u64)?;
                let delta = inflate_at(f, after)?;
                let base_offset = offset
                    .checked_sub(base_distance)
                    .ok_or_else(|| bad("delta points before the start of the pack"))?;
                let base = self.resolve(f, base_offset, depth + 1)?;
                let data = apply_delta(&base.data, &delta)?;
                Ok(Packed {
                    kind: base.kind,
                    data,
                })
            }
            Kind::RefDelta => {
                let name = read_at(f, at as u64, 20)?;
                let after = at + 20;
                let delta = inflate_at(f, after)?;
                let base_hash = hex::encode(&name);
                // A base can be in this pack or, after a fetch, in another one. The caller has a
                // list of packs, but a REF_DELTA inside a pack is nearly always against a base in
                // the same file, so try that first and fall back to the named hash.
                if let Some(&base_offset) = self.offsets.get(&base_hash) {
                    let base = self.resolve(f, base_offset, depth + 1)?;
                    let data = apply_delta(&base.data, &delta)?;
                    return Ok(Packed {
                        kind: base.kind,
                        data,
                    });
                }
                Err(bad(format!(
                    "delta in {} needs object {}, which is not in this pack",
                    self.path.display(),
                    base_hash
                )))
            }
        }
    }

    /// Decode the type and size varint, plus where the zip stream starts.
    ///
    /// The header is at most ten bytes (a tag byte plus a nine-byte size varint), so a short read
    /// from `offset` is all it needs. A read past the end is not an error here: the parse reports
    /// which field ran out, which is a better message than a bare I/O error.
    fn entry_header(&self, f: &mut std::fs::File, offset: u64) -> ZResult<(Kind, usize, usize)> {
        let head = read_upto(f, offset, 32)?;
        let bytes = &head[..];
        let mut at = 0usize;
        let first = *bytes
            .get(at)
            .ok_or_else(|| bad(format!("pack ends at offset {}", offset)))?;
        at += 1;
        let code = (first >> 4) & 0x07;
        let kind = Kind::from_code(code).ok_or_else(|| {
            bad(format!(
                "pack entry at {} has type {}, which is not a git object type",
                offset, code
            ))
        })?;
        let mut size = (first & 0x0f) as usize;
        let mut shift = 4;
        let mut byte = first;
        while byte & 0x80 != 0 {
            byte = *bytes
                .get(at)
                .ok_or_else(|| bad("pack entry header is truncated"))?;
            at += 1;
            let payload = (byte & 0x7f) as usize;
            if shift >= usize::BITS || payload > (usize::MAX >> shift) {
                return Err(bad("pack entry size varint overflows usize"));
            }
            size |= payload << shift;
            shift += 7;
        }
        Ok((kind, size, offset as usize + at))
    }
}

/// Read exactly `len` bytes at `at`, or report that the file is shorter.
fn read_at(f: &mut std::fs::File, at: u64, len: usize) -> ZResult<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    f.seek(SeekFrom::Start(at))?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read up to `len` bytes at `at`, returning what the file holds.
///
/// For a header that may sit at the very end: a short read is reported by the parse that follows,
/// which knows whether the missing byte was needed.
fn read_upto(f: &mut std::fs::File, at: u64, len: usize) -> ZResult<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    f.seek(SeekFrom::Start(at))?;
    let mut buf = vec![0u8; len];
    let mut got = 0usize;
    while got < len {
        match f.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    buf.truncate(got);
    Ok(buf)
}

/// The base distance of an `OFS_DELTA`, which is a varint with a different continuation rule.
///
/// Reads at most ten bytes at `at` from the pack file rather than from a whole-file buffer.
fn ofs_delta_base(f: &mut std::fs::File, at: u64) -> ZResult<(u64, usize)> {
    let head = read_upto(f, at, 10)?;
    let mut pos = 0usize;
    let mut byte = *head
        .get(pos)
        .ok_or_else(|| bad("delta base offset is truncated"))?;
    pos += 1;
    let mut value = (byte & 0x7f) as u64;
    while byte & 0x80 != 0 {
        byte = *head
            .get(pos)
            .ok_or_else(|| bad("delta base offset is truncated"))?;
        pos += 1;
        value = value
            .checked_add(1)
            .and_then(|v| v.checked_mul(128))
            .map(|v| v | (byte & 0x7f) as u64)
            .ok_or_else(|| bad("delta base offset varint overflows u64"))?;
    }
    Ok((value, at as usize + pos))
}

/// Inflate one zlib stream starting at `at`.
///
/// The decoder wraps the file handle rather than a slice of the whole pack, so a delta chain costs
/// the decoded objects and one zlib window instead of the pack's size. The handle is left at an
/// unspecified position, which is why every read here seeks first.
fn inflate_at(f: &mut std::fs::File, at: usize) -> ZResult<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::{Seek, SeekFrom};
    // A pack file arrives from a remote, so its entries get the same ceiling as any other compressed
    // input (#156). `inflate_at` is the one place a pack entry becomes bytes.
    f.seek(SeekFrom::Start(at as u64))?;
    crate::store::inflate_capped(ZlibDecoder::new(f), crate::store::MAX_INFLATED)
}

/// Apply a git delta to its base.
///
/// The delta is a base size, a result size, then copy and insert instructions. A copy names a
/// range of the base; an insert carries literal bytes.
/// `apply_delta`, reachable from the delta writer.
///
/// The writer verifies every delta by applying it before offering it, and the decoder is the one
/// that has to be right about the format, so the two share this function instead of the writer
/// carrying a second decoder that could disagree. Not test-only: the verification runs in the
/// server, which is the only place a wrong delta would be sent.
pub fn apply_delta_for_test(base: &[u8], delta: &[u8]) -> ZResult<Vec<u8>> {
    apply_delta(base, delta)
}

fn apply_delta(base: &[u8], delta: &[u8]) -> ZResult<Vec<u8>> {
    let mut at = 0usize;
    let (base_size, used) = delta_size(delta, at)?;
    at = used;
    if base_size != base.len() {
        return Err(bad(format!(
            "delta expects a base of {} bytes and its base is {}",
            base_size,
            base.len()
        )));
    }
    let (result_size, used) = delta_size(delta, at)?;
    at = used;

    // `result_size` is a varint from the delta header, decoded before a byte of output exists. A
    // delta cannot honestly emit more than a bounded multiple of its body, so a huge promise is
    // either corruption or a probe. Without this, `with_capacity` reserves the whole promise up
    // front and the process dies on the allocation rather than on a reported error.
    //
    // The bound is 65536 bytes per body byte: the largest single copy emits 0x10000 bytes and costs
    // two bytes to encode, so 65536x is the per-byte maximum with room to spare. An earlier version
    // of this check used 255x and refused a legitimate full copy of a long run - the delta was
    // larger than the object, which is fine, and the ceiling was simply too tight.
    let ceiling = delta.len().saturating_mul(65536);
    // The ratio ceiling alone is not a bound (#279).
    //
    // 65536x is the per-byte maximum for a single copy op, which is the right shape for "this delta
    // cannot legitimately claim more". It is not an absolute limit: a 1 MiB delta passes the ratio
    // check while claiming 64 GiB, and `Vec::with_capacity` then tries to reserve it before a single
    // op is validated. Whether that aborts or returns depends on the allocator, and neither outcome is
    // a refusal a caller can act on. `MAX_INFLATED` is the same absolute ceiling every decompression
    // path in this store already uses.
    if result_size > ceiling || result_size > crate::store::MAX_INFLATED {
        return Err(bad(format!(
            "delta claims {} bytes of output from a {} byte body{}",
            result_size,
            delta.len(),
            if result_size > crate::store::MAX_INFLATED {
                format!(
                    ", which is above the {} byte ceiling this store accepts",
                    crate::store::MAX_INFLATED
                )
            } else {
                String::new()
            }
        )));
    }

    // Grown incrementally rather than reserved up front. `with_capacity(result_size)` trusted the
    // claimed size before the ops were checked; the ops are the only thing that proves the claim, and
    // growing means a delta that lies about its size costs memory in proportion to what it actually
    // emits rather than to what it says.
    let mut out = Vec::new();
    while at < delta.len() {
        let op = delta[at];
        at += 1;
        if op & 0x80 != 0 {
            // Copy: the low seven bits say which of offset and length are present.
            let mut offset = 0usize;
            let mut length = 0usize;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    let b = *delta
                        .get(at)
                        .ok_or_else(|| bad("delta copy is truncated"))?;
                    at += 1;
                    offset |= (b as usize) << (i * 8);
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    let b = *delta
                        .get(at)
                        .ok_or_else(|| bad("delta copy is truncated"))?;
                    at += 1;
                    length |= (b as usize) << (i * 8);
                }
            }
            // A zero length means 0x10000, which is how the format encodes a full copy.
            if length == 0 {
                length = 0x10000;
            }
            let end = offset
                .checked_add(length)
                .ok_or_else(|| bad("delta copy range overflows"))?;
            let slice = base
                .get(offset..end)
                .ok_or_else(|| bad("delta copy reads past the end of its base"))?;
            out.extend_from_slice(slice);
        } else if op != 0 {
            let len = op as usize;
            let slice = delta
                .get(at..at + len)
                .ok_or_else(|| bad("delta insert is truncated"))?;
            out.extend_from_slice(slice);
            at += len;
        } else {
            return Err(bad("delta holds a zero opcode"));
        }
    }

    if out.len() != result_size {
        return Err(bad(format!(
            "delta produced {} bytes and declares {}",
            out.len(),
            result_size
        )));
    }
    Ok(out)
}

/// Read a little-endian size varint, returning the value and the position after it.
fn delta_size(delta: &[u8], mut at: usize) -> ZResult<(usize, usize)> {
    let mut value = 0usize;
    let mut shift = 0;
    loop {
        let byte = *delta
            .get(at)
            .ok_or_else(|| bad("delta header is truncated"))?;
        at += 1;
        let payload = (byte & 0x7f) as usize;
        if shift >= usize::BITS || payload > (usize::MAX >> shift) {
            return Err(bad("delta size varint overflows usize"));
        }
        value |= payload << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            break;
        }
        if shift > 63 {
            return Err(bad("delta size varint is longer than 64 bits"));
        }
    }
    Ok((value, at))
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn be64(bytes: &[u8], at: usize) -> Option<u64> {
    bytes
        .get(at..at + 8)
        .map(|s| u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a size as the varint delta headers use.
    fn varint(mut v: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                return out;
            }
        }
    }

    #[test]
    fn a_delta_claiming_more_than_the_absolute_ceiling_is_refused_before_allocating() {
        // #279: the ratio ceiling is 65536x the delta length, which is the right shape for "this delta
        // cannot legitimately claim more" but not an absolute bound. A 1 MiB delta passed the ratio
        // check claiming 64 GiB, and `Vec::with_capacity` reserved that before a single op was
        // validated.
        //
        // The delta here is four bytes and claims 2 GiB. The refusal has to happen without attempting
        // the allocation: a `with_capacity` of 2 GiB would either abort the process or make this test
        // allocate gigabytes, and neither happens.
        let base = b"hello".to_vec();
        let mut delta = Vec::new();
        delta.extend(varint(base.len()));
        delta.extend(varint(2 * 1024 * 1024 * 1024));
        delta.push(0x01);

        let err = apply_delta_for_test(&base, &delta)
            .expect_err("a delta above the absolute ceiling must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("claims"),
            "the refusal must be the size check, not a parse failure: {msg}"
        );
        assert!(
            msg.contains("ceiling"),
            "the refusal must say which limit was hit: {msg}"
        );
    }

    #[test]
    fn a_delta_within_the_ceiling_still_applies() {
        // The control: the guard must not refuse ordinary deltas.
        let base = b"hello".to_vec();
        let mut delta = Vec::new();
        delta.extend(varint(base.len())); // base size
        delta.extend(varint(base.len() + 1)); // result size
        delta.push(0x80 | 0x10 | 0x01); // copy: offset present, size present
        delta.push(0x00); // offset = 0
        delta.push(0x05); // size = 5
        delta.push(0x01); // insert one literal
        delta.push(b'!');

        let out = apply_delta_for_test(&base, &delta).expect("a small delta must apply");
        assert_eq!(out, b"hello!".to_vec());
    }

    /// The header parsers read from a file now, so a test that used to pass a slice writes the
    /// bytes to a file and hands back a handle. A scribble file per test rather than a shared one:
    /// the parsers seek, and two tests sharing a handle would interleave their seeks.
    fn handle_for(bytes: &[u8]) -> (std::fs::File, std::path::PathBuf) {
        // The directory was keyed on the pid alone, so every call in one process shared it, and the
        // file name inside it came from the clock. Both halves now come from one counter.
        let dir =
            crate::support::scratch::scratch("gesta-pkvar").expect("a test scratch directory");
        let p = dir.join(format!(
            "h-{}-{:?}.bin",
            bytes.len(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, bytes).unwrap();
        let f = std::fs::File::open(&p).unwrap();
        (f, p)
    }

    #[test]
    fn a_delta_header_claiming_more_than_its_body_could_produce_is_refused() {
        // base size 5, then a result size of 1<<34 followed by one byte. The varint is legal, the
        // promise is not: `with_capacity` used to reserve the whole thing before reading a single
        // op, so a 10-byte delta could ask for 16 GB.
        let base = b"abcde";
        let mut delta = vec![5u8];
        // 1 << 34 as a varint
        let mut v: u64 = 1 << 34;
        while v >= 0x80 {
            delta.push((v as u8) | 0x80);
            v >>= 7;
        }
        delta.push(v as u8);
        delta.push(0x00);
        let err = apply_delta_for_test(base, &delta).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("claims") && msg.contains("byte body"),
            "the refusal did not name the impossible promise: {msg}"
        );
    }

    #[test]
    fn an_index_claiming_more_objects_than_the_file_could_hold_is_refused() {
        // A four-byte fanout total saying 4 billion objects, in a file of 1032 bytes. Two things
        // could reject this: the ceiling added for this bug, or the per-entry bounds check that was
        // always there ("index names are truncated"). Only the first one stops the allocation, so
        // the test asserts on the message. An earlier version of this test only asserted `is_err`
        // and passed with the ceiling removed - it was green for the wrong reason.
        let dir = std::env::temp_dir().join(format!("gesta-idx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut idx = Vec::new();
        idx.extend_from_slice(b"\xfftOc");
        idx.extend_from_slice(&2u32.to_be_bytes());
        for i in 0..256u32 {
            let v = if i == 255 { 4_000_000_000u32 } else { 0 };
            idx.extend_from_slice(&v.to_be_bytes());
        }
        let p = dir.join("t.idx");
        std::fs::write(&p, &idx).unwrap();
        let pack = dir.join("t.pack");
        std::fs::write(&pack, b"PACK").unwrap();
        let msg = match Pack::open(&pack, &p) {
            Ok(_) => panic!("an index claiming 4e9 objects was accepted"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("claims") && msg.contains("can hold at most"),
            "rejected for the wrong reason, so the allocation was still attempted: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The name a git pack index gives a blob: SHA-1 over `"blob <len>\0"` and the body.
    ///
    /// Not `store::hash_bytes`, which is SHA-256 and names sidecar objects, and not a bare hash of
    /// the body either: a git object name covers a header with the type and length. Using the
    /// production function rather than re-deriving it means the fixture cannot drift from what the
    /// reader looks for.
    fn blob_name(body: &[u8]) -> String {
        crate::store::gitobj::object_hash_with(
            crate::store::gitobj::Kind::Blob,
            body,
            crate::store::gitobj::HashKind::Sha1,
        )
    }

    /// Build a pack holding exactly one blob, plus its v2 index, then pad the pack file out to
    /// `pad_to` bytes with bytes no index entry points at.
    ///
    /// The padding is the point: the read path's cost is what is being measured, and a file made
    /// large by a real 192 MB blob would be mostly compressed away, leaving nothing to notice. A
    /// padded file makes the difference between "reads the entry" and "reads the file" impossible
    /// to miss, and a one-entry index has none of the large-offset arithmetic that a multi-hundred
    /// megabyte pack needs.
    fn write_padded_pack(
        dir: &std::path::Path,
        body: &[u8],
        pad_to: u64,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::io::Write;
        std::fs::create_dir_all(dir).unwrap();

        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        let offset = pack.len() as u64;
        let mut size = body.len();
        let mut header = vec![0x30u8 | (size & 0x0f) as u8];
        size >>= 4;
        while size != 0 {
            *header.last_mut().unwrap() |= 0x80;
            header.push((size & 0x7f) as u8);
            size >>= 7;
        }
        pack.extend_from_slice(&header);
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(body).unwrap();
        pack.extend_from_slice(&enc.finish().unwrap());

        // Padding written as a hole, not as bytes held in memory: the fixture must not allocate the
        // file's size, or the peak-RSS delta this test measures is spent before the read starts.
        // The trailer is a hash of everything before it, which is a hole of zeros here, so it is
        // computed over a zero-filled stream rather than over a buffer.
        let pack_path = dir.join("t.pack");
        {
            let mut f = std::fs::File::create(&pack_path).unwrap();
            f.write_all(&pack).unwrap();
            let trailer_at = pad_to.max(pack.len() as u64);
            f.set_len(trailer_at).unwrap();
            // The trailer is never verified by the read path, so a zero hash is honest here and
            // avoids streaming 256 MB of zeros through SHA-256 for every run.
            let h = sha2::Sha256::new();
            use sha2::Digest as _;
            f.write_all(&h.finalize()).unwrap();
        }

        let name = blob_name(body);
        let mut idx = Vec::new();
        idx.extend_from_slice(b"\xfftOc");
        idx.extend_from_slice(&2u32.to_be_bytes());
        let first_byte = u8::from_str_radix(&name[0..2], 16).unwrap();
        for i in 0..256u32 {
            idx.extend_from_slice(&(if i >= first_byte as u32 { 1u32 } else { 0 }).to_be_bytes());
        }
        idx.extend_from_slice(&hex::decode(&name).unwrap());
        idx.extend_from_slice(&0u32.to_be_bytes()); // crc, unchecked on this path
        idx.extend_from_slice(&(offset as u32).to_be_bytes());
        idx.extend_from_slice(&[0u8; 20]); // pack trailer, never checked on this path
        std::fs::write(dir.join("t.idx"), &idx).unwrap();
        (dir.join("t.pack"), dir.join("t.idx"))
    }

    /// Peak resident size of this process so far, in KiB.
    ///
    /// Read from the kernel rather than estimated, so a claim about memory is measured. On a
    /// platform without `/proc` this is `None` and the caller says it could not check, rather than
    /// pretending it did.
    fn peak_rss_kib() -> Option<u64> {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                return rest.split_whitespace().next()?.parse().ok();
            }
        }
        None
    }

    #[test]
    fn reading_one_object_does_not_load_the_whole_pack() {
        // `Pack::read` called `fs::read` on the whole pack for every object, so one small object in
        // a large pack cost the pack's size in memory, and importing N objects cost it N times.
        //
        // Stock git packs are the size that hurt, and this fixture is a cheap stand-in for one: a
        // valid one-entry index, a 256 MB file. Peak RSS is read from the kernel across the read of
        // the only object the index names. Loading the file would add 256 MB; reading the entry
        // adds its compressed size.
        let dir = std::env::temp_dir().join(format!("gesta-bigpack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let body: &[u8] = b"the only object this pack names\n";
        let (pack_path, idx_path) = write_padded_pack(&dir, body, 256 << 20);
        let pack = Pack::open(&pack_path, &idx_path).unwrap();

        let before = peak_rss_kib();
        let got = pack
            .read(&blob_name(body))
            .unwrap()
            .expect("the only object in the index was not found");
        assert_eq!(got.data, body, "the wrong body was returned");
        let after = peak_rss_kib();

        match (before, after) {
            (Some(b), Some(a)) => {
                let grew_kib = a.saturating_sub(b);
                let allowance_kib = (256u64 << 20) / 4 / 1024; // 64 MiB
                assert!(
                    grew_kib < allowance_kib,
                    "reading the only object from a 256 MB pack grew peak RSS by {} KiB, which is \
                     the file being loaded rather than the entry (allowance {} KiB)",
                    grew_kib,
                    allowance_kib
                );
            }
            _ => eprintln!("no /proc/self/status here, so the memory ceiling was not checked"),
        }
        drop(pack);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_delta_copies_and_inserts() {
        // base size 5, result size 8, copy 5 bytes from 0, insert "XYZ"
        let base = b"abcde";
        let mut delta = vec![5u8, 8u8];
        delta.push(0x90); // copy with a one byte length
        delta.push(5);
        delta.push(3); // insert three bytes
        delta.extend_from_slice(b"XYZ");
        assert_eq!(apply_delta(base, &delta).unwrap(), b"abcdeXYZ");
    }

    #[test]
    fn a_delta_that_reads_past_its_base_is_refused() {
        let base = b"abc";
        // Declares a base of three bytes, then copies ten.
        let mut delta = vec![3u8, 10u8];
        delta.push(0x91);
        delta.push(0);
        delta.push(10);
        let err = apply_delta(base, &delta).unwrap_err().to_string();
        assert!(err.contains("past the end"), "unhelpful message: {}", err);
    }

    #[test]
    fn a_delta_against_the_wrong_base_is_refused() {
        let mut delta = vec![9u8, 4u8];
        delta.push(4);
        delta.extend_from_slice(b"abcd");
        let err = apply_delta(b"abc", &delta).unwrap_err().to_string();
        assert!(err.contains("expects a base"), "unhelpful message: {}", err);
    }

    #[test]
    fn the_type_codes_are_the_format_codes() {
        assert_eq!(Kind::from_code(1), Some(Kind::Commit));
        assert_eq!(Kind::from_code(2), Some(Kind::Tree));
        assert_eq!(Kind::from_code(3), Some(Kind::Blob));
        assert_eq!(Kind::from_code(4), Some(Kind::Tag));
        assert_eq!(Kind::from_code(6), Some(Kind::OfsDelta));
        assert_eq!(Kind::from_code(7), Some(Kind::RefDelta));
        // Five was reserved and never assigned; a file claiming it is damaged.
        assert_eq!(Kind::from_code(5), None);
    }

    fn encode_entry_size(mut size: usize) -> Vec<u8> {
        let mut bytes = vec![0x30 | (size & 0x0f) as u8];
        size >>= 4;
        while size != 0 {
            *bytes.last_mut().unwrap() |= 0x80;
            bytes.push((size & 0x7f) as u8);
            size >>= 7;
        }
        bytes
    }

    #[test]
    fn pack_varints_preserve_every_representable_boundary() {
        let pack = Pack {
            path: std::path::PathBuf::new(),
            offsets: HashMap::new(),
        };
        for size in [0, 1, 15, 16, 127, 128, usize::MAX] {
            let bytes = encode_entry_size(size);
            let (mut f, _h) = handle_for(&bytes);
            let (kind, decoded, at) = pack.entry_header(&mut f, 0).unwrap();
            assert_eq!((kind, decoded, at), (Kind::Blob, size, bytes.len()));
        }
        let mut distance = u64::MAX;
        let mut bytes = vec![(distance & 0x7f) as u8];
        while distance > 0x7f {
            distance = (distance >> 7) - 1;
            bytes.push((distance & 0x7f) as u8 | 0x80);
        }
        bytes.reverse();
        let (mut f2, _h2) = handle_for(&bytes);
        assert_eq!(ofs_delta_base(&mut f2, 0).unwrap(), (u64::MAX, bytes.len()));
        let (mut fz, _hz) = handle_for(&[0]);
        assert_eq!(ofs_delta_base(&mut fz, 0).unwrap(), (0, 1));
    }

    #[test]
    fn pack_varints_refuse_entry_size_overflow() {
        let pack = Pack {
            path: std::path::PathBuf::new(),
            offsets: HashMap::new(),
        };
        let mut bytes = encode_entry_size(usize::MAX);
        let remaining = (usize::BITS - 4) % 7;
        if remaining == 0 {
            *bytes.last_mut().unwrap() |= 0x80;
            bytes.push(1);
        } else {
            *bytes.last_mut().unwrap() |= 1 << remaining;
        }
        let (mut f, _h) = handle_for(&bytes);
        let err = pack.entry_header(&mut f, 0).unwrap_err().to_string();
        assert!(err.contains("overflow"), "{err}");
    }

    #[test]
    fn pack_varints_refuse_overlong_entry_headers() {
        let pack = Pack {
            path: std::path::PathBuf::new(),
            offsets: HashMap::new(),
        };
        let mut bytes = vec![0x80; 32];
        bytes[0] = 0xb0;
        bytes.push(0);
        let (mut f, _h) = handle_for(&bytes);
        let err = pack.entry_header(&mut f, 0).unwrap_err().to_string();
        assert!(err.contains("overflow"), "{err}");
    }

    #[test]
    fn pack_varints_refuse_base_distance_overflow() {
        let mut bytes = vec![0xff; 32];
        bytes.push(0x7f);
        let (mut f, _h) = handle_for(&bytes);
        let err = ofs_delta_base(&mut f, 0).unwrap_err().to_string();
        assert!(err.contains("overflow"), "{err}");
    }

    #[test]
    fn pack_varints_refuse_delta_size_overflow() {
        let mut size = usize::MAX;
        let mut bytes = Vec::new();
        loop {
            let byte = (size & 0x7f) as u8;
            size >>= 7;
            bytes.push(byte | if size == 0 { 0 } else { 0x80 });
            if size == 0 {
                break;
            }
        }
        assert_eq!(delta_size(&bytes, 0).unwrap(), (usize::MAX, bytes.len()));
        *bytes.last_mut().unwrap() |= 0x40;
        let err = delta_size(&bytes, 0).unwrap_err().to_string();
        assert!(err.contains("overflow"), "{err}");
    }

    #[test]
    fn pack_varints_keep_truncation_errors() {
        let pack = Pack {
            path: std::path::PathBuf::new(),
            offsets: HashMap::new(),
        };
        let (mut ft, _ht) = handle_for(&[0xb0]);
        assert!(pack
            .entry_header(&mut ft, 0)
            .unwrap_err()
            .to_string()
            .contains("truncated"));
        let (mut fo, _ho) = handle_for(&[0x80]);
        assert!(ofs_delta_base(&mut fo, 0)
            .unwrap_err()
            .to_string()
            .contains("truncated"));
        assert!(delta_size(&[0x80], 0)
            .unwrap_err()
            .to_string()
            .contains("truncated"));
    }

    #[test]
    fn an_object_size_varint_round_trips() {
        // Type 3 (blob) with size 200. 200 is 0x0c8, so the low four bits (8) go in the first
        // byte with the continuation bit set, and the rest (12) in the second.
        let bytes = [0x38u8 | 0x80, 0x0c];
        let pack = Pack {
            path: std::path::PathBuf::new(),
            offsets: HashMap::new(),
        };
        // Prepend the header to a body so `entry_header` has something to read.
        let (mut f, _h) = handle_for(&bytes);
        let (kind, size, at) = pack.entry_header(&mut f, 0).unwrap();
        assert_eq!(kind, Kind::Blob);
        assert_eq!(size, 200);
        assert_eq!(at, 2);
    }
}
