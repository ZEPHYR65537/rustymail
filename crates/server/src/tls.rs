use rustymail_core::config::Tls;
use std::{
    fs,
    io::{self, Read},
    path::Path,
    sync::{Arc, RwLock},
};
use tokio_rustls::rustls::{
    self, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use zeroize::Zeroizing;

fn material(path: &Path, limit: usize, private: bool) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit as u64 {
        return Err(io::Error::other("invalid TLS material path or size"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if private && metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "TLS private key must not be accessible to group or other users",
            ));
        }
    }
    let _ = private;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::other("TLS material exceeds limit"));
    }
    Ok(bytes)
}

fn load(settings: &Tls) -> io::Result<Arc<ServerConfig>> {
    let cert = material(&settings.certificate_file, 1024 * 1024, false)?;
    let key = Zeroizing::new(material(&settings.private_key_file, 65536, true)?);
    let certs = CertificateDer::pem_slice_iter(&cert)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| io::Error::other("invalid certificate PEM"))?;
    if certs.is_empty() || certs.len() > 16 {
        return Err(io::Error::other(
            "certificate chain must contain 1..16 certificates",
        ));
    }
    let key = PrivateKeyDer::from_pem_slice(&key)
        .map_err(|_| io::Error::other("invalid private key PEM"))?;
    let versions: &[&rustls::SupportedProtocolVersion] = if settings.minimum_version == "1.3" {
        &[&rustls::version::TLS13]
    } else {
        &[&rustls::version::TLS13, &rustls::version::TLS12]
    };
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(versions)
            .map_err(|_| io::Error::other("invalid TLS protocol configuration"))?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|_| {
                io::Error::other("certificate and private key mismatch or unsupported material")
            })?;
    Ok(Arc::new(config))
}

#[derive(Clone)]
pub struct TlsSettings {
    settings: Tls,
    current: Arc<RwLock<Arc<ServerConfig>>>,
}
impl TlsSettings {
    pub fn new(settings: Tls) -> io::Result<Self> {
        Ok(Self {
            current: Arc::new(RwLock::new(load(&settings)?)),
            settings,
        })
    }
    pub fn config(&self) -> io::Result<Arc<ServerConfig>> {
        self.current
            .read()
            .map(|config| config.clone())
            .map_err(|_| io::Error::other("TLS configuration unavailable"))
    }
    pub fn reload(&self) -> io::Result<()> {
        let replacement = load(&self.settings)?;
        *self
            .current
            .write()
            .map_err(|_| io::Error::other("TLS configuration unavailable"))? = replacement;
        Ok(())
    }
}
