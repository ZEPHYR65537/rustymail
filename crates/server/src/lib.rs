//! Laboratory SMTP service; production features are gated until implemented.
mod worker;
pub use worker::store_options;
use worker::{StoreClient, StoreWorker};

use rustymail_core::config::{Config, ConfigError};
use rustymail_protocol::{
    Action, Command, Envelope, LineDecoder, ParseError, Reply, Session, decode_data_line,
    parse_command,
};
use rustymail_store::{Acceptance, PreparedMessage, StagedMessage, StorageRuntime, StoreError};
use std::{
    collections::HashMap,
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        TcpListener, TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
    time::timeout,
};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Storage(#[from] StoreError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("server task failed")]
    Task,
}

pub fn log_event(event: &str, fields: serde_json::Value) {
    eprintln!("{}", serde_json::json!({"event":event,"fields":fields}));
}

pub struct LabServer {
    listener: TcpListener,
    config: Arc<Config>,
    worker: StoreWorker,
}

struct ConnectionLease {
    _permit: OwnedSemaphorePermit,
    ip: IpAddr,
    counts: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        if let Ok(mut counts) = self.counts.lock()
            && let Some(count) = counts.get_mut(&self.ip)
        {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

impl LabServer {
    pub async fn bind(config: Config) -> Result<Self, ServerError> {
        Self::bind_with_runtime(config, StorageRuntime::default()).await
    }

    pub async fn bind_with_runtime(
        config: Config,
        runtime: StorageRuntime,
    ) -> Result<Self, ServerError> {
        config.require_lab_receiver()?;
        let listener = TcpListener::bind(config.listeners.smtp).await?;
        let for_worker = config.clone();
        let worker = tokio::task::spawn_blocking(move || StoreWorker::start(&for_worker, runtime))
            .await
            .map_err(|_| ServerError::Task)??;
        Ok(Self {
            listener,
            config: Arc::new(config),
            worker,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub async fn serve_until(self, shutdown: impl Future<Output = ()>) -> Result<(), ServerError> {
        let connections = Arc::new(Semaphore::new(self.config.limits.connections));
        // Reserving worst-case message size bounds temporary bytes as well as
        // the number of active DATA streams. All permits live through commit.
        let slots = (self.config.limits.temporary_reserved_bytes / self.config.limits.message_bytes)
            .min(self.config.limits.ingest_concurrency as u64) as usize;
        let ingest = Arc::new(Semaphore::new(slots));
        let counts = Arc::new(Mutex::new(HashMap::<IpAddr, usize>::new()));
        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);
        log_event(
            "lab_smtp_ready",
            serde_json::json!({"bind":self.local_addr()?.to_string(),
            "tls":false,"imap":false,"outbound":false,"scanning":false,
            "directory_sync":rustymail_store::Store::directory_sync_supported()}),
        );
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                joined = tasks.join_next(), if !tasks.is_empty() => {
                    if joined.is_some_and(|result| result.is_err()) { log_event("session_task_failed",serde_json::json!({})); }
                }
                incoming = self.listener.accept() => {
                    let (stream, peer) = incoming?;
                    let Ok(permit) = connections.clone().try_acquire_owned() else {
                        reject_connection(stream,b"421 4.3.2 Connection limit\r\n").await; continue;
                    };
                    let allowed = {
                        let mut map = counts.lock().map_err(|_| ServerError::Task)?;
                        if map.get(&peer.ip()).copied().unwrap_or(0) >= self.config.limits.connections_per_ip {
                            false
                        } else { *map.entry(peer.ip()).or_default() += 1; true }
                    };
                    if !allowed { reject_connection(stream,b"421 4.3.2 IP connection limit\r\n").await; continue; }
                    let lease = ConnectionLease { _permit:permit, ip:peer.ip(), counts:counts.clone() };
                    let config = self.config.clone();
                    let client = self.worker.client.clone();
                    let ingest = ingest.clone();
                    tasks.spawn(async move {
                        let _lease = lease;
                        if smtp_session(stream,config,client,ingest).await.is_err() {
                            // Deliberately no message, address, AUTH or arbitrary input in logs.
                            log_event("smtp_session_closed_with_error",serde_json::json!({}));
                        }
                    });
                }
            }
        }
        drop(self.listener);
        let grace = Duration::from_secs(self.config.timeouts.shutdown_grace_seconds);
        if timeout(grace, async { while tasks.join_next().await.is_some() {} })
            .await
            .is_err()
        {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
        self.worker.shutdown().await?;
        log_event("stopped", serde_json::json!({}));
        Ok(())
    }
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "SMTP phase deadline exceeded")
}

async fn reject_connection(mut stream: TcpStream, message: &[u8]) {
    // A fresh nonblocking socket may not yet be marked writable. Poll readiness
    // through write_all, with a small deadline and no unbounded rejection tasks.
    let _ = timeout(Duration::from_millis(250), stream.write_all(message)).await;
}

async fn line(reader: &mut BufReader<OwnedReadHalf>, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut decoder = LineDecoder::new(limit);
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if decoder.buffered_bytes() == 0 {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial SMTP line",
                ))
            };
        }
        let (used, result) = decoder
            .feed(available)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        reader.consume(used);
        if result.is_some() {
            return Ok(result);
        }
    }
}

