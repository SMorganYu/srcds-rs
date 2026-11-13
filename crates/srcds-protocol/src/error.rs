use thiserror::Error;

/// Errors produced while decoding untrusted wire data.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProtocolError {
    #[error("unexpected end of data: needed {needed} more, {remaining} remaining")]
    Truncated { needed: usize, remaining: usize },
    #[error("string not terminated within {limit} bytes")]
    UnterminatedString { limit: usize },
    #[error("invalid header {0:#010x}")]
    InvalidHeader(u32),
    #[error("unknown message type {0:#04x}")]
    UnknownType(u8),
    #[error("packet too large: {size} > {limit}")]
    Oversized { size: usize, limit: usize },
    #[error("invalid field `{field}`: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    #[error("buffer overflow while writing: needed {needed}, capacity {capacity}")]
    Overflow { needed: usize, capacity: usize },
}

pub type Result<T> = std::result::Result<T, ProtocolError>;
