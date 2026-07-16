use crate::framing::{FramedReader, FramedWriter, FramingError};
use agent_protocol::{
    REMOTE_PROTOCOL_VERSION, RemoteEnvelope, RemoteEvent, RemoteHello, RemoteHelloAck,
    RemoteMessage, RemoteRequest, RemoteResponse,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};

#[derive(Debug, Error)]
pub enum RemoteClientError {
    #[error(transparent)]
    Framing(#[from] FramingError),
    #[error("remote protocol version {actual} is incompatible with {expected}")]
    Protocol { expected: u32, actual: u32 },
    #[error("remote handshake was invalid")]
    Handshake,
    #[error("remote connection closed")]
    Closed,
    #[error("remote request failed: {0}")]
    Remote(String),
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<RemoteResponse>>>>;

#[derive(Clone)]
pub struct RemoteClient {
    writer: mpsc::Sender<RemoteEnvelope>,
    pending: Pending,
    events: broadcast::Sender<RemoteEnvelope>,
    closed: watch::Receiver<bool>,
    next_request: Arc<AtomicU64>,
    hello: RemoteHello,
}

impl RemoteClient {
    pub async fn connect<R, W>(reader: R, writer: W) -> Result<Self, RemoteClientError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut reader = FramedReader::new(reader);
        let handshake = reader.read_handshake::<RemoteEnvelope>().await?;
        validate_version(handshake.protocol_version)?;
        if handshake.channel_id != 0 {
            return Err(RemoteClientError::Handshake);
        }
        let RemoteMessage::Hello(hello) = handshake.message else {
            return Err(RemoteClientError::Handshake);
        };

        let mut writer = FramedWriter::new(writer);
        writer
            .write_handshake(&RemoteEnvelope::new(
                0,
                "hello-ack",
                RemoteMessage::HelloAck(RemoteHelloAck {
                    desktop_version: env!("CARGO_PKG_VERSION").to_string(),
                }),
            ))
            .await?;

        let (writer_tx, mut writer_rx) = mpsc::channel::<RemoteEnvelope>(64);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let pending_for_reader = pending.clone();
        let (events, _) = broadcast::channel(256);
        let events_for_reader = events.clone();
        let (closed_tx, closed) = watch::channel(false);
        let closed_for_writer = closed_tx.clone();

        tokio::spawn(async move {
            while let Some(envelope) = writer_rx.recv().await {
                if writer.write_frame(&envelope).await.is_err() {
                    break;
                }
            }
            let _ = closed_for_writer.send(true);
        });

        tokio::spawn(async move {
            loop {
                let envelope = match reader.read_frame().await {
                    Ok(Some(envelope)) => envelope,
                    Ok(None) | Err(_) => break,
                };
                if validate_version(envelope.protocol_version).is_err() {
                    break;
                }
                match &envelope.message {
                    RemoteMessage::Response(response) => {
                        if let Some(sender) =
                            pending_for_reader.lock().await.remove(&envelope.request_id)
                        {
                            let _ = sender.send(response.clone());
                        }
                    }
                    RemoteMessage::Event(RemoteEvent::SessionMessage { .. })
                    | RemoteMessage::Event(RemoteEvent::WorkspaceLog { .. })
                    | RemoteMessage::Event(RemoteEvent::WorkerExited { .. })
                    | RemoteMessage::Event(RemoteEvent::WorkspaceReconnected { .. }) => {
                        let _ = events_for_reader.send(envelope);
                    }
                    _ => {}
                }
            }
            pending_for_reader.lock().await.clear();
            let _ = closed_tx.send(true);
        });

        Ok(Self {
            writer: writer_tx,
            pending,
            events,
            closed,
            next_request: Arc::new(AtomicU64::new(1)),
            hello,
        })
    }

    pub fn hello(&self) -> &RemoteHello {
        &self.hello
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<RemoteEnvelope> {
        self.events.subscribe()
    }

    pub fn subscribe_closed(&self) -> watch::Receiver<bool> {
        self.closed.clone()
    }

    pub async fn request(
        &self,
        channel_id: u32,
        request: RemoteRequest,
    ) -> Result<RemoteResponse, RemoteClientError> {
        let request_id = format!(
            "remote-{}",
            self.next_request.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), sender);
        let envelope = RemoteEnvelope::new(
            channel_id,
            request_id.clone(),
            RemoteMessage::Request(request),
        );
        if self.writer.send(envelope).await.is_err() {
            self.pending.lock().await.remove(&request_id);
            return Err(RemoteClientError::Closed);
        }
        let response = receiver.await.map_err(|_| RemoteClientError::Closed)?;
        match response {
            RemoteResponse::Error(error) => Err(RemoteClientError::Remote(error.message)),
            response => Ok(response),
        }
    }
}

fn validate_version(actual: u32) -> Result<(), RemoteClientError> {
    if actual == REMOTE_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(RemoteClientError::Protocol {
            expected: REMOTE_PROTOCOL_VERSION,
            actual,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{RemoteActivity, RemoteMessage, RemoteRole};

    async fn write_server_hello<W: AsyncWrite + Unpin>(writer: W) -> FramedWriter<W> {
        let mut writer = FramedWriter::new(writer);
        writer
            .write_handshake(&RemoteEnvelope::new(
                0,
                "hello",
                RemoteMessage::Hello(RemoteHello {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    platform: "linux".to_string(),
                    arch: "x86_64".to_string(),
                    pid: 1,
                    role: RemoteRole::Host,
                }),
            ))
            .await
            .expect("write hello");
        writer
    }

    fn response_for(envelope: &RemoteEnvelope) -> RemoteResponse {
        match &envelope.message {
            RemoteMessage::Request(RemoteRequest::Ping) => RemoteResponse::Pong,
            RemoteMessage::Request(RemoteRequest::Activity) => {
                RemoteResponse::Activity(RemoteActivity {
                    running_turns: 2,
                    pending_approvals: 1,
                })
            }
            _ => panic!("unexpected test request"),
        }
    }

    #[tokio::test]
    async fn concurrent_requests_are_matched_by_request_id() {
        let (client_stream, server_stream) = tokio::io::duplex(16 * 1024);
        let (client_read, client_write) = tokio::io::split(client_stream);
        let (server_read, server_write) = tokio::io::split(server_stream);
        let server = tokio::spawn(async move {
            let mut writer = write_server_hello(server_write).await;
            let mut reader = FramedReader::new(server_read);
            reader
                .read_handshake::<RemoteEnvelope>()
                .await
                .expect("read hello ack");
            let first = reader
                .read_frame()
                .await
                .expect("read first")
                .expect("first frame");
            let second = reader
                .read_frame()
                .await
                .expect("read second")
                .expect("second frame");
            writer
                .write_frame(&RemoteEnvelope::new(
                    second.channel_id,
                    second.request_id.clone(),
                    RemoteMessage::Response(response_for(&second)),
                ))
                .await
                .expect("write second response");
            writer
                .write_frame(&RemoteEnvelope::new(
                    first.channel_id,
                    first.request_id.clone(),
                    RemoteMessage::Response(response_for(&first)),
                ))
                .await
                .expect("write first response");
        });
        let client = RemoteClient::connect(client_read, client_write)
            .await
            .expect("connect");
        let (ping, activity) = tokio::join!(
            client.request(1, RemoteRequest::Ping),
            client.request(1, RemoteRequest::Activity)
        );

        assert!(matches!(ping.expect("ping"), RemoteResponse::Pong));
        assert!(matches!(
            activity.expect("activity"),
            RemoteResponse::Activity(RemoteActivity {
                running_turns: 2,
                pending_approvals: 1
            })
        ));
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn incompatible_handshake_version_is_rejected() {
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_stream);
        let (_server_read, server_write) = tokio::io::split(server_stream);
        let server = tokio::spawn(async move {
            let mut envelope = RemoteEnvelope::new(
                0,
                "hello",
                RemoteMessage::Hello(RemoteHello {
                    version: "other".to_string(),
                    platform: "linux".to_string(),
                    arch: "x86_64".to_string(),
                    pid: 1,
                    role: RemoteRole::Host,
                }),
            );
            envelope.protocol_version += 1;
            let mut writer = FramedWriter::new(server_write);
            writer
                .write_handshake(&envelope)
                .await
                .expect("write hello");
        });

        let error = match RemoteClient::connect(client_read, client_write).await {
            Ok(_) => panic!("incompatible version must fail"),
            Err(error) => error,
        };
        assert!(matches!(error, RemoteClientError::Protocol { .. }));
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn pending_request_fails_when_the_transport_disconnects() {
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_stream);
        let (server_read, server_write) = tokio::io::split(server_stream);
        let server = tokio::spawn(async move {
            let _writer = write_server_hello(server_write).await;
            let mut reader = FramedReader::new(server_read);
            reader
                .read_handshake::<RemoteEnvelope>()
                .await
                .expect("read hello ack");
            reader
                .read_frame()
                .await
                .expect("read request")
                .expect("request frame");
        });
        let client = RemoteClient::connect(client_read, client_write)
            .await
            .expect("connect");

        let error = client
            .request(1, RemoteRequest::Ping)
            .await
            .expect_err("disconnect must fail request");

        assert!(matches!(error, RemoteClientError::Closed));
        server.await.expect("server task");
    }
}
