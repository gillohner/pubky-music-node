//! `CastMessage` protobuf, hand-encoded (seven fields, no code generation):
//! 1 protocol_version (enum, 0), 2 source_id, 3 destination_id,
//! 4 namespace, 5 payload_type (0 = STRING, 1 = BINARY), 6 payload_utf8,
//! 7 payload_binary. Frames are a big-endian u32 length then the message.

use crate::CastError;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CastMessage {
    pub source_id: String,
    pub destination_id: String,
    pub namespace: String,
    pub payload_utf8: Option<String>,
    pub payload_binary: Option<Vec<u8>>,
}

impl CastMessage {
    pub fn text(source: &str, destination: &str, namespace: &str, json: &str) -> Self {
        Self {
            source_id: source.into(),
            destination_id: destination.into(),
            namespace: namespace.into(),
            payload_utf8: Some(json.into()),
            payload_binary: None,
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn put_bytes(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    put_varint(out, ((field as u64) << 3) | 2);
    put_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn put_uint(out: &mut Vec<u8>, field: u32, v: u64) {
    put_varint(out, (field as u64) << 3);
    put_varint(out, v);
}

pub fn encode(m: &CastMessage) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + m.payload_utf8.as_ref().map_or(0, String::len));
    put_uint(&mut out, 1, 0);
    put_bytes(&mut out, 2, m.source_id.as_bytes());
    put_bytes(&mut out, 3, m.destination_id.as_bytes());
    put_bytes(&mut out, 4, m.namespace.as_bytes());
    match (&m.payload_utf8, &m.payload_binary) {
        (Some(t), _) => {
            put_uint(&mut out, 5, 0);
            put_bytes(&mut out, 6, t.as_bytes());
        }
        (None, Some(b)) => {
            put_uint(&mut out, 5, 1);
            put_bytes(&mut out, 7, b);
        }
        (None, None) => put_uint(&mut out, 5, 0),
    }
    out
}

fn get_varint(data: &[u8], i: &mut usize) -> Result<u64, CastError> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = *data
            .get(*i)
            .ok_or_else(|| CastError::Protocol("truncated varint".into()))?;
        *i += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            return Err(CastError::Protocol("varint too long".into()));
        }
    }
}

pub fn decode(data: &[u8]) -> Result<CastMessage, CastError> {
    let mut m = CastMessage::default();
    let mut i = 0;
    while i < data.len() {
        let key = get_varint(data, &mut i)?;
        let field = (key >> 3) as u32;
        let wire = key & 7;
        match wire {
            0 => {
                let _ = get_varint(data, &mut i)?;
            }
            2 => {
                let len = get_varint(data, &mut i)? as usize;
                let end = i
                    .checked_add(len)
                    .filter(|e| *e <= data.len())
                    .ok_or_else(|| CastError::Protocol("truncated field".into()))?;
                let bytes = &data[i..end];
                i = end;
                match field {
                    2 => m.source_id = String::from_utf8_lossy(bytes).into_owned(),
                    3 => m.destination_id = String::from_utf8_lossy(bytes).into_owned(),
                    4 => m.namespace = String::from_utf8_lossy(bytes).into_owned(),
                    6 => m.payload_utf8 = Some(String::from_utf8_lossy(bytes).into_owned()),
                    7 => m.payload_binary = Some(bytes.to_vec()),
                    _ => {}
                }
            }
            1 => i += 8,
            5 => i += 4,
            w => return Err(CastError::Protocol(format!("unsupported wire type {w}"))),
        }
    }
    Ok(m)
}

/// Frame for the wire: u32 BE length then the message.
pub fn frame(m: &CastMessage) -> Vec<u8> {
    let body = encode(m);
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let m = CastMessage::text(
            "sender-0",
            "receiver-0",
            "urn:x-cast:com.google.cast.tp.connection",
            r#"{"type":"CONNECT"}"#,
        );
        let bytes = encode(&m);
        assert_eq!(bytes[0..2], [0x08, 0x00], "field 1 varint 0");
        let back = decode(&bytes).unwrap();
        assert_eq!(back, m);
        let f = frame(&m);
        assert_eq!(u32::from_be_bytes([f[0], f[1], f[2], f[3]]) as usize, bytes.len());
        let bin = CastMessage {
            payload_utf8: None,
            payload_binary: Some(vec![1, 2, 3]),
            ..m.clone()
        };
        assert_eq!(decode(&encode(&bin)).unwrap(), bin);
        assert!(decode(&[0x12, 0x10, 1]).is_err());
    }
}
