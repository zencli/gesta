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
    pub fn read(&self, hash: &str) -> ZResult<Option<Packed>> {
        let Some(&offset) = self.offsets.get(hash) else {
            return Ok(None);
        };
        let bytes = std::fs::read(&self.path)?;
        let depth = self.resolve(&bytes, offset, 0)?;
        Ok(Some(depth))
    }

    /// Resolve the entry at `offset` into a whole object.
    ///
    /// `depth` guards against a chain that loops or is absurdly long. A delta chain in a real
    /// pack is short because a repack rewrites it, but a corrupt file is not obliged to agree.
    fn resolve(&self, bytes: &[u8], offset: u64, depth: usize) -> ZResult<Packed> {
        if depth > 128 {
            return Err(bad(format!(
                "delta chain at offset {} is deeper than any real pack: the file may be damaged",
                offset
            )));
        }
        let (kind, size, mut at) = self.entry_header(bytes, offset)?;
        match kind {
            Kind::Commit | Kind::Tree | Kind::Blob | Kind::Tag => {
                let body = inflate_at(bytes, at)?;
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
                let (base_offset, next) = ofs_delta_base(bytes, at)?;
                at = next;
                let delta = inflate_at(bytes, at)?;
                let base_offset = offset
                    .checked_sub(base_offset)
                    .ok_or_else(|| bad("delta points before the start of the pack"))?;
                let base = self.resolve(bytes, base_offset, depth + 1)?;
                let data = apply_delta(&base.data, &delta)?;
                Ok(Packed {
                    kind: base.kind,
                    data,
                })
            }
            Kind::RefDelta => {
                let name = bytes
                    .get(at..at + 20)
                    .ok_or_else(|| bad("delta base name is truncated"))?
                    .to_vec();
                at += 20;
                let delta = inflate_at(bytes, at)?;
                let base_hash = hex::encode(&name);
                // A base can be in this pack or, after a fetch, in another one. The caller has a
                // list of packs, but a REF_DELTA inside a pack is nearly always against a base in
                // the same file, so try that first and fall back to the named hash.
                if let Some(&base_offset) = self.offsets.get(&base_hash) {
                    let base = self.resolve(bytes, base_offset, depth + 1)?;
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
    fn entry_header(&self, bytes: &[u8], offset: u64) -> ZResult<(Kind, usize, usize)> {
        let mut at = offset as usize;
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
            size |= ((byte & 0x7f) as usize) << shift;
            shift += 7;
        }
        Ok((kind, size, at))
    }
}

/// The base distance of an `OFS_DELTA`, which is a varint with a different continuation rule.
fn ofs_delta_base(bytes: &[u8], mut at: usize) -> ZResult<(u64, usize)> {
    let mut byte = *bytes
        .get(at)
        .ok_or_else(|| bad("delta base offset is truncated"))?;
    at += 1;
    let mut value = (byte & 0x7f) as u64;
    while byte & 0x80 != 0 {
        byte = *bytes
            .get(at)
            .ok_or_else(|| bad("delta base offset is truncated"))?;
        at += 1;
        value = ((value + 1) << 7) | (byte & 0x7f) as u64;
    }
    Ok((value, at))
}

/// Inflate one zlib stream starting at `at`.
fn inflate_at(bytes: &[u8], at: usize) -> ZResult<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    let mut out = Vec::new();
    ZlibDecoder::new(&bytes[at..])
        .read_to_end(&mut out)
        .map_err(|e| bad(format!("pack entry does not decompress: {}", e)))?;
    Ok(out)
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

    let mut out = Vec::with_capacity(result_size);
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
        value |= ((byte & 0x7f) as usize) << shift;
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
        let (kind, size, at) = pack.entry_header(&bytes, 0).unwrap();
        assert_eq!(kind, Kind::Blob);
        assert_eq!(size, 200);
        assert_eq!(at, 2);
    }
}
