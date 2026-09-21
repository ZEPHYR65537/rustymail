//! One bounded JSON request per local connection. No internet admin listener.
use crate::{auth::AuthService, tls::TlsSettings, worker::StoreClient};
use rustymail_core::config::Config;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{io, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use zeroize::Zeroizing;
#[cfg(unix)]
use {crate::log_event, rustymail_core::Address, serde_json::json};

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminRequest {
    Status,
    CredentialCreate {
        login: String,
        label: String,
        scope: String,
    },
    CredentialList {
        login: String,
        after_id: i64,
        limit: usize,
    },
    CredentialRevoke {
        selector: String,
    },
    AccountDisable {
        login: String,
    },
    SendAs {
        login: String,
        address: String,
        enabled: bool,
    },
    ReloadTls,
}

#[derive(Clone, Copy)]
pub enum FrameKind {
    Request,
    Response,
}

impl FrameKind {
    fn limit(self) -> usize {
        match self {
            Self::Request => 16 * 1024,
            // Covers 100 credentials, 80-byte fully escaped labels and all
            // integer fields at their maximum encoded lengths. Tested below.
            Self::Response => 64 * 1024,
        }
    }
}

pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: R,
    kind: FrameKind,
) -> io::Result<Zeroizing<Vec<u8>>> {
    let limit = kind.limit();
    let mut bytes = Zeroizing::new(Vec::with_capacity(limit + 1));
    BufReader::new(reader.take((limit + 1) as u64))
        .read_until(b'\n', &mut bytes)
        .await?;
    if bytes.len() > limit || bytes.last() != Some(&b'\n') {
        return Err(io::Error::other("invalid management frame"));
    }
    Ok(bytes)
}
struct EncodedFrame {
    bytes: Zeroizing<Vec<u8>>,
    limit: usize,
}

