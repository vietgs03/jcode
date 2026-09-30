//! Decoder for the AWS event stream binary framing.
//!
//! Every message on the wire is laid out as:
//!
//! ```text
//! [total_len: u32][headers_len: u32][prelude_crc: u32][headers][payload][message_crc: u32]
//! ```
//!
//! All integers are big-endian and both checksums are CRC-32 (IEEE). Headers
//! carry the routing metadata (`:message-type`, `:event-type`,
//! `:exception-type`, ...) while the payload holds the event JSON.

const PRELUDE_LEN: usize = 12;
const MESSAGE_CRC_LEN: usize = 4;
const MIN_MESSAGE_LEN: usize = PRELUDE_LEN + MESSAGE_CRC_LEN;
/// AWS caps event stream messages at 16 MiB; anything larger is corruption.
const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

/// A typed event stream header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderValue {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Bytes(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}

/// One decoded event stream message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub headers: Vec<(String, HeaderValue)>,
    pub payload: Vec<u8>,
}

impl Message {
    /// Value of a string header, if present.
    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|(key, value)| match value {
            HeaderValue::String(text) if key == name => Some(text.as_str()),
            _ => None,
        })
    }

    /// `:message-type` header (`event`, `exception` or `error`).
    pub fn message_type(&self) -> Option<&str> {
        self.header_str(":message-type")
    }

    /// `:event-type` header, e.g. `assistantResponseEvent`.
    pub fn event_type(&self) -> Option<&str> {
        self.header_str(":event-type")
    }

    /// `:exception-type` header, e.g. `ThrottlingException`.
    pub fn exception_type(&self) -> Option<&str> {
        self.header_str(":exception-type")
    }

    /// Payload parsed as JSON.
    pub fn payload_json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_slice(&self.payload)
    }

    /// Payload as (lossy) UTF-8 text, for diagnostics.
    pub fn payload_text(&self) -> String {
        String::from_utf8_lossy(&self.payload).into_owned()
    }
}

/// Unrecoverable framing error. The stream cannot be resynchronized after one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    InvalidLength {
        total_len: usize,
        headers_len: usize,
    },
    PreludeChecksum {
        expected: u32,
        actual: u32,
    },
    MessageChecksum {
        expected: u32,
        actual: u32,
    },
    InvalidHeader(&'static str),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLength {
                total_len,
                headers_len,
            } => write!(
                f,
                "invalid event stream frame length (total={total_len}, headers={headers_len})"
            ),
            Self::PreludeChecksum { expected, actual } => write!(
                f,
                "event stream prelude checksum mismatch (expected {expected:#010x}, got {actual:#010x})"
            ),
            Self::MessageChecksum { expected, actual } => write!(
                f,
                "event stream message checksum mismatch (expected {expected:#010x}, got {actual:#010x})"
            ),
            Self::InvalidHeader(reason) => write!(f, "invalid event stream header: {reason}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Incremental decoder: push raw response bytes, pull complete messages.
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append raw bytes received from the response body.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Number of bytes buffered but not yet decoded into a message.
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// Decode the next complete message, or `Ok(None)` if more bytes are needed.
    pub fn next_message(&mut self) -> Result<Option<Message>, DecodeError> {
        if self.buffer.len() < PRELUDE_LEN {
            return Ok(None);
        }

        let total_len = read_u32(&self.buffer[0..4]) as usize;
        let headers_len = read_u32(&self.buffer[4..8]) as usize;
        if !(MIN_MESSAGE_LEN..=MAX_MESSAGE_LEN).contains(&total_len)
            || headers_len > total_len - MIN_MESSAGE_LEN
        {
            return Err(DecodeError::InvalidLength {
                total_len,
                headers_len,
            });
        }

        let expected_prelude_crc = read_u32(&self.buffer[8..12]);
        let actual_prelude_crc = crc32(&self.buffer[0..8]);
        if expected_prelude_crc != actual_prelude_crc {
            return Err(DecodeError::PreludeChecksum {
                expected: expected_prelude_crc,
                actual: actual_prelude_crc,
            });
        }

        if self.buffer.len() < total_len {
            return Ok(None);
        }

        let frame: Vec<u8> = self.buffer.drain(..total_len).collect();
        let crc_offset = total_len - MESSAGE_CRC_LEN;
        let expected_message_crc = read_u32(&frame[crc_offset..]);
        let actual_message_crc = crc32(&frame[..crc_offset]);
        if expected_message_crc != actual_message_crc {
            return Err(DecodeError::MessageChecksum {
                expected: expected_message_crc,
                actual: actual_message_crc,
            });
        }

        let headers_end = PRELUDE_LEN + headers_len;
        let headers = parse_headers(&frame[PRELUDE_LEN..headers_end])?;
        let payload = frame[headers_end..crc_offset].to_vec();
        Ok(Some(Message { headers, payload }))
    }
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8], DecodeError> {
    let end = cursor
        .checked_add(len)
        .filter(|end| *end <= bytes.len())
        .ok_or(DecodeError::InvalidHeader(
            "header extends past header block",
        ))?;
    let slice = &bytes[*cursor..end];
    *cursor = end;
    Ok(slice)
}

fn take_array<const N: usize>(bytes: &[u8], cursor: &mut usize) -> Result<[u8; N], DecodeError> {
    let slice = take(bytes, cursor, N)?;
    let mut out = [0u8; N];
    out.copy_from_slice(slice);
    Ok(out)
}

fn parse_headers(bytes: &[u8]) -> Result<Vec<(String, HeaderValue)>, DecodeError> {
    let mut headers = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let [name_len] = take_array::<1>(bytes, &mut cursor)?;
        let name = std::str::from_utf8(take(bytes, &mut cursor, name_len as usize)?)
            .map_err(|_| DecodeError::InvalidHeader("header name is not UTF-8"))?
            .to_string();
        let [value_type] = take_array::<1>(bytes, &mut cursor)?;
        let value = match value_type {
            0 => HeaderValue::Bool(true),
            1 => HeaderValue::Bool(false),
            2 => HeaderValue::Byte(i8::from_be_bytes(take_array::<1>(bytes, &mut cursor)?)),
            3 => HeaderValue::Short(i16::from_be_bytes(take_array::<2>(bytes, &mut cursor)?)),
            4 => HeaderValue::Int(i32::from_be_bytes(take_array::<4>(bytes, &mut cursor)?)),
            5 => HeaderValue::Long(i64::from_be_bytes(take_array::<8>(bytes, &mut cursor)?)),
            6 | 7 => {
                let len = u16::from_be_bytes(take_array::<2>(bytes, &mut cursor)?) as usize;
                let raw = take(bytes, &mut cursor, len)?;
                if value_type == 6 {
                    HeaderValue::Bytes(raw.to_vec())
                } else {
                    HeaderValue::String(
                        std::str::from_utf8(raw)
                            .map_err(|_| DecodeError::InvalidHeader("string header is not UTF-8"))?
                            .to_string(),
                    )
                }
            }
            8 => HeaderValue::Timestamp(i64::from_be_bytes(take_array::<8>(bytes, &mut cursor)?)),
            9 => HeaderValue::Uuid(take_array::<16>(bytes, &mut cursor)?),
            _ => return Err(DecodeError::InvalidHeader("unknown header value type")),
        };
        headers.push((name, value));
    }
    Ok(headers)
}

const CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                0xEDB8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

/// CRC-32 (IEEE 802.3), as used by the AWS event stream framing.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc = CRC32_TABLE[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}