async fn write_response(writer: &mut OwnedWriteHalf, bytes: &[u8]) -> io::Result<()> {
    timeout(Duration::from_secs(30), writer.write_all(bytes))
        .await
        .map_err(|_| timed_out())?
}

async fn reply(writer: &mut OwnedWriteHalf, reply: Reply) -> io::Result<()> {
    write_response(
        writer,
        format!("{} {}\r\n", reply.code, reply.text).as_bytes(),
    )
    .await
}

async fn smtp_session(
    stream: TcpStream,
    config: Arc<Config>,
    store: StoreClient,
    ingest: Arc<Semaphore>,
) -> Result<(), ServerError> {
    stream.set_nodelay(true)?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::with_capacity(config.limits.stream_buffer_bytes, read);
    write_response(
        &mut write,
        format!("220 {} rustymail LAB SMTP\r\n", config.hostname).as_bytes(),
    )
    .await?;
    let mut state = Session::new(
        config.limits.message_bytes,
        config.limits.recipients_per_message,
    );
    loop {
        let incoming = timeout(
            Duration::from_secs(config.timeouts.smtp_command_seconds),
            line(&mut read, 512),
        )
        .await;
        let bytes = match incoming {
            Ok(Ok(Some(bytes))) => bytes,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(error)) => {
                let _ = reply(&mut write, Reply::new(500, "5.5.2 Invalid framing")).await;
                return Err(error.into());
            }
            Err(_) => {
                let _ = reply(&mut write, Reply::new(421, "4.4.2 Command timeout")).await;
                return Ok(());
            }
        };
        let command = match parse_command(&bytes) {
            Ok(command) => command,
            Err(error) => {
                if bytes
                    .split(|&b| b == b' ')
                    .next()
                    .is_some_and(|verb| verb.eq_ignore_ascii_case(b"MAIL"))
                {
                    state.apply(Command::Reset);
                }
                let response = match error {
                    ParseError::Syntax => {
                        Reply::new(501, "5.5.2 Invalid syntax or unsupported mailbox form")
                    }
                    ParseError::UnsupportedParameter => {
                        Reply::new(555, "5.5.4 Unsupported parameter")
                    }
                };
                reply(&mut write, response).await?;
                continue;
            }
        };
        match state.apply(command) {
            Action::Reply(response) => reply(&mut write, response).await?,
            Action::Hello { extended } => {
                let text = if extended {
                    format!(
                        "250-{}\r\n250-SIZE {}\r\n250-8BITMIME\r\n250 ENHANCEDSTATUSCODES\r\n",
                        config.hostname, config.limits.message_bytes
                    )
                } else {
                    format!("250 {}\r\n", config.hostname)
                };
                write_response(&mut write, text.as_bytes()).await?;
            }
            Action::Quit => {
                reply(&mut write, Reply::new(221, "2.0.0 Bye")).await?;
                return Ok(());
            }
            Action::CheckRecipient(address) => {
                let local = config
                    .local_domains
                    .iter()
                    .any(|domain| domain.eq_ignore_ascii_case(address.domain()));
                let result = if local {
                    timeout(
                        Duration::from_secs(30),
                        store.recipient_exists(address.clone()),
                    )
                    .await
                    .map_err(|_| StoreError::WorkerUnavailable)
                    .and_then(|result| result)
                } else {
                    Ok(false)
                };
                match result {
                    Ok(exists) => {
                        reply(&mut write, state.recipient_result(address, exists)).await?
                    }
                    Err(_) => {
                        reply(
                            &mut write,
                            Reply::new(451, "4.3.0 Recipient lookup unavailable"),
                        )
                        .await?
                    }
                }
            }
            Action::BeginData(envelope) => {
                let Ok(_permit) = ingest.clone().try_acquire_owned() else {
                    reply(
                        &mut write,
                        Reply::new(452, "4.3.1 Ingest capacity exhausted"),
                    )
                    .await?;
                    continue;
                };
                let stage = match timeout(Duration::from_secs(30), store.stage()).await {
                    Ok(Ok(stage)) => stage,
                    _ => {
                        reply(&mut write, Reply::new(452, "4.3.1 Storage unavailable")).await?;
                        continue;
                    }
                };
                reply(
                    &mut write,
                    Reply::new(354, "Send message; end with <CRLF>.<CRLF>"),
                )
                .await?;
                let receiving = timeout(
                    Duration::from_secs(config.timeouts.data_total_seconds),
                    receive_message(&mut read, stage, &config),
                )
                .await;
                let message = match receiving {
                    Ok(Ok(message)) => message,
                    Ok(Err(ServerError::Storage(StoreError::SizeLimit))) => {
                        reply(
                            &mut write,
                            Reply::new(552, "5.3.4 Message or headers too large"),
                        )
                        .await?;
                        return Ok(());
                    }
                    _ => {
                        let _ = reply(
                            &mut write,
                            Reply::new(451, "4.3.0 DATA incomplete or unavailable"),
                        )
                        .await;
                        return Ok(());
                    }
                };
                // The result is ambiguous after dispatch if the reply channel
                // fails or times out. In that case close without a false 4xx.
                let Envelope { sender, recipients } = envelope;
                let operation_id = Uuid::new_v4().simple().to_string();
                let plan = Acceptance {
                    operation_id: operation_id.clone(),
                    sender,
                    recipients,
                };
                match timeout(Duration::from_secs(60), store.accept(message, plan)).await {
                    Ok(Ok(accepted)) => {
                        log_event(
                            "message_accepted",
                            serde_json::json!({"message_id":accepted.message_id,"operation_id":operation_id}),
                        );
                        write_response(
                            &mut write,
                            format!("250 2.0.0 Accepted {}\r\n", accepted.message_id).as_bytes(),
                        )
                        .await?;
                    }
                    Ok(Err(StoreError::OutcomeUnknown)) | Err(_) => {
                        log_event(
                            "acceptance_outcome_unknown",
                            serde_json::json!({"operation_id":operation_id}),
                        );
                        return Ok(());
                    }
                    Ok(Err(StoreError::Quota)) => {
                        reply(&mut write, Reply::new(452, "4.2.2 Mailbox quota exceeded")).await?
                    }
                    Ok(Err(StoreError::RecipientUnavailable)) => {
                        reply(
                            &mut write,
                            Reply::new(451, "4.3.0 Recipient changed; retry transaction"),
                        )
                        .await?
                    }
                    Ok(Err(_)) => {
                        reply(&mut write, Reply::new(451, "4.3.0 Commit rejected")).await?
                    }
                }
            }
        }
    }
}

async fn receive_message(
    reader: &mut BufReader<OwnedReadHalf>,
    mut stage: StagedMessage,
    config: &Config,
) -> Result<PreparedMessage, ServerError> {
    let mut in_headers = true;
    let mut headers = 0usize;
    loop {
        // One extra octet is permitted for SMTP transparency. The decoded line
        // including CRLF is checked against 1000 bytes separately.
        let raw = timeout(
            Duration::from_secs(config.timeouts.data_idle_seconds),
            line(reader, 1001),
        )
        .await
        .map_err(|_| timed_out())??
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "incomplete DATA"))?;
        let data = decode_data_line(raw)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let Some(data) = data else {
            return Ok(stage.prepare().await?);
        };
        if in_headers {
            headers = headers
                .checked_add(data.len())
                .ok_or(StoreError::SizeLimit)?;
            if headers > config.limits.header_bytes {
                return Err(StoreError::SizeLimit.into());
            }
            if data == b"\r\n" {
                in_headers = false;
            }
        }
        stage.append(&data).await?;
    }
}