impl io::Write for EncodedFrame {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(io::Error::other("management response exceeds frame limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
    kind: FrameKind,
) -> io::Result<()> {
    let mut frame = EncodedFrame {
        bytes: Zeroizing::new(Vec::with_capacity(kind.limit())),
        limit: kind.limit() - 1,
    };
    // Serialize through a bounded sink; do not allocate an oversized response
    // before discovering it cannot be sent. No partial frame reaches the peer.
    serde_json::to_writer(&mut frame, value)?;
    frame.bytes.push(b'\n');
    writer.write_all(&frame.bytes).await?;
    writer.flush().await
}

#[cfg(unix)]
pub(crate) async fn dispatch(
    request: AdminRequest,
    store: &StoreClient,
    auth: &AuthService,
    tls: &TlsSettings,
    config: &Config,
    mode: &'static str,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    fn address(raw: &str, config: &Config) -> Result<Address, io::Error> {
        let address = Address::parse(raw).map_err(|_| io::Error::other("invalid local address"))?;
        if !config
            .local_domains
            .iter()
            .any(|domain| domain.eq_ignore_ascii_case(address.domain()))
        {
            return Err(io::Error::other("address is not local"));
        }
        Ok(address)
    }
    let (event, result) = match request {
        AdminRequest::Status => (
            "admin_status",
            json!({"version":env!("CARGO_PKG_VERSION"),"mode":mode,"production_ready":false}),
        ),
        AdminRequest::CredentialCreate {
            login,
            label,
            scope,
        } => {
            let login = address(&login, config)?;
            if label.is_empty()
                || label.len() > 80
                || !matches!(scope.as_str(), "mail" | "read_only")
            {
                return Err("invalid credential options".into());
            }
            let (selector, token, phc) = auth.generate().await?;
            let for_store = selector.clone();
            let id = store
                .call(move |store| {
                    store.create_credential(&login, &for_store, &label, &scope, &phc)
                })
                .await?;
            // This response travels only over the private management socket.
            (
                "credential_created",
                json!({"credential_id":id,"selector":selector,"application_password":&*token}),
            )
        }
        AdminRequest::CredentialList {
            login,
            after_id,
            limit,
        } => {
            let login = address(&login, config)?;
            let list = store
                .call(move |store| store.list_credentials(&login, after_id, limit))
                .await?;
            ("credential_listed", json!({"credentials":list}))
        }
        AdminRequest::CredentialRevoke { selector } => {
            let id = store
                .change_authority(auth.changes.clone(), move |store| {
                    store.revoke_credential(&selector)
                })
                .await?;
            ("credential_revoked", json!({"account_id":id}))
        }
        AdminRequest::AccountDisable { login } => {
            let login = address(&login, config)?;
            let id = store
                .change_authority(auth.changes.clone(), move |store| {
                    store.disable_account(&login)
                })
                .await?;
            ("account_disabled", json!({"account_id":id}))
        }
        AdminRequest::SendAs {
            login,
            address: sender,
            enabled,
        } => {
            let login = address(&login, config)?;
            let sender = address(&sender, config)?;
            let id = store
                .change_authority(auth.changes.clone(), move |store| {
                    store.set_send_as(&login, &sender, enabled)
                })
                .await?;
            (
                "send_as_changed",
                json!({"account_id":id,"enabled":enabled}),
            )
        }
        AdminRequest::ReloadTls => {
            let tls = tls.clone();
            tokio::task::spawn_blocking(move || tls.reload()).await??;
            ("tls_reloaded", json!({"reloaded":true}))
        }
    };
    log_event(event, json!({})); // Never serialize response/request into audit logs.
    Ok(result)
}

#[cfg(unix)]
pub(crate) struct AdminListener {
    pub listener: tokio::net::UnixListener,
    path: std::path::PathBuf,
    device: u64,
    inode: u64,
}
#[cfg(not(unix))]
pub(crate) struct AdminListener;
#[cfg(unix)]
type AdminStream = tokio::net::UnixStream;
#[cfg(not(unix))]
type AdminStream = tokio::net::TcpStream;

pub(crate) async fn accept(listener: &Option<AdminListener>) -> io::Result<AdminStream> {
    #[cfg(unix)]
    if let Some(listener) = listener {
        return listener.listener.accept().await.map(|(stream, _)| stream);
    }
    let _ = listener;
    std::future::pending().await
}

#[cfg(not(unix))]
pub(crate) async fn session(
    _stream: AdminStream,
    _store: StoreClient,
    _auth: Arc<AuthService>,
    _tls: TlsSettings,
    _config: Arc<Config>,
    _mode: &'static str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "online administration requires Unix sockets",
    ))
}
#[cfg(unix)]
impl AdminListener {
    pub fn bind(path: &std::path::Path) -> io::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| io::Error::other("admin socket needs a private parent directory"))?;
        if !parent.exists() {
            std::fs::DirBuilder::new().mode(0o700).create(parent)?;
        }
        let metadata = std::fs::symlink_metadata(parent)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(io::Error::other(
                "admin parent must be owned by the daemon and private (0700)",
            ));
        }
        // Refuse stale paths. Never unlink someone else's live socket at startup.
        let listener = tokio::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let metadata = std::fs::symlink_metadata(path)?;
        Ok(Self {
            listener,
            path: path.into(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}
#[cfg(unix)]
impl Drop for AdminListener {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
struct SensitiveResponse(Value);
#[cfg(unix)]
impl Drop for SensitiveResponse {
    fn drop(&mut self) {
        if let Some(Value::String(secret)) = self.0.pointer_mut("/result/application_password") {
            use zeroize::Zeroize;
            secret.zeroize();
        }
    }
}

#[cfg(unix)]
pub(crate) async fn session(
    mut stream: tokio::net::UnixStream,
    store: StoreClient,
    auth: Arc<AuthService>,
    tls: TlsSettings,
    config: Arc<Config>,
    mode: &'static str,
) -> io::Result<()> {
    use std::time::Duration;
    let peer = stream.peer_cred()?;
    let allowed = peer.uid() == 0
        || (peer.uid() == rustix::process::geteuid().as_raw()
            && peer.gid() == rustix::process::getegid().as_raw());
    if !allowed {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "management peer denied",
        ));
    }
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        read_frame(&mut stream, FrameKind::Request),
    )
    .await
    .map_err(|_| io::Error::other("management read timeout"))??;
    let request = serde_json::from_slice::<AdminRequest>(&bytes);
    let response = SensitiveResponse(match request {
        Ok(request) => match dispatch(request, &store, &auth, &tls, &config, mode).await {
            Ok(value) => json!({"ok":true,"result":value}),
            Err(_) => {
                log_event("admin_request_failed", json!({}));
                json!({"ok":false,"error":"management request failed"})
            }
        },
        Err(_) => json!({"ok":false,"error":"invalid management request"}),
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        write_frame(&mut stream, &response.0, FrameKind::Response),
    )
    .await
    .map_err(|_| io::Error::other("management write timeout"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustymail_store::CredentialSummary;
    use serde_json::json;

    #[tokio::test]
    async fn maximum_credential_page_survives_json_escaping_and_transport() {
        let credentials: Vec<_> = (0..100)
            .map(|i| CredentialSummary {
                id: i64::MAX - i,
                selector: "a".repeat(32),
                label: "\"\\".repeat(40),
                scope: "read_only".into(),
                revoked_at_ms: Some(i64::MIN),
                last_used_at_ms: Some(i64::MAX),
            })
            .collect();
        let response = json!({"ok": true, "result": {"credentials": credentials}});
        let (mut write, read) = tokio::io::duplex(FrameKind::Response.limit());
        write_frame(&mut write, &response, FrameKind::Response)
            .await
            .unwrap();
        let bytes = read_frame(read, FrameKind::Response).await.unwrap();
        assert!(bytes.len() > FrameKind::Request.limit());
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), response);
    }

    #[tokio::test]
    async fn oversized_frames_fail_before_writing_and_requests_keep_their_limit() {
        for kind in [FrameKind::Request, FrameKind::Response] {
            let (mut write, mut read) = tokio::io::duplex(64);
            assert!(
                write_frame(&mut write, &json!("x".repeat(kind.limit())), kind)
                    .await
                    .is_err()
            );
            drop(write);
            let mut received = Vec::new();
            read.read_to_end(&mut received).await.unwrap();
            assert!(received.is_empty());
        }
        let oversized = vec![b'x'; FrameKind::Request.limit() + 1];
        assert!(
            read_frame(oversized.as_slice(), FrameKind::Request)
                .await
                .is_err()
        );
        assert!(
            read_frame(b"{}".as_slice(), FrameKind::Request)
                .await
                .is_err()
        );
    }
}
