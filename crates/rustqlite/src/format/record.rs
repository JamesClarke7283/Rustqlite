//! The record format (<https://www.sqlite.org/fileformat2.html#record_format>).
//!
//! A record (the payload of a table-leaf or index cell) is a header followed by a body. The
//! header starts with a varint giving the total header length in bytes (including that
//! varint), followed by one serial-type varint per value. The body holds the values back to
//! back, in the same order, each laid out per its serial type.

use crate::error::{Error, Result};
use crate::types::Value;

use super::header::TextEncoding;
use super::serial_type::SerialType;
use super::varint::{read_varint, varint_len, write_varint};

/// Decode a record payload into its sequence of values.
pub fn decode_record(payload: &[u8], encoding: TextEncoding) -> Result<Vec<Value>> {
    let (header_len, n0) =
        read_varint(payload).ok_or_else(|| Error::corrupt("record header length varint"))?;
    let header_len = header_len as usize;
    if header_len > payload.len() || header_len < n0 {
        return Err(Error::corrupt("record header length out of range"));
    }

    let mut header_pos = n0;
    let mut body_pos = header_len;
    let mut values = Vec::new();
    while header_pos < header_len {
        let (code, k) = read_varint(&payload[header_pos..])
            .ok_or_else(|| Error::corrupt("record serial-type varint"))?;
        header_pos += k;
        let st = SerialType::from_code(code);
        let len = st.byte_len();
        let end = body_pos
            .checked_add(len)
            .ok_or_else(|| Error::corrupt("record body overflow"))?;
        if end > payload.len() {
            return Err(Error::corrupt(
                "record body shorter than serial types imply",
            ));
        }
        values.push(st.decode(&payload[body_pos..end], encoding)?);
        body_pos = end;
    }
    Ok(values)
}

/// Encode a sequence of values into a record payload, choosing minimal serial types (matching
/// SQLite's canonical encoding for schema format 4: 0/1 use the const types, integers use the
/// smallest width). TEXT is encoded as UTF-8.
pub fn encode_record(values: &[Value]) -> Vec<u8> {
    let mut serials = Vec::with_capacity(values.len());
    let mut bodies = Vec::with_capacity(values.len());
    let mut serial_bytes = 0usize;
    for value in values {
        let (st, body) = SerialType::encode(value);
        serial_bytes += varint_len(st.code());
        serials.push(st);
        bodies.push(body);
    }

    // The header length includes the varint that encodes it, which is mildly self-referential;
    // grow the header-length-varint width until it is consistent.
    let mut hv = 1;
    while varint_len((serial_bytes + hv) as u64) > hv {
        hv += 1;
    }
    let header_len = serial_bytes + hv;

    let mut out = Vec::with_capacity(header_len + bodies.iter().map(Vec::len).sum::<usize>());
    write_varint(header_len as u64, &mut out);
    for st in &serials {
        write_varint(st.code(), &mut out);
    }
    for body in &bodies {
        out.extend_from_slice(body);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_mixed_record() {
        let values = vec![
            Value::Null,
            Value::Int(0),
            Value::Int(1),
            Value::Int(42),
            Value::Int(-1),
            Value::Int(100_000),
            Value::Real(2.5),
            Value::Text("hello".into()),
            Value::Blob(vec![0xde, 0xad, 0xbe, 0xef]),
        ];
        let encoded = encode_record(&values);
        let decoded = decode_record(&encoded, TextEncoding::Utf8).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn empty_record() {
        let encoded = encode_record(&[]);
        // Just the header-length varint (value 1).
        assert_eq!(encoded, vec![0x01]);
        assert_eq!(decode_record(&encoded, TextEncoding::Utf8).unwrap(), vec![]);
    }

    #[test]
    fn known_small_record_layout() {
        // A record holding the single integer 1 is: header [0x02, 0x09] (header len 2, serial
        // type 9 = const 1), body empty.
        let encoded = encode_record(&[Value::Int(1)]);
        assert_eq!(encoded, vec![0x02, 0x09]);

        // The integer 7: header [0x02, 0x01] (serial type 1 = i8), body [0x07].
        let encoded = encode_record(&[Value::Int(7)]);
        assert_eq!(encoded, vec![0x02, 0x01, 0x07]);

        // Text "abc": serial type = 13 + 3*2 = 19 (0x13); header [0x02, 0x13], body "abc".
        let encoded = encode_record(&[Value::Text("abc".into())]);
        assert_eq!(encoded, vec![0x02, 0x13, b'a', b'b', b'c']);
    }

    #[test]
    fn large_header_grows_length_varint() {
        // 64 text columns of 1 byte each => 64 serial-type varints (each value 15 = 1 byte) +
        // header-length varint. serial_bytes = 64, header_len = 65 (still 1-byte varint).
        let values: Vec<Value> = (0..64).map(|_| Value::Text("x".into())).collect();
        let encoded = encode_record(&values);
        assert_eq!(decode_record(&encoded, TextEncoding::Utf8).unwrap(), values);
        assert_eq!(encoded[0], 65);

        // 200 such columns => serial_bytes = 200, needs a 2-byte header-length varint.
        let values: Vec<Value> = (0..200).map(|_| Value::Text("x".into())).collect();
        let encoded = encode_record(&values);
        let (hdr_len, n) = read_varint(&encoded).unwrap();
        assert_eq!(n, 2);
        assert_eq!(hdr_len as usize, 200 + 2);
        assert_eq!(decode_record(&encoded, TextEncoding::Utf8).unwrap(), values);
    }

    #[test]
    fn corrupt_records_error() {
        // Header length claims more bytes than present.
        assert!(decode_record(&[0x09], TextEncoding::Utf8).is_err());
        // Serial type implies a body longer than the payload.
        assert!(decode_record(&[0x02, 0x01], TextEncoding::Utf8).is_err());
    }
}
