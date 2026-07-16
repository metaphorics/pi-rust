//! Hand-rolled AWS `application/vnd.amazon.eventstream` frame codec.
//!
//! Spec: total_len(4) + headers_len(4) + prelude_crc(4) + headers + payload + message_crc(4).
//! CRCs are CRC-32/ISO-HDLC (IEEE), big-endian. Prefer this over smithy crates for footprint.

use crc32fast::Hasher;

const PRELUDE_LEN: usize = 12;
const MESSAGE_CRC_LEN: usize = 4;
const MIN_FRAME_LEN: usize = PRELUDE_LEN + MESSAGE_CRC_LEN;

/// One decoded eventstream message (headers + payload).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventStreamMessage {
    pub headers: Vec<(String, EventStreamHeaderValue)>,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventStreamHeaderValue {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Integer(i32),
    Long(i64),
    Bytes(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}

impl EventStreamMessage {
    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|(key, value)| {
            if key.eq_ignore_ascii_case(name) {
                match value {
                    EventStreamHeaderValue::String(text) => Some(text.as_str()),
                    _ => None,
                }
            } else {
                None
            }
        })
    }

    /// Map a Bedrock eventstream frame into the JSON object shape the incremental
    /// decoder expects: `{ "<event-type>": <payload> }` or exception error objects.
    pub fn bedrock_event_json(&self) -> Result<String, String> {
        let message_type = self.header_str(":message-type").unwrap_or("event");
        if message_type.eq_ignore_ascii_case("exception")
            || message_type.eq_ignore_ascii_case("error")
        {
            let exception = self
                .header_str(":exception-type")
                .or_else(|| self.header_str(":error-code"))
                .unwrap_or("modelStreamErrorException");
            let payload = if self.payload.is_empty() {
                serde_json::json!({"message": exception})
            } else {
                serde_json::from_slice(&self.payload).unwrap_or_else(
                    |_| serde_json::json!({"message": String::from_utf8_lossy(&self.payload)}),
                )
            };
            return Ok(serde_json::json!({ exception: payload }).to_string());
        }

        let event_type = self
            .header_str(":event-type")
            .ok_or_else(|| "eventstream frame missing :event-type".to_owned())?;
        let payload = if self.payload.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_slice::<serde_json::Value>(&self.payload)
                .map_err(|error| format!("invalid eventstream JSON payload: {error}"))?
        };
        Ok(serde_json::json!({ event_type: payload }).to_string())
    }
}

/// Incremental decoder that reassembles frames split across arbitrary TCP chunks.
#[derive(Default)]
pub struct EventStreamDecoder {
    pending: Vec<u8>,
}

impl EventStreamDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<EventStreamMessage>, String> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some((message, consumed)) = try_decode_one(&self.pending)? {
            self.pending.drain(..consumed);
            out.push(message);
        }
        Ok(out)
    }

    pub fn finish(self) -> Result<Vec<EventStreamMessage>, String> {
        if self.pending.is_empty() {
            Ok(Vec::new())
        } else {
            Err(format!(
                "truncated eventstream buffer ({} trailing bytes)",
                self.pending.len()
            ))
        }
    }
}

fn try_decode_one(buf: &[u8]) -> Result<Option<(EventStreamMessage, usize)>, String> {
    if buf.len() < PRELUDE_LEN {
        return Ok(None);
    }
    let total_len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
    let headers_len = u32::from_be_bytes(buf[4..8].try_into().unwrap()) as usize;
    if total_len < MIN_FRAME_LEN {
        return Err(format!("eventstream total_length too small: {total_len}"));
    }
    if headers_len > total_len - MIN_FRAME_LEN {
        return Err(format!(
            "eventstream headers_length {headers_len} exceeds frame {total_len}"
        ));
    }
    if buf.len() < total_len {
        return Ok(None);
    }

    let prelude_crc = u32::from_be_bytes(buf[8..12].try_into().unwrap());
    let computed_prelude = crc32(&buf[0..8]);
    if prelude_crc != computed_prelude {
        return Err(format!(
            "eventstream prelude CRC mismatch: expected {computed_prelude:#010x}, got {prelude_crc:#010x}"
        ));
    }

    let headers_end = PRELUDE_LEN + headers_len;
    let payload_end = total_len - MESSAGE_CRC_LEN;
    if headers_end > payload_end {
        return Err("eventstream headers overrun payload".into());
    }
    let message_crc = u32::from_be_bytes(buf[payload_end..total_len].try_into().unwrap());
    let computed_message = crc32(&buf[0..payload_end]);
    if message_crc != computed_message {
        return Err(format!(
            "eventstream message CRC mismatch: expected {computed_message:#010x}, got {message_crc:#010x}"
        ));
    }

    let headers = decode_headers(&buf[PRELUDE_LEN..headers_end])?;
    let payload = buf[headers_end..payload_end].to_vec();
    Ok(Some((EventStreamMessage { headers, payload }, total_len)))
}

