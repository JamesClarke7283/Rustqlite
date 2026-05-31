//! SQLite "varint" codec (big-endian, 1–9 bytes).
//!
//! A varint is a variable-length encoding of a 64-bit unsigned integer. The first eight
//! bytes use the high bit as a continuation flag and the low seven bits as data, most
//! significant group first. If a ninth byte is needed, it carries a full eight data bits
//! (so the maximum nine bytes hold 8×7 + 8 = 64 bits). See the SQLite file format spec.

/// Read a varint from the front of `buf`. Returns the decoded value and the number of bytes
/// consumed (1–9), or `None` if `buf` ends before the varint is complete.
pub fn read_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    for i in 0..8 {
        let byte = *buf.get(i)?;
        result = (result << 7) | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
    }
    // Ninth byte: all eight bits are data.
    let byte = *buf.get(8)?;
    result = (result << 8) | byte as u64;
    Some((result, 9))
}

/// Read a varint and reinterpret it as a signed 64-bit integer (the bit pattern is the same;
/// SQLite stores rowids and similar signed quantities this way).
pub fn read_varint_i64(buf: &[u8]) -> Option<(i64, usize)> {
    read_varint(buf).map(|(v, n)| (v as i64, n))
}

/// Number of bytes a varint encoding of `value` will occupy (1–9).
pub fn varint_len(value: u64) -> usize {
    // 9 bytes are needed once any of the top eight bits are set (value >= 2^56).
    if value & 0xff00_0000_0000_0000 != 0 {
        return 9;
    }
    let mut len = 1;
    let mut v = value >> 7;
    while v != 0 {
        len += 1;
        v >>= 7;
    }
    len
}

/// Append the varint encoding of `value` to `out`, returning the number of bytes written.
pub fn write_varint(value: u64, out: &mut Vec<u8>) -> usize {
    if value & 0xff00_0000_0000_0000 != 0 {
        // Nine-byte form: eight 7-bit groups (all with the continuation bit set) followed by
        // a final byte carrying the low eight bits.
        let mut bytes = [0u8; 9];
        bytes[8] = (value & 0xff) as u8;
        let mut v = value >> 8;
        for slot in bytes[..8].iter_mut().rev() {
            *slot = (v as u8 & 0x7f) | 0x80;
            v >>= 7;
        }
        out.extend_from_slice(&bytes);
        return 9;
    }

    // 1–8 byte form: emit 7-bit groups, most significant first, continuation bit on all but
    // the last.
    let mut tmp = [0u8; 9];
    let mut n = 0;
    let mut v = value;
    loop {
        tmp[n] = (v as u8 & 0x7f) | 0x80;
        n += 1;
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    tmp[0] &= 0x7f; // the least-significant group becomes the final byte (no continuation)
    for i in (0..n).rev() {
        out.push(tmp[i]);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: u64, expected_len: usize) {
        let mut out = Vec::new();
        let written = write_varint(v, &mut out);
        assert_eq!(written, expected_len, "encoded length for {v:#x}");
        assert_eq!(out.len(), expected_len);
        assert_eq!(varint_len(v), expected_len, "varint_len for {v:#x}");
        let (decoded, read) = read_varint(&out).expect("decodes");
        assert_eq!(decoded, v, "value roundtrip for {v:#x}");
        assert_eq!(read, expected_len, "decoded length for {v:#x}");
    }

    #[test]
    fn boundary_lengths() {
        roundtrip(0, 1);
        roundtrip(1, 1);
        roundtrip(127, 1); // 2^7 - 1
        roundtrip(128, 2); // 2^7
        roundtrip(16_383, 2); // 2^14 - 1
        roundtrip(16_384, 3); // 2^14
        roundtrip((1 << 21) - 1, 3);
        roundtrip(1 << 21, 4);
        roundtrip((1 << 28) - 1, 4);
        roundtrip(1 << 28, 5);
        roundtrip((1 << 56) - 1, 8); // largest 8-byte value
        roundtrip(1 << 56, 9); // smallest 9-byte value
        roundtrip(u64::MAX, 9);
    }

    #[test]
    fn known_encodings() {
        // 300 = 0b1_0010_1100 -> [0x82, 0x2c]
        let mut out = Vec::new();
        write_varint(300, &mut out);
        assert_eq!(out, vec![0x82, 0x2c]);
        assert_eq!(read_varint(&[0x82, 0x2c]), Some((300, 2)));
    }

    #[test]
    fn signed_reinterpretation() {
        // -1 as u64 is all ones -> 9-byte varint -> read back as i64 == -1.
        let mut out = Vec::new();
        write_varint((-1i64) as u64, &mut out);
        assert_eq!(out.len(), 9);
        assert_eq!(read_varint_i64(&out), Some((-1, 9)));
    }

    #[test]
    fn truncated_input_returns_none() {
        // A lone continuation byte cannot be a complete varint.
        assert_eq!(read_varint(&[0x80]), None);
        assert_eq!(read_varint(&[]), None);
    }

    #[test]
    fn extra_trailing_bytes_are_ignored() {
        // read_varint reports only what it consumed.
        let (v, n) = read_varint(&[0x01, 0xff, 0xff]).unwrap();
        assert_eq!((v, n), (1, 1));
    }
}
