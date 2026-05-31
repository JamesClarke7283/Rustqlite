//! Record serial types (<https://www.sqlite.org/fileformat2.html#record_format>).
//!
//! Every value inside a record is described by a "serial type" varint that gives both the
//! storage class and the byte length:
//!
//! | code | meaning |
//! |------|---------|
//! | 0 | NULL (0 bytes) |
//! | 1..=6 | big-endian twos-complement int of 1,2,3,4,6,8 bytes |
//! | 7 | IEEE-754 64-bit big-endian float (8 bytes) |
//! | 8 / 9 | integer constant 0 / 1 (0 bytes; schema format ≥ 4) |
//! | 10 / 11 | reserved for internal use |
//! | N≥12 even | BLOB of (N-12)/2 bytes |
//! | N≥13 odd | TEXT of (N-13)/2 bytes (in the database text encoding) |

use crate::error::{Error, Result};
use crate::types::Value;

use super::header::TextEncoding;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerialType {
    Null,
    I8,
    I16,
    I24,
    I32,
    I48,
    I64,
    F64,
    Const0,
    Const1,
    Reserved(u64),
    Blob(usize),
    Text(usize),
}

impl SerialType {
    /// Decode a serial-type code into a [`SerialType`].
    pub fn from_code(code: u64) -> SerialType {
        match code {
            0 => SerialType::Null,
            1 => SerialType::I8,
            2 => SerialType::I16,
            3 => SerialType::I24,
            4 => SerialType::I32,
            5 => SerialType::I48,
            6 => SerialType::I64,
            7 => SerialType::F64,
            8 => SerialType::Const0,
            9 => SerialType::Const1,
            10 | 11 => SerialType::Reserved(code),
            n if n % 2 == 0 => SerialType::Blob(((n - 12) / 2) as usize),
            n => SerialType::Text(((n - 13) / 2) as usize),
        }
    }

    /// The serial-type code for this type (the inverse of [`from_code`](Self::from_code)).
    pub fn code(self) -> u64 {
        match self {
            SerialType::Null => 0,
            SerialType::I8 => 1,
            SerialType::I16 => 2,
            SerialType::I24 => 3,
            SerialType::I32 => 4,
            SerialType::I48 => 5,
            SerialType::I64 => 6,
            SerialType::F64 => 7,
            SerialType::Const0 => 8,
            SerialType::Const1 => 9,
            SerialType::Reserved(n) => n,
            SerialType::Blob(len) => 12 + (len as u64) * 2,
            SerialType::Text(len) => 13 + (len as u64) * 2,
        }
    }

    /// Number of bytes this value occupies in the record body.
    pub fn byte_len(self) -> usize {
        match self {
            SerialType::Null | SerialType::Const0 | SerialType::Const1 => 0,
            SerialType::Reserved(_) => 0,
            SerialType::I8 => 1,
            SerialType::I16 => 2,
            SerialType::I24 => 3,
            SerialType::I32 => 4,
            SerialType::I48 => 6,
            SerialType::I64 | SerialType::F64 => 8,
            SerialType::Blob(len) | SerialType::Text(len) => len,
        }
    }

    /// Decode this serial type's value from exactly `bytes` (which must be `byte_len()` long),
    /// interpreting TEXT in the given database `encoding`.
    pub fn decode(self, bytes: &[u8], encoding: TextEncoding) -> Result<Value> {
        debug_assert_eq!(bytes.len(), self.byte_len());
        Ok(match self {
            SerialType::Null => Value::Null,
            SerialType::Const0 => Value::Int(0),
            SerialType::Const1 => Value::Int(1),
            SerialType::I8
            | SerialType::I16
            | SerialType::I24
            | SerialType::I32
            | SerialType::I48
            | SerialType::I64 => Value::Int(decode_be_int(bytes)),
            SerialType::F64 => {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(bytes);
                Value::Real(f64::from_be_bytes(buf))
            }
            SerialType::Blob(_) => Value::Blob(bytes.to_vec()),
            SerialType::Text(_) => Value::Text(decode_text(bytes, encoding)),
            SerialType::Reserved(n) => {
                return Err(Error::corrupt(format!("reserved serial type {n}")))
            }
        })
    }

    /// Choose the serial type and body bytes that encode `value`, picking the minimal integer
    /// width (and the 0/1 constant types) exactly as SQLite does. TEXT is encoded as UTF-8;
    /// other encodings are produced by the write path once it lands.
    pub fn encode(value: &Value) -> (SerialType, Vec<u8>) {
        match value {
            Value::Null => (SerialType::Null, Vec::new()),
            Value::Int(i) => encode_int(*i),
            Value::Real(r) => (SerialType::F64, r.to_be_bytes().to_vec()),
            Value::Text(s) => (SerialType::Text(s.len()), s.as_bytes().to_vec()),
            Value::Blob(b) => (SerialType::Blob(b.len()), b.clone()),
        }
    }
}

/// Decode an `N`-byte big-endian two's-complement integer, sign-extending to `i64`.
fn decode_be_int(bytes: &[u8]) -> i64 {
    let mut v: i64 = if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        -1 // start with all ones so the high bits sign-extend
    } else {
        0
    };
    for &b in bytes {
        v = (v << 8) | b as i64;
    }
    v
}

