// Git delta encoding for the packs this server writes.
//
// The pack writer sent every object compressed on its own, so a file that changed once between two
// commits travelled twice in full. Measured on this project's own history the pack was 6.65x git's,
// which is the number this module exists to reduce.
//
// The format is git's own: a base size, a result size, then copy and insert instructions. A copy is
// an offset and a length read from the base as raw bytes; an insert carries the bytes. Copy is
// preferred because that is the whole point, and a delta that would not be smaller than the object
// itself is not produced at all, so a wrong choice costs bytes rather than correctness.
//
// Every delta this module returns is verified by applying it back to its base before it is offered.
// A delta that produces the wrong bytes makes git reject the whole pack with a message about a bad
// object, which reads as a protocol bug and hides the encoder.

/// Bit flags for the copy instruction, matching git's encoding.
const COPY: u8 = 0x80;

fn push_varint(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Encode `target` against `base`, returning git delta bytes, or `None` when the encoding would not
/// be smaller than the target.
///
/// The search is a plain longest-match over the base, which is what `diff-delta.c` does in essence:
/// for each position in the target, find the longest run that also appears in the base, emit a copy
/// for it, and emit an insert for the bytes in between. It is not the smallest possible delta and it
/// is fast enough for a server writing one pack.
pub fn encode(base: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    if base.is_empty() || target.is_empty() {
        return None;
    }
    // Below this length the header alone is the size of the object, and a delta that saves nothing
    // costs the client a second object to resolve.
    if target.len() < 32 {
        return None;
    }

    // An index of every position by its first four bytes. Without it every scan of the base is a
    // linear search per byte of the target, which is quadratic on a large file.
    let mut index: std::collections::HashMap<[u8; 4], Vec<usize>> =
        std::collections::HashMap::new();
    if base.len() >= 4 {
        for i in 0..=base.len() - 4 {
            let key = [base[i], base[i + 1], base[i + 2], base[i + 3]];
            index.entry(key).or_default().push(i);
        }
    }

    let mut out = Vec::new();
    push_varint(&mut out, base.len());
    push_varint(&mut out, target.len());

    let mut at = 0usize;
    let mut pending: Vec<u8> = Vec::new();
    while at < target.len() {
        let mut best = (0usize, 0usize); // (length, base offset)
        if at + 4 <= target.len() {
            let key = [target[at], target[at + 1], target[at + 2], target[at + 3]];
            if let Some(positions) = index.get(&key) {
                // A cap on how many candidates to try keeps a file of one repeated line from being
                // quadratic, and the first candidates are as good as any.
                for &pos in positions.iter().take(64) {
                    let mut len = 4usize;
                    while pos + len < base.len()
                        && at + len < target.len()
                        && base[pos + len] == target[at + len]
                    {
                        len += 1;
                    }
                    if len > best.0 {
                        best = (len, pos);
                    }
                }
            }
        }

        if best.0 >= 8 {
            // A short copy costs two opcodes plus the offset and length bytes, which is not worth it
            // against an insert that would carry the same bytes once.
            if !pending.is_empty() {
                emit_insert(&mut out, &pending);
                pending.clear();
            }
            emit_copy(&mut out, best.1, best.0);
            at += best.0;
        } else {
            pending.push(target[at]);
            at += 1;
            if pending.len() >= 127 {
                emit_insert(&mut out, &pending);
                pending.clear();
            }
        }
    }
    if !pending.is_empty() {
        emit_insert(&mut out, &pending);
    }

    if out.len() >= target.len() {
        return None;
    }
    // The encoder is verified against its own decoder before it is offered. A delta that does not
    // rebuild the target exactly is dropped rather than sent: git would refuse the pack and the
    // message would point at the protocol, not at this file.
    let rebuilt = crate::store::packfile::apply_delta_for_test(base, &out).ok()?;
    if rebuilt != target {
        return None;
    }
    Some(out)
}

fn emit_insert(out: &mut Vec<u8>, bytes: &[u8]) {
    // An insert opcode is the length in its low seven bits, 1 to 127. Longer runs are split.
    for chunk in bytes.chunks(127) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
}

fn emit_copy(out: &mut Vec<u8>, offset: usize, length: usize) {
    // The opcode says which bytes of the offset and length follow. A zero length field means
    // 0x10000, which is how the format encodes the longest copy.
    let mut remaining = length;
    let mut off = offset;
    loop {
        let this = remaining.min(0x10000);
        let mut op = COPY;
        let mut body: Vec<u8> = Vec::with_capacity(7);
        for i in 0..4 {
            let b = ((off >> (i * 8)) & 0xff) as u8;
            if b != 0 {
                op |= 1 << i;
                body.push(b);
            }
        }
        // A length of 0x10000 is encoded as an absent length.
        let encoded_len = if this == 0x10000 { 0 } else { this };
        for i in 0..3 {
            let b = ((encoded_len >> (i * 8)) & 0xff) as u8;
            if b != 0 {
                op |= 0x10 << i;
                body.push(b);
            }
        }
        out.push(op);
        out.extend_from_slice(&body);
        remaining -= this;
        off += this;
        if remaining == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delta_rebuilds_its_target_exactly() {
        let base = b"the quick brown fox jumps over the lazy dog\n".repeat(20);
        let mut target = base.clone();
        target.extend_from_slice(b"one new line at the end\n");
        let d = encode(&base, &target).expect("a target with an insertion should delta");
        let back = crate::store::packfile::apply_delta_for_test(&base, &d).unwrap();
        assert_eq!(
            back, target,
            "the delta must rebuild the target byte for byte"
        );
    }

    #[test]
    fn a_delta_is_smaller_than_the_object_or_it_is_not_offered() {
        // Unrelated content: no copy can help, so the encoder must refuse rather than produce a
        // delta that is the object plus a header.
        let base = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_vec();
        let target: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
        assert!(
            encode(&base, &target).is_none(),
            "an unrelated target must not be deltaed"
        );
    }

    #[test]
    fn a_small_target_is_not_deltaed() {
        assert!(encode(b"some base content here", b"tiny").is_none());
    }

    #[test]
    fn a_copy_of_a_long_run_uses_the_zero_length_form() {
        // 0x10000 is encoded as an absent length field, so a full copy of 65536 bytes has an opcode
        // with no length bytes at all.
        let base: Vec<u8> = std::iter::repeat_n(b'x', 70000).collect();
        let d = encode(&base, &base).expect("a full copy of a long run should delta");
        let back = crate::store::packfile::apply_delta_for_test(&base, &d).unwrap();
        assert_eq!(
            back.len(),
            base.len(),
            "the copy must rebuild the whole base"
        );
        let mut out = Vec::new();
        emit_copy(&mut out, 0, 65536);
        assert_eq!(out[0] & 0x80, 0x80, "a copy opcode has the high bit set");
        assert_eq!(
            out.len(),
            1,
            "a full copy needs no offset or length bytes: {:?}",
            out
        );
    }
}
