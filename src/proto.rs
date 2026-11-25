use std::io;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Longest JSON header of a stream, in bytes.
pub const MAX_HEADER_LEN: usize = 4096;

/// First frame of a stream, sent by its opener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRequest {
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotAllowed,
    ResolveFailed,
    ConnectRefused,
    ConnectTimeout,
    ConnectFailed,
    TooManyStreams,
    ShuttingDown,
}

/// Answer of the tunnel, before any byte of the target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl OpenResponse {
    pub fn accepted() -> Self {
        Self {
            ok: true,
            error: None,
            message: None,
        }
    }

    pub fn refused(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(code),
            message: Some(message.into()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("the header length must be between 1 and {MAX_HEADER_LEN}")]
    Length,
    #[error("the header is not the expected JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Reads one frame: a big endian u16 length, then JSON.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let length = usize::from(reader.read_u16().await?);

    if length == 0 || length > MAX_HEADER_LEN {
        return Err(FrameError::Length);
    }

    let mut json = vec![0; length];
    reader.read_exact(&mut json).await?;

    Ok(serde_json::from_slice(&json)?)
}

pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let json = serde_json::to_vec(value)?;
    let length = u16::try_from(json.len())
        .ok()
        .filter(|length| usize::from(*length) <= MAX_HEADER_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "header too long"))?;
    let mut frame = Vec::with_capacity(json.len() + 2);

    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&json);
    writer.write_all(&frame).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read(bytes: &[u8]) -> Result<OpenRequest, FrameError> {
        read_frame(&mut &bytes[..]).await
    }

    fn framed(json: &[u8]) -> Vec<u8> {
        let mut frame = (json.len() as u16).to_be_bytes().to_vec();
        frame.extend_from_slice(json);
        frame
    }

    #[tokio::test]
    async fn writes_what_it_reads() {
        let mut wire = Vec::new();
        let refused = OpenResponse::refused(ErrorCode::NotAllowed, "erp.local:80 is not allowed");

        write_frame(&mut wire, &refused).await.unwrap();

        assert_eq!(
            read_frame::<_, OpenResponse>(&mut &wire[..]).await.unwrap(),
            refused
        );
        assert!(String::from_utf8_lossy(&wire).contains(r#""error":"not_allowed""#));
    }

    #[tokio::test]
    async fn ignores_unknown_fields_of_a_request() {
        let request = read(&framed(br#"{"target":"erp.local:80","trace":"abc"}"#))
            .await
            .unwrap();

        assert_eq!(request.target, "erp.local:80");
    }

    #[tokio::test]
    async fn refuses_an_empty_an_oversized_or_a_malformed_header() {
        let oversized = framed(&vec![b' '; MAX_HEADER_LEN + 1]);

        assert!(matches!(read(&[0, 0]).await, Err(FrameError::Length)));
        assert!(matches!(read(&oversized).await, Err(FrameError::Length)));
        assert!(matches!(
            read(&framed(b"{\"target\":7}")).await,
            Err(FrameError::Json(_))
        ));
        assert!(matches!(read(&[0, 9, b'{']).await, Err(FrameError::Io(_))));
    }
}
