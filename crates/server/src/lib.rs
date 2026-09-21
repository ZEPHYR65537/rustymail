//! Laboratory SMTP service; production features are gated until implemented.
pub mod admin;
pub mod auth;
mod auth_dialog;
mod logging;
mod revocation;
pub mod tls;
mod trace;
mod transport;
mod worker;
pub use logging::{flush_logs, log_event, start_logging};
use transport::{Role, Transport};
pub use worker::store_options;
use worker::{StoreClient, StoreWorker};

use rustymail_core::config::{Config, ConfigError};
use rustymail_protocol::{
    Action, Command, Envelope, LineDecoder, ParseError, Reply, Session, decode_data_frame,
    parse_command,
};
use rustymail_store::{
    Acceptance, PreparedMessage, Principal, StagedMessage, StorageRuntime, StoreError,
    SubmissionIdentity,
};
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
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
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
    #[error("invalid message header structure")]
    InvalidHeaders,
}

pub struct LabServer {
    listener: TcpListener,
    role: Role,
    submission_listener: Option<TcpListener>,
    implicit_listener: Option<TcpListener>,
    config: Arc<Config>,
    worker: StoreWorker,
    tls: Option<tls::TlsSettings>,
    auth: Option<Arc<auth::AuthService>>,
    admin: Option<admin::AdminListener>,
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
            role: Role::Receiver,
            submission_listener: None,
            implicit_listener: None,
            config: Arc::new(config),
            worker,
            tls: None,
            auth: None,
            admin: None,
        })
    }

    /// Implicit TLS plus AUTH PLAIN on the loopback submissions listener.
    pub async fn bind_tls(config: Config) -> Result<Self, ServerError> {
        Self::bind_secure(config, false).await
    }

    /// Three loopback SMTP roles sharing one store and global admission budgets.
    pub async fn bind_smtp(config: Config) -> Result<Self, ServerError> {
        Self::bind_secure(config, true).await
    }

    async fn bind_secure(config: Config, all_roles: bool) -> Result<Self, ServerError> {
        config.require_lab_receiver()?;
        if !config.listeners.submissions.ip().is_loopback() {
            return Err(io::Error::other("lab TLS listener must use loopback").into());
        }
        let tls_config = config.tls.clone();
        let tls = tokio::task::spawn_blocking(move || tls::TlsSettings::new(tls_config))
            .await
            .map_err(|_| ServerError::Task)??;
        let auth = auth::AuthService::new(config.authentication.clone())
            .await
            .map_err(|_| io::Error::other("authentication initialization failed"))?;
        let listener = TcpListener::bind(if all_roles {
            config.listeners.smtp
        } else {
            config.listeners.submissions
        })
        .await?;
        let (submission_listener, implicit_listener) = if all_roles {
            (
                Some(TcpListener::bind(config.listeners.submission).await?),
                Some(TcpListener::bind(config.listeners.submissions).await?),
            )
        } else {
            (None, None)
        };
        #[cfg(unix)]
        let admin = Some(admin::AdminListener::bind(&config.admin_socket)?);
        #[cfg(not(unix))]
        let admin = None;
        let for_worker = config.clone();
        let worker = tokio::task::spawn_blocking(move || {
            StoreWorker::start(&for_worker, StorageRuntime::default())
        })
        .await
        .map_err(|_| ServerError::Task)??;
        Ok(Self {
            listener,
            role: if all_roles {
                Role::Receiver
            } else {
                Role::ImplicitSubmission
            },
            submission_listener,
            implicit_listener,
            config: Arc::new(config),
            worker,
            tls: Some(tls),
            auth: Some(auth),
            admin,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    async fn accept(&self) -> io::Result<(TcpStream, SocketAddr, Role)> {
        async fn optional(listener: &Option<TcpListener>) -> io::Result<(TcpStream, SocketAddr)> {
            match listener {
                Some(listener) => listener.accept().await,
                None => std::future::pending().await,
            }
        }
        let (incoming, role) = tokio::select! {
            result = self.listener.accept() => (result, self.role),
            result = optional(&self.submission_listener) => (result, Role::StartTlsSubmission),
            result = optional(&self.implicit_listener) => (result, Role::ImplicitSubmission),
        };
        let (stream, peer) = incoming?;
        Ok((stream, peer, role))
    }

    pub async fn serve_until(self, shutdown: impl Future<Output = ()>) -> Result<(), ServerError> {
        let connections = Arc::new(Semaphore::new(self.config.limits.connections));
        let handshakes = Arc::new(Semaphore::new(self.config.limits.tls_handshakes));
        let management = Arc::new(Semaphore::new(4));
        // Reserving worst-case message size bounds temporary bytes as well as
        // the number of active DATA streams. All permits live through commit.
        let slots = (self.config.limits.temporary_reserved_bytes
            / store_options(&self.config).max_message_bytes)
            .min(self.config.limits.ingest_concurrency as u64) as usize;
        let ingest = Arc::new(Semaphore::new(slots));
        let counts = Arc::new(Mutex::new(HashMap::<IpAddr, usize>::new()));
        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);
        log_event(
            "lab_smtp_ready",
            serde_json::json!({"bind":self.local_addr()?.to_string(),
            "tls":self.tls.is_some(),"imap":false,"outbound":false,"scanning":false,
            "directory_sync":rustymail_store::Store::directory_sync_supported()}),
        );
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                joined = tasks.join_next(), if !tasks.is_empty() => {
                    if joined.is_some_and(|result| result.is_err()) { log_event("session_task_failed",serde_json::json!({})); }
                }
                incoming=admin::accept(&self.admin)=> {
                    let stream=incoming?;
                    if let Ok(permit)=management.clone().try_acquire_owned()
                        && let (Some(auth),Some(tls))=(self.auth.clone(),self.tls.clone()) {
                        let client=self.worker.client.clone();let config=self.config.clone();
                        let mode=if self.submission_listener.is_some() {"lab_smtp"} else {"lab_tls"};
                        tasks.spawn(async move {let _permit=permit;
                            let _=timeout(Duration::from_secs(60),admin::session(stream,client,auth,tls,config,mode)).await;
                        });
                    }
                }
                incoming = self.accept() => {
                    let (stream, peer, role) = incoming?;
                    let Ok(permit) = connections.clone().try_acquire_owned() else {
                        if role != Role::ImplicitSubmission { reject_connection(stream,b"421 4.3.2 Connection limit\r\n").await; } continue;
                    };
                    let allowed = {
                        let mut map = counts.lock().map_err(|_| ServerError::Task)?;
                        if map.get(&peer.ip()).copied().unwrap_or(0) >= self.config.limits.connections_per_ip {
                            false
                        } else { *map.entry(peer.ip()).or_default() += 1; true }
                    };
                    if !allowed { if role != Role::ImplicitSubmission {reject_connection(stream,b"421 4.3.2 IP connection limit\r\n").await;} continue; }
                    let lease = ConnectionLease { _permit:permit, ip:peer.ip(), counts:counts.clone() };
                    let config = self.config.clone();
                    let client = self.worker.client.clone();
                    let ingest = ingest.clone();
                    let tls=self.tls.clone();let auth=self.auth.clone();let handshakes=handshakes.clone();
                    tasks.spawn(async move {
                        let _lease = lease;
                        let result=async {
                            stream.set_nodelay(true)?;
                            smtp_session(stream, SessionContext {config, store:client, ingest, auth, peer:peer.ip(), tls, handshakes, role}).await
                        }.await;
                        if result.is_err() {
                            // Deliberately no message, address, AUTH or arbitrary input in logs.
                            log_event("smtp_session_closed_with_error",serde_json::json!({}));
                        }
                    });
                }
            }
        }
        drop(self.listener);
        drop(self.submission_listener);
        drop(self.implicit_listener);
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

async fn line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    limit: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut decoder = LineDecoder::new(limit);
    if !fill_line(reader, &mut decoder).await? {
        return Ok(None);
    }
    // Commands own their small buffer so AUTH can zeroize it independently.
    Ok(decoder.take_line())
}