fn decode_headers(mut bytes: &[u8]) -> Result<Vec<(String, EventStreamHeaderValue)>, String> {
    let mut headers = Vec::new();
    while !bytes.is_empty() {
        let name_len = bytes[0] as usize;
        bytes = &bytes[1..];
        if bytes.len() < name_len + 1 {
            return Err("truncated eventstream header name".into());
        }
        let name = String::from_utf8(bytes[..name_len].to_vec())
            .map_err(|error| format!("invalid header name: {error}"))?;
        bytes = &bytes[name_len..];
        let header_type = bytes[0];
        bytes = &bytes[1..];
        let (value, rest) = decode_header_value(header_type, bytes)?;
        bytes = rest;
        headers.push((name, value));
    }
    Ok(headers)
}

fn decode_header_value(
    header_type: u8,
    bytes: &[u8],
) -> Result<(EventStreamHeaderValue, &[u8]), String> {
    match header_type {
        0 => Ok((EventStreamHeaderValue::Bool(true), bytes)),
        1 => Ok((EventStreamHeaderValue::Bool(false), bytes)),
        2 => {
            if bytes.is_empty() {
                return Err("truncated byte header".into());
            }
            Ok((EventStreamHeaderValue::Byte(bytes[0] as i8), &bytes[1..]))
        }
        3 => {
            if bytes.len() < 2 {
                return Err("truncated short header".into());
            }
            let value = i16::from_be_bytes(bytes[0..2].try_into().unwrap());
            Ok((EventStreamHeaderValue::Short(value), &bytes[2..]))
        }
        4 => {
            if bytes.len() < 4 {
                return Err("truncated integer header".into());
            }
            let value = i32::from_be_bytes(bytes[0..4].try_into().unwrap());
            Ok((EventStreamHeaderValue::Integer(value), &bytes[4..]))
        }
        5 => {
            if bytes.len() < 8 {
                return Err("truncated long header".into());
            }
            let value = i64::from_be_bytes(bytes[0..8].try_into().unwrap());
            Ok((EventStreamHeaderValue::Long(value), &bytes[8..]))
        }
        6 => {
            if bytes.len() < 2 {
                return Err("truncated bytes header length".into());
            }
            let len = u16::from_be_bytes(bytes[0..2].try_into().unwrap()) as usize;
            if bytes.len() < 2 + len {
                return Err("truncated bytes header value".into());
            }
            Ok((
                EventStreamHeaderValue::Bytes(bytes[2..2 + len].to_vec()),
                &bytes[2 + len..],
            ))
        }
        7 => {
            if bytes.len() < 2 {
                return Err("truncated string header length".into());
            }
            let len = u16::from_be_bytes(bytes[0..2].try_into().unwrap()) as usize;
            if bytes.len() < 2 + len {
                return Err("truncated string header value".into());
            }
            let text = String::from_utf8(bytes[2..2 + len].to_vec())
                .map_err(|error| format!("invalid string header: {error}"))?;
            Ok((EventStreamHeaderValue::String(text), &bytes[2 + len..]))
        }
        8 => {
            if bytes.len() < 8 {
                return Err("truncated timestamp header".into());
            }
            let value = i64::from_be_bytes(bytes[0..8].try_into().unwrap());
            Ok((EventStreamHeaderValue::Timestamp(value), &bytes[8..]))
        }
        9 => {
            if bytes.len() < 16 {
                return Err("truncated uuid header".into());
            }
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&bytes[0..16]);
            Ok((EventStreamHeaderValue::Uuid(uuid), &bytes[16..]))
        }
        other => Err(format!("unknown eventstream header type {other}")),
    }
}

/// Encode a message for tests / fixtures.
pub fn encode_message(
    headers: &[(&str, EventStreamHeaderValue)],
    payload: &[u8],
) -> Result<Vec<u8>, String> {
    let mut header_bytes = Vec::new();
    for (name, value) in headers {
        if name.len() > 255 {
            return Err("header name too long".into());
        }
        header_bytes.push(name.len() as u8);
        header_bytes.extend_from_slice(name.as_bytes());
        encode_header_value(&mut header_bytes, value)?;
    }

    let total_len = PRELUDE_LEN + header_bytes.len() + payload.len() + MESSAGE_CRC_LEN;
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&(total_len as u32).to_be_bytes());
    out.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    let prelude_crc = crc32(&out[0..8]);
    out.extend_from_slice(&prelude_crc.to_be_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(payload);
    let message_crc = crc32(&out);
    out.extend_from_slice(&message_crc.to_be_bytes());
    Ok(out)
}

