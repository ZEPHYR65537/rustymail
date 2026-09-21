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

pub async fn read_frame<R: AsyncRead + Unpin>(reader: R) -> io::Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    BufReader::new(reader.take(16385))
        .read_until(b'\n', &mut bytes)
        .await?;
    if bytes.len() > 16384 || bytes.last() != Some(&b'\n') {
        return Err(io::Error::other("invalid management frame"));
    }
    Ok(bytes)
}
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> io::Result<()> {
    let mut bytes = Zeroizing::new(serde_json::to_vec(value)?);
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[cfg(unix)]
pub(crate) async fn dispatch(
    request: AdminRequest,
    store: &StoreClient,
    auth: &AuthService,
    tls: &TlsSettings,
    config: &Config,
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
            json!({"version":env!("CARGO_PKG_VERSION"),"mode":"lab_tls","production_ready":false}),
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
                .call(move |store| store.revoke_credential(&selector))
                .await?;
            auth.notify_change();
            ("credential_revoked", json!({"account_id":id}))
        }
        AdminRequest::AccountDisable { login } => {
            let login = address(&login, config)?;
            let id = store
                .call(move |store| store.disable_account(&login))
                .await?;
            auth.notify_change();
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
                .call(move |store| store.set_send_as(&login, &sender, enabled))
                .await?;
            auth.notify_change();
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
pub(crate) async fn session(
    mut stream: tokio::net::UnixStream,
    store: StoreClient,
    auth: Arc<AuthService>,
    tls: TlsSettings,
    config: Arc<Config>,
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
    let bytes = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream))
        .await
        .map_err(|_| io::Error::other("management read timeout"))??;
    let request = serde_json::from_slice::<AdminRequest>(&bytes);
    let mut response = match request {
        Ok(request) => match dispatch(request, &store, &auth, &tls, &config).await {
            Ok(value) => json!({"ok":true,"result":value}),
            Err(_) => {
                log_event("admin_request_failed", json!({}));
                json!({"ok":false,"error":"management request failed"})
            }
        },
        Err(_) => json!({"ok":false,"error":"invalid management request"}),
    };
    let result = tokio::time::timeout(Duration::from_secs(5), write_frame(&mut stream, &response))
        .await
        .map_err(|_| io::Error::other("management write timeout"))?;
    if let Some(Value::String(secret)) = response.pointer_mut("/result/application_password") {
        use zeroize::Zeroize;
        secret.zeroize();
    }
    result
}
