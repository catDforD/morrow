use agent_protocol::{REMOTE_MAX_FRAME_BYTES, RemoteEnvelope};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::ErrorKind;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const MAX_HANDSHAKE_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum FramingError {
    #[error("remote stream ended")]
    Eof,
    #[error("remote frame is too large: {size} bytes")]
    FrameTooLarge { size: usize },
    #[error("remote handshake line is too large")]
    HandshakeTooLarge,
    #[error("remote I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("remote JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct FramedReader<R> {
    inner: BufReader<R>,
}

impl<R> FramedReader<R>
where
    R: AsyncRead + Unpin,
{
    pub fn new(reader: R) -> Self {
        Self {
            inner: BufReader::new(reader),
        }
    }

    pub async fn read_handshake<T: DeserializeOwned>(&mut self) -> Result<T, FramingError> {
        let mut line = Vec::new();
        let mut limited = (&mut self.inner).take((MAX_HANDSHAKE_BYTES + 1) as u64);
        let read = limited.read_until(b'\n', &mut line).await?;
        if read == 0 {
            return Err(FramingError::Eof);
        }
        if line.len() > MAX_HANDSHAKE_BYTES {
            return Err(FramingError::HandshakeTooLarge);
        }
        Ok(serde_json::from_slice(&line)?)
    }

    pub async fn read_frame(&mut self) -> Result<Option<RemoteEnvelope>, FramingError> {
        let mut length = [0_u8; 4];
        match self.inner.read_exact(&mut length[..1]).await {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        self.inner.read_exact(&mut length[1..]).await?;
        let size = u32::from_be_bytes(length) as usize;
        if size > REMOTE_MAX_FRAME_BYTES {
            return Err(FramingError::FrameTooLarge { size });
        }
        let mut payload = vec![0_u8; size];
        self.inner.read_exact(&mut payload).await?;
        Ok(Some(serde_json::from_slice(&payload)?))
    }
}

pub struct FramedWriter<W> {
    inner: W,
}

impl<W> FramedWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub fn new(writer: W) -> Self {
        Self { inner: writer }
    }

    pub async fn write_handshake<T: Serialize>(&mut self, value: &T) -> Result<(), FramingError> {
        let payload = serde_json::to_vec(value)?;
        if payload.len() > MAX_HANDSHAKE_BYTES {
            return Err(FramingError::HandshakeTooLarge);
        }
        self.inner.write_all(&payload).await?;
        self.inner.write_all(b"\n").await?;
        self.inner.flush().await?;
        Ok(())
    }

    pub async fn write_frame(&mut self, envelope: &RemoteEnvelope) -> Result<(), FramingError> {
        let payload = serde_json::to_vec(envelope)?;
        if payload.len() > REMOTE_MAX_FRAME_BYTES {
            return Err(FramingError::FrameTooLarge {
                size: payload.len(),
            });
        }
        self.inner
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await?;
        self.inner.write_all(&payload).await?;
        self.inner.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{RemoteMessage, RemoteRequest};

    #[tokio::test]
    async fn handshake_and_fragmented_frame_round_trip() {
        let (client, server) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client);
        let (server_read, server_write) = tokio::io::split(server);
        let expected =
            RemoteEnvelope::new(7, "request-1", RemoteMessage::Request(RemoteRequest::Ping));
        let expected_for_writer = expected.clone();

        let writer = tokio::spawn(async move {
            let mut writer = FramedWriter::new(client_write);
            writer
                .write_handshake(&expected_for_writer)
                .await
                .expect("write handshake");
            writer
                .write_frame(&expected_for_writer)
                .await
                .expect("write frame");
        });

        let mut reader = FramedReader::new(server_read);
        assert_eq!(
            reader
                .read_handshake::<RemoteEnvelope>()
                .await
                .expect("read handshake"),
            expected
        );
        assert_eq!(
            reader.read_frame().await.expect("read frame"),
            Some(expected)
        );
        writer.await.expect("writer task");

        drop(server_write);
        drop(client_read);
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_payload_read() {
        let (mut writer, reader) = tokio::io::duplex(16);
        let task = tokio::spawn(async move {
            writer
                .write_all(&((REMOTE_MAX_FRAME_BYTES + 1) as u32).to_be_bytes())
                .await
                .expect("write size");
        });
        let mut reader = FramedReader::new(reader);
        assert!(matches!(
            reader.read_frame().await,
            Err(FramingError::FrameTooLarge { .. })
        ));
        task.await.expect("writer task");
    }

    #[tokio::test]
    async fn coalesced_frames_are_split_at_length_boundaries() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let first = RemoteEnvelope::new(1, "first", RemoteMessage::Request(RemoteRequest::Ping));
        let second =
            RemoteEnvelope::new(2, "second", RemoteMessage::Request(RemoteRequest::Activity));
        let first_payload = serde_json::to_vec(&first).expect("first JSON");
        let second_payload = serde_json::to_vec(&second).expect("second JSON");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(first_payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&first_payload);
        bytes.extend_from_slice(&(second_payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&second_payload);

        let task = tokio::spawn(async move {
            writer.write_all(&bytes).await.expect("write frames");
        });
        let mut reader = FramedReader::new(reader);

        assert_eq!(reader.read_frame().await.expect("first frame"), Some(first));
        assert_eq!(
            reader.read_frame().await.expect("second frame"),
            Some(second)
        );
        task.await.expect("writer task");
    }

    #[tokio::test]
    async fn invalid_and_truncated_frames_are_rejected() {
        let (mut invalid_writer, invalid_reader) = tokio::io::duplex(64);
        let invalid = tokio::spawn(async move {
            invalid_writer
                .write_all(&5_u32.to_be_bytes())
                .await
                .expect("write invalid length");
            invalid_writer
                .write_all(b"nope!")
                .await
                .expect("write invalid JSON");
        });
        let mut invalid_reader = FramedReader::new(invalid_reader);
        assert!(matches!(
            invalid_reader.read_frame().await,
            Err(FramingError::Json(_))
        ));
        invalid.await.expect("invalid writer");

        let (mut truncated_writer, truncated_reader) = tokio::io::duplex(64);
        let truncated = tokio::spawn(async move {
            truncated_writer
                .write_all(&10_u32.to_be_bytes())
                .await
                .expect("write truncated length");
            truncated_writer
                .write_all(b"short")
                .await
                .expect("write truncated payload");
        });
        let mut truncated_reader = FramedReader::new(truncated_reader);
        assert!(matches!(
            truncated_reader.read_frame().await,
            Err(FramingError::Io(_))
        ));
        truncated.await.expect("truncated writer");

        let (mut header_writer, header_reader) = tokio::io::duplex(64);
        let truncated_header = tokio::spawn(async move {
            header_writer
                .write_all(&[0, 0])
                .await
                .expect("write truncated header");
        });
        let mut header_reader = FramedReader::new(header_reader);
        assert!(matches!(
            header_reader.read_frame().await,
            Err(FramingError::Io(_))
        ));
        truncated_header.await.expect("truncated header writer");
    }

    #[tokio::test]
    async fn oversized_handshake_is_rejected_at_the_limit() {
        let (mut writer, reader) = tokio::io::duplex(MAX_HANDSHAKE_BYTES + 16);
        let task = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; MAX_HANDSHAKE_BYTES + 1])
                .await
                .expect("write oversized handshake");
        });
        let mut reader = FramedReader::new(reader);
        assert!(matches!(
            reader.read_handshake::<RemoteEnvelope>().await,
            Err(FramingError::HandshakeTooLarge)
        ));
        task.await.expect("writer task");
    }
}