fn encode_header_value(out: &mut Vec<u8>, value: &EventStreamHeaderValue) -> Result<(), String> {
    match value {
        EventStreamHeaderValue::Bool(true) => out.push(0),
        EventStreamHeaderValue::Bool(false) => out.push(1),
        EventStreamHeaderValue::Byte(v) => {
            out.push(2);
            out.push(*v as u8);
        }
        EventStreamHeaderValue::Short(v) => {
            out.push(3);
            out.extend_from_slice(&v.to_be_bytes());
        }
        EventStreamHeaderValue::Integer(v) => {
            out.push(4);
            out.extend_from_slice(&v.to_be_bytes());
        }
        EventStreamHeaderValue::Long(v) => {
            out.push(5);
            out.extend_from_slice(&v.to_be_bytes());
        }
        EventStreamHeaderValue::Bytes(bytes) => {
            if bytes.len() > u16::MAX as usize {
                return Err("bytes header too long".into());
            }
            out.push(6);
            out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        EventStreamHeaderValue::String(text) => {
            if text.len() > u16::MAX as usize {
                return Err("string header too long".into());
            }
            out.push(7);
            out.extend_from_slice(&(text.len() as u16).to_be_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        EventStreamHeaderValue::Timestamp(v) => {
            out.push(8);
            out.extend_from_slice(&v.to_be_bytes());
        }
        EventStreamHeaderValue::Uuid(uuid) => {
            out.push(9);
            out.extend_from_slice(uuid);
        }
    }
    Ok(())
}

/// Encode a Bedrock converse-stream event (`:event-type` + JSON payload).
pub fn encode_bedrock_event(event_type: &str, payload_json: &str) -> Result<Vec<u8>, String> {
    encode_message(
        &[
            (
                ":message-type",
                EventStreamHeaderValue::String("event".into()),
            ),
            (
                ":event-type",
                EventStreamHeaderValue::String(event_type.into()),
            ),
            (
                ":content-type",
                EventStreamHeaderValue::String("application/json".into()),
            ),
        ],
        payload_json.as_bytes(),
    )
}

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame() -> Vec<u8> {
        encode_bedrock_event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"Hi"}}"#,
        )
        .unwrap()
    }

    #[test]
    fn roundtrip_encode_decode() {
        let frame = sample_frame();
        let mut decoder = EventStreamDecoder::default();
        let messages = decoder.push(&frame).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].header_str(":event-type"),
            Some("contentBlockDelta")
        );
        let json = messages[0].bedrock_event_json().unwrap();
        assert!(json.contains("contentBlockDelta"));
        assert!(json.contains("\"Hi\""));
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn reassembles_frames_split_mid_byte() {
        let frame = sample_frame();
        let mut decoder = EventStreamDecoder::default();
        assert!(decoder.push(&frame[..1]).unwrap().is_empty());
        assert!(decoder.push(&frame[1..7]).unwrap().is_empty());
        assert!(decoder.push(&frame[7..frame.len() - 3]).unwrap().is_empty());
        let messages = decoder.push(&frame[frame.len() - 3..]).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].header_str(":event-type"),
            Some("contentBlockDelta")
        );
    }

    #[test]
    fn rejects_prelude_crc_failure() {
        let mut frame = sample_frame();
        frame[8] ^= 0xff;
        let mut decoder = EventStreamDecoder::default();
        let err = decoder.push(&frame).unwrap_err();
        assert!(err.contains("prelude CRC"), "{err}");
    }

    #[test]
    fn rejects_message_crc_failure() {
        let mut frame = sample_frame();
        let last = frame.len() - 1;
        frame[last] ^= 0xff;
        let mut decoder = EventStreamDecoder::default();
        let err = decoder.push(&frame).unwrap_err();
        assert!(err.contains("message CRC"), "{err}");
    }

    #[test]
    fn rejects_truncated_finish() {
        let frame = sample_frame();
        let mut decoder = EventStreamDecoder::default();
        decoder.push(&frame[..frame.len() - 1]).unwrap();
        let err = decoder.finish().unwrap_err();
        assert!(err.contains("truncated"), "{err}");
    }

    #[test]
    fn packs_multiple_frames_in_one_chunk() {
        let a = encode_bedrock_event("messageStart", r#"{"role":"assistant"}"#).unwrap();
        let b = encode_bedrock_event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"x"}}"#,
        )
        .unwrap();
        let mut chunk = a;
        chunk.extend_from_slice(&b);
        let mut decoder = EventStreamDecoder::default();
        let messages = decoder.push(&chunk).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].header_str(":event-type"), Some("messageStart"));
        assert_eq!(
            messages[1].header_str(":event-type"),
            Some("contentBlockDelta")
        );
    }
}