async fn fill_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    decoder: &mut LineDecoder,
) -> io::Result<bool> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if decoder.buffered_bytes() == 0 {
                Ok(false)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial SMTP line",
                ))
            };
        }
        let (used, complete) = decoder
            .feed_buffered(available)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        reader.consume(used);
        if complete {
            return Ok(true);
        }
    }
}

async fn write_response<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    timeout(Duration::from_secs(30), async {
        writer.write_all(bytes).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| timed_out())?
}

async fn reply<W: AsyncWrite + Unpin>(writer: &mut W, reply: Reply) -> io::Result<()> {
    write_response(
        writer,
        format!("{} {}\r\n", reply.code, reply.text).as_bytes(),
    )
    .await
}

struct SessionContext {
    config: Arc<Config>,
    store: StoreClient,
    ingest: Arc<Semaphore>,
    auth: Option<Arc<auth::AuthService>>,
    peer: IpAddr,
    tls: Option<tls::TlsSettings>,
    handshakes: Arc<Semaphore>,
    role: Role,
}

async fn smtp_session(stream: TcpStream, context: SessionContext) -> Result<(), ServerError> {
    let SessionContext {
        config,
        store,
        ingest,
        auth,
        peer,
        tls,
        handshakes,
        role,
    } = context;
    let auth = auth.filter(|_| role.submission());
    // One unauthenticated lifetime, including STARTTLS: upgrading does not buy
    // another connection budget or another authentication timeout.
    let unauthenticated_deadline = tokio::time::Instant::now()
        + Duration::from_secs(config.timeouts.submission_unauthenticated_seconds);
    let mut encrypted = role == Role::ImplicitSubmission;
    let mut stream = Transport::Plain(stream);
    if encrypted {
        let Ok(_permit) = handshakes.clone().try_acquire_owned() else {
            return Ok(());
        };
        let tls = tls
            .as_ref()
            .ok_or_else(|| io::Error::other("TLS settings missing"))?;
        stream = tokio::time::timeout_at(
            unauthenticated_deadline,
            stream.upgrade(
                tls.config()?,
                Duration::from_secs(config.tls.handshake_timeout_seconds),
            ),
        )
        .await
        .map_err(|_| timed_out())??;
    }
    let (read, mut write) = tokio::io::split(stream);
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
    let mut principal: Option<Principal> = None;
    let (_unused, mut changes) = tokio::sync::watch::channel(0);
    if let Some(auth) = &auth {
        changes = auth.changes.subscribe();
    }
    let mut revocations = revocation::Revocations::new(changes);
    let mut auth_attempts = 0;
    loop {
        let command_deadline =
            tokio::time::Instant::now() + Duration::from_secs(config.timeouts.smtp_command_seconds);
        let deadline = if auth.is_some() && principal.is_none() {
            command_deadline.min(unauthenticated_deadline)
        } else {
            command_deadline
        };
        let incoming = tokio::select! {
            _=revocations.wait(principal.as_ref(),&store)=>{reply(&mut write,Reply::new(421,"4.7.0 Session authorization changed")).await?;return Ok(());},
            result=tokio::time::timeout_at(deadline,line(&mut read,if auth.is_some(){1024}else{512}))=>result,
        };
        let bytes = zeroize::Zeroizing::new(match incoming {
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
        });
        if bytes
            .split(|&b| b == b' ')
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case(b"AUTH"))
        {
            if !role.submission() {
                reply(
                    &mut write,
                    Reply::new(502, "5.5.1 AUTH not available on receiver"),
                )
                .await?;
                continue;
            }
            if !encrypted {
                reply(&mut write, Reply::new(538, "5.7.11 Encryption required")).await?;
                continue;
            }
            let Some(auth) = &auth else {
                reply(&mut write, Reply::new(538, "5.7.11 Encryption required")).await?;
                continue;
            };
            if principal.is_some() || !state.authentication_allowed() {
                reply(
                    &mut write,
                    Reply::new(503, "5.5.1 AUTH not allowed in this state"),
                )
                .await?;
                continue;
            }
            match auth_dialog::authenticate(
                &mut read, &mut write, &bytes, auth, &store, peer, deadline,
            )
            .await?
            {
                auth_dialog::AuthOutcome::Authenticated(identity) => principal = Some(identity),
                auth_dialog::AuthOutcome::Failed => auth_attempts += 1,
                auth_dialog::AuthOutcome::Ignored => (),
            }
            if auth_attempts >= 5 && principal.is_none() {
                return Ok(());
            }
            continue;
        }
        if bytes.len() + 2 > 512 {
            reply(&mut write, Reply::new(500, "5.5.2 Command too long")).await?;
            return Ok(());
        }
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
        if role.submission()
            && !encrypted
            && !matches!(
                command,
                Command::Ehlo(_) | Command::Noop | Command::StartTls | Command::Quit
            )
        {
            reply(
                &mut write,
                Reply::new(530, "5.7.0 Must issue STARTTLS first"),
            )
            .await?;
            continue;
        }
        if auth.is_some() {
            if matches!(command, Command::Mail { .. }) {
                state.apply(Command::Reset);
            }
            if principal.is_none()
                && matches!(
                    command,
                    Command::Mail { .. } | Command::Rcpt(_) | Command::Data
                )
            {
                reply(&mut write, Reply::new(530, "5.7.0 Authentication required")).await?;
                continue;
            }
            if let Command::Mail { sender, .. } = &command {
                let allowed = if let (Some(identity), Some(sender)) = (&principal, sender) {
                    let identity = identity.clone();
                    let sender = sender.clone();
                    store
                        .call(move |store| store.authorize_sender(&identity, &sender))
                        .await?
                } else {
                    false
                };
                if !allowed {
                    reply(
                        &mut write,
                        Reply::new(553, "5.7.1 Sender identity not permitted"),
                    )
                    .await?;
                    continue;
                }
            }
        }
        match state.apply(command) {
            Action::Reply(response) => reply(&mut write, response).await?,
            Action::Hello { extended } => {
                let text = if extended {
                    format!(
                        "250-{}\r\n250-SIZE {}\r\n250-8BITMIME\r\n{}250 ENHANCEDSTATUSCODES\r\n",
                        config.hostname,
                        config.limits.message_bytes,
                        if !encrypted && tls.is_some() {
                            "250-STARTTLS\r\n"
                        } else if encrypted && auth.is_some() && principal.is_none() {
                            "250-AUTH PLAIN\r\n"
                        } else {
                            ""
                        }
                    )
                } else {
                    format!("250 {}\r\n", config.hostname)
                };
                write_response(&mut write, text.as_bytes()).await?;
            }
            Action::StartTls => {
                if encrypted {
                    reply(&mut write, Reply::new(503, "5.5.1 TLS already active")).await?;
                    continue;
                }
                let Some(tls) = &tls else {
                    reply(&mut write, Reply::new(502, "5.5.1 STARTTLS not available")).await?;
                    continue;
                };
                let Ok(_permit) = handshakes.clone().try_acquire_owned() else {
                    reply(&mut write, Reply::new(454, "4.7.0 TLS capacity exhausted")).await?;
                    continue;
                };
                let tls_config = tls.config()?;
                reply(&mut write, Reply::new(220, "2.0.0 Ready to start TLS")).await?;
                // into_inner deliberately discards every prefetched plaintext
                // byte. Remaining kernel bytes enter TLS parsing, never SMTP.
                let plain = discard_read_buffer(read, write);
                let handshake = plain.upgrade(
                    tls_config,
                    Duration::from_secs(config.tls.handshake_timeout_seconds),
                );
                let stream = if role.submission() {
                    tokio::time::timeout_at(unauthenticated_deadline, handshake)
                        .await
                        .map_err(|_| timed_out())??
                } else {
                    handshake.await?
                };
                let halves = tokio::io::split(stream);
                read = BufReader::with_capacity(config.limits.stream_buffer_bytes, halves.0);
                write = halves.1;
                encrypted = true;
                state = Session::new(
                    config.limits.message_bytes,
                    config.limits.recipients_per_message,
                );
                principal = None;
                // No second SMTP banner; the client should send a fresh EHLO.
            }
            Action::Quit => {
                reply(&mut write, Reply::new(221, "2.0.0 Bye")).await?;
                let _ = timeout(Duration::from_secs(5), write.shutdown()).await;
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
                // Generate once per DATA transaction. The same operation ID is
                // persisted with the exact prepared bytes; never regenerate on replay.
                let operation = Uuid::new_v4();
                let prefix = trace::Trace {
                    hostname: &config.hostname,
                    greeting: state.greeting(),
                    peer,
                    extended: state.extended(),
                    encrypted,
                    authenticated: principal.is_some(),
                }
                .prefix(
                    envelope.sender.as_ref(),
                    operation,
                    time::OffsetDateTime::now_utc(),
                )?;
                reply(
                    &mut write,
                    Reply::new(354, "Send message; end with <CRLF>.<CRLF>"),
                )
                .await?;
                let receiving = tokio::select! {
                    _=revocations.wait(principal.as_ref(),&store)=>{reply(&mut write,Reply::new(421,"4.7.0 Session authorization changed")).await?;return Ok(());},
                    result=timeout(Duration::from_secs(config.timeouts.data_total_seconds),receive_message(&mut read,stage,&config,auth.is_some(),&prefix))=>result,
                };
                let (message, author) = match receiving {
                    Ok(Ok(message)) => message,
                    Ok(Err(ServerError::Storage(StoreError::SizeLimit))) => {
                        reply(
                            &mut write,
                            Reply::new(552, "5.3.4 Message or headers too large"),
                        )
                        .await?;
                        return Ok(());
                    }
                    Ok(Err(ServerError::Storage(StoreError::PermissionDenied))) => {
                        reply(
                            &mut write,
                            Reply::new(550, "5.7.1 Invalid submission identity headers"),
                        )
                        .await?;
                        return Ok(());
                    }
                    Ok(Err(ServerError::InvalidHeaders)) => {
                        reply(&mut write, Reply::new(550, "5.6.0 Invalid message headers")).await?;
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
                let operation_id = operation.simple().to_string();
                let plan = Acceptance {
                    operation_id: operation_id.clone(),
                    sender,
                    recipients,
                };
                let identity = match (principal.clone(), author) {
                    (Some(principal), Some(author)) => {
                        Some(SubmissionIdentity { principal, author })
                    }
                    (None, None) => None,
                    _ => return Err(StoreError::PermissionDenied.into()),
                };
                match timeout(
                    Duration::from_secs(60),
                    store.accept(message, plan, identity),
                )
                .await
                {
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
                    Ok(Err(StoreError::PermissionDenied)) => {
                        reply(
                            &mut write,
                            Reply::new(550, "5.7.1 Submission identity denied or revoked"),
                        )
                        .await?;
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

fn discard_read_buffer<S: AsyncRead + Unpin>(
    read: BufReader<tokio::io::ReadHalf<S>>,
    write: tokio::io::WriteHalf<S>,
) -> S {
    read.into_inner().unsplit(write)
}

#[cfg(test)]
mod transport_boundary_tests {
    use super::*;

    #[tokio::test]
    async fn upgrade_drops_prefetched_plaintext_before_reading_new_transport_bytes() {
        let (mut peer, stream) = tokio::io::duplex(256);
        peer.write_all(b"STARTTLS\r\nEHLO injected\r\nMAIL FROM:<evil@remote.test>\r\n")
            .await
            .unwrap();
        let (read, write) = tokio::io::split(stream);
        let mut read = BufReader::with_capacity(256, read);
        assert_eq!(line(&mut read, 512).await.unwrap().unwrap(), b"STARTTLS");
        assert!(read.buffer().starts_with(b"EHLO injected"));
        let stream = discard_read_buffer(read, write);
        peer.write_all(b"fresh transport bytes\r\n").await.unwrap();
        let mut read = BufReader::new(stream);
        assert_eq!(
            line(&mut read, 512).await.unwrap().unwrap(),
            b"fresh transport bytes"
        );
    }
}

async fn receive_message<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    mut stage: StagedMessage,
    config: &Config,
    submission: bool,
    prefix: &str,
) -> Result<(PreparedMessage, Option<rustymail_core::Address>), ServerError> {
    let mut in_headers = true;
    let mut headers = 0usize;
    let mut input_bytes = 0u64;
    let mut filter = trace::HeaderFilter::default();
    let mut identities = SubmissionHeaders::default();
    let mut decoder = LineDecoder::new(1001);
    stage.append(prefix.as_bytes()).await?;
    loop {
        decoder.clear();
        // One extra octet is permitted for SMTP transparency. The decoded line
        // including CRLF is checked against 1000 bytes separately.
        let complete = timeout(
            Duration::from_secs(config.timeouts.data_idle_seconds),
            fill_line(reader, &mut decoder),
        )
        .await
        .map_err(|_| timed_out())??;
        if !complete {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "incomplete DATA").into());
        }
        let frame = decoder
            .frame()
            .ok_or_else(|| io::Error::other("missing DATA frame"))?;
        let data = decode_data_frame(frame)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let Some(data) = data else {
            let author = if submission {
                Some(identities.finish()?)
            } else {
                None
            };
            if in_headers {
                // Empty or header-only input has an empty body. This separator
                // belongs to the bounded server overhead, not client SIZE.
                stage.append(b"\r\n").await?;
            }
            return Ok((stage.prepare().await?, author));
        };
        input_bytes = input_bytes
            .checked_add(data.len() as u64)
            .filter(|size| *size <= config.limits.message_bytes)
            .ok_or(StoreError::SizeLimit)?;
        if in_headers {
            headers = headers
                .checked_add(data.len())
                .ok_or(StoreError::SizeLimit)?;
            if headers > config.limits.header_bytes {
                return Err(StoreError::SizeLimit.into());
            }
            if data == b"\r\n" {
                in_headers = false;
            } else {
                let retain = filter.retain(data)?;
                if submission {
                    identities.line(data)?;
                }
                if !retain {
                    continue;
                }
            }
        }
        stage.append(data).await?;
    }
}

#[derive(Default)]
struct SubmissionHeaders {
    author: Option<rustymail_core::Address>,
    identity_field: bool,
}
impl SubmissionHeaders {
    fn line(&mut self, line: &[u8]) -> Result<(), StoreError> {
        if line.first().is_some_and(|b| b.is_ascii_whitespace()) {
            return if self.identity_field {
                Err(StoreError::PermissionDenied)
            } else {
                Ok(())
            };
        }
        let colon = line
            .iter()
            .position(|&b| b == b':')
            .ok_or(StoreError::PermissionDenied)?;
        let (name, value) = (&line[..colon], &line[colon + 1..]);
        if name.is_empty()
            || name
                .iter()
                .any(|b| !b.is_ascii_alphanumeric() && *b != b'-')
        {
            return Err(StoreError::PermissionDenied);
        }
        self.identity_field = name.eq_ignore_ascii_case(b"From");
        if name.eq_ignore_ascii_case(b"Sender")
            || name
                .get(..7)
                .is_some_and(|name| name.eq_ignore_ascii_case(b"Resent-"))
        {
            return Err(StoreError::PermissionDenied);
        }
        if self.identity_field {
            if self.author.is_some() {
                return Err(StoreError::PermissionDenied);
            }
            let value = std::str::from_utf8(value)
                .map_err(|_| StoreError::PermissionDenied)?
                .trim();
            let mailbox = if let Some((display, mailbox)) = value.rsplit_once('<') {
                if display.contains(['<', '>', ',', ';', ':', '(', ')']) || !display.is_ascii() {
                    return Err(StoreError::PermissionDenied);
                }
                mailbox
                    .strip_suffix('>')
                    .ok_or(StoreError::PermissionDenied)?
            } else {
                value
            };
            self.author = Some(
                rustymail_core::Address::parse(mailbox)
                    .map_err(|_| StoreError::PermissionDenied)?,
            );
        }
        Ok(())
    }
    fn finish(self) -> Result<rustymail_core::Address, StoreError> {
        self.author.ok_or(StoreError::PermissionDenied)
    }
}