fn encode_int(i: i64) -> (SerialType, Vec<u8>) {
    match i {
        0 => (SerialType::Const0, Vec::new()),
        1 => (SerialType::Const1, Vec::new()),
        _ if (-0x80..=0x7f).contains(&i) => (SerialType::I8, vec![i as u8]),
        _ if (-0x8000..=0x7fff).contains(&i) => {
            (SerialType::I16, (i as i16).to_be_bytes().to_vec())
        }
        _ if (-0x80_0000..=0x7f_ffff).contains(&i) => {
            (SerialType::I24, (i as i32).to_be_bytes()[1..4].to_vec())
        }
        _ if (-0x8000_0000..=0x7fff_ffff).contains(&i) => {
            (SerialType::I32, (i as i32).to_be_bytes().to_vec())
        }
        _ if (-0x8000_0000_0000..=0x7fff_ffff_ffff).contains(&i) => {
            (SerialType::I48, i.to_be_bytes()[2..8].to_vec())
        }
        _ => (SerialType::I64, i.to_be_bytes().to_vec()),
    }
}

fn decode_text(bytes: &[u8], encoding: TextEncoding) -> String {
    match encoding {
        TextEncoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| match encoding {
                    TextEncoding::Utf16Be => u16::from_be_bytes([pair[0], pair[1]]),
                    _ => u16::from_le_bytes([pair[0], pair[1]]),
                })
                .collect();
            String::from_utf16_lossy(&units)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_roundtrip() {
        for code in 0u64..40 {
            let st = SerialType::from_code(code);
            if let SerialType::Reserved(_) = st {
                continue;
            }
            assert_eq!(st.code(), code, "code {code}");
        }
    }

    #[test]
    fn integer_widths_match_sqlite() {
        assert_eq!(SerialType::encode(&Value::Int(0)).0, SerialType::Const0);
        assert_eq!(SerialType::encode(&Value::Int(1)).0, SerialType::Const1);
        assert_eq!(SerialType::encode(&Value::Int(2)).0, SerialType::I8);
        assert_eq!(SerialType::encode(&Value::Int(127)).0, SerialType::I8);
        assert_eq!(SerialType::encode(&Value::Int(128)).0, SerialType::I16);
        assert_eq!(SerialType::encode(&Value::Int(-129)).0, SerialType::I16);
        assert_eq!(SerialType::encode(&Value::Int(32_768)).0, SerialType::I24);
        assert_eq!(
            SerialType::encode(&Value::Int(8_388_608)).0,
            SerialType::I32
        );
        assert_eq!(
            SerialType::encode(&Value::Int(2_147_483_648)).0,
            SerialType::I48
        );
        assert_eq!(SerialType::encode(&Value::Int(i64::MAX)).0, SerialType::I64);
        assert_eq!(SerialType::encode(&Value::Int(i64::MIN)).0, SerialType::I64);
    }

    #[test]
    fn int_value_roundtrip_all_widths() {
        for v in [
            0i64,
            1,
            -1,
            2,
            127,
            -128,
            128,
            -129,
            32_767,
            -32_768,
            8_388_607,
            -8_388_608,
            2_147_483_647,
            -2_147_483_648,
            140_737_488_355_327,
            -140_737_488_355_328,
            i64::MAX,
            i64::MIN,
        ] {
            let (st, body) = SerialType::encode(&Value::Int(v));
            assert_eq!(body.len(), st.byte_len(), "len for {v}");
            let decoded = st.decode(&body, TextEncoding::Utf8).unwrap();
            assert_eq!(decoded, Value::Int(v), "roundtrip {v}");
        }
    }

    #[test]
    fn float_roundtrip() {
        let (st, body) = SerialType::encode(&Value::Real(3.5));
        assert_eq!(st, SerialType::F64);
        assert_eq!(
            st.decode(&body, TextEncoding::Utf8).unwrap(),
            Value::Real(3.5)
        );
    }

    #[test]
    fn text_and_blob() {
        let (st, body) = SerialType::encode(&Value::Text("hi".into()));
        assert_eq!(st, SerialType::Text(2));
        assert_eq!(st.code(), 13 + 2 * 2);
        assert_eq!(
            st.decode(&body, TextEncoding::Utf8).unwrap(),
            Value::Text("hi".into())
        );

        let (st, body) = SerialType::encode(&Value::Blob(vec![1, 2, 3]));
        assert_eq!(st, SerialType::Blob(3));
        assert_eq!(st.code(), 12 + 3 * 2);
        assert_eq!(
            st.decode(&body, TextEncoding::Utf8).unwrap(),
            Value::Blob(vec![1, 2, 3])
        );
    }

    #[test]
    fn utf16_text_decode() {
        // "Hi" in UTF-16LE: 0x48 0x00 0x69 0x00
        let st = SerialType::Text(4);
        assert_eq!(
            st.decode(&[0x48, 0x00, 0x69, 0x00], TextEncoding::Utf16Le)
                .unwrap(),
            Value::Text("Hi".into())
        );
    }
}
