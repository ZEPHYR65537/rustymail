use rustymail_core::{Address, valid_domain};
use serde::Deserialize;
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Security {
    Implicit,
    Starttls,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Timeouts {
    pub connect_timeout_seconds: u64,
    pub banner_timeout_seconds: u64,
    pub command_timeout_seconds: u64,
    pub data_command_timeout_seconds: u64,
    pub data_write_timeout_seconds: u64,
    pub final_reply_timeout_seconds: u64,
    pub handshake_timeout_seconds: u64,
    pub data_total_seconds: u64,
}
impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect_timeout_seconds: 30,
            banner_timeout_seconds: 60,
            command_timeout_seconds: 60,
            data_command_timeout_seconds: 120,
            data_write_timeout_seconds: 60,
            final_reply_timeout_seconds: 600,
            handshake_timeout_seconds: 15,
            data_total_seconds: 600,
        }
    }
}
fn tls_minimum() -> String {
    "1.2".into()
}
fn buffer_bytes() -> usize {
    16384
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub ehlo: String,
    pub ca_file: PathBuf,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password_file: PathBuf,
    #[serde(default = "tls_minimum")]
    pub minimum_tls_version: String,
    #[serde(default = "buffer_bytes")]
    pub buffer_bytes: usize,
    #[serde(default)]
    pub timeouts: Timeouts,
}
impl SmtpSettings {
    pub fn validate(&self) -> io::Result<()> {
        let t = &self.timeouts;
        if !valid_domain(&self.host)
            || !valid_domain(&self.ehlo)
            || self.port == 0
            || self.ca_file.as_os_str().is_empty()
            || !matches!(self.minimum_tls_version.as_str(), "1.2" | "1.3")
            || !(1024..=65536).contains(&self.buffer_bytes)
            || self.username.len() > 256
            || self.username.bytes().any(|b| b.is_ascii_control())
            || (self.username.is_empty() != self.password_file.as_os_str().is_empty())
            || [
                t.connect_timeout_seconds,
                t.banner_timeout_seconds,
                t.command_timeout_seconds,
                t.data_command_timeout_seconds,
                t.data_write_timeout_seconds,
                t.final_reply_timeout_seconds,
                t.handshake_timeout_seconds,
                t.data_total_seconds,
            ]
            .iter()
            .any(|v| !(1..=86400).contains(v))
        {
            return Err(io::Error::other("invalid SMTP settings or resource bounds"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct InputLimits {
    pub message_bytes: u64,
    pub header_bytes: usize,
}
impl Default for InputLimits {
    fn default() -> Self {
        Self {
            message_bytes: 25 * 1024 * 1024,
            header_bytes: 65536,
        }
    }
}
impl InputLimits {
    pub fn validate(&self) -> io::Result<()> {
        if !(1024..=25 * 1024 * 1024).contains(&self.message_bytes)
            || self.header_bytes == 0
            || self.header_bytes as u64 > self.message_bytes
        {
            return Err(io::Error::other("invalid message input bounds"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub sender: String,
    pub smtp: SmtpSettings,
    #[serde(default)]
    pub limits: InputLimits,
}
impl ClientConfig {
    pub fn parse(text: &str) -> io::Result<Self> {
        if text.len() > 65536 {
            return Err(io::Error::other("client configuration exceeds 64 KiB"));
        }
        let config: Self = toml::from_str(text).map_err(|_| {
            io::Error::other("invalid client TOML, unknown field or wrong type; values redacted")
        })?;
        if Address::parse(&config.sender).is_err() {
            return Err(io::Error::other("invalid envelope sender"));
        }
        config.smtp.validate()?;
        config.limits.validate()?;
        Ok(config)
    }
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut text = String::new();
        std::fs::File::open(path)?
            .take(65537)
            .read_to_string(&mut text)?;
        let mut config = Self::parse(&text)?;
        let parent = path.parent().unwrap_or(Path::new("."));
        if config.smtp.ca_file.is_relative() {
            config.smtp.ca_file = parent.join(&config.smtp.ca_file);
        }
        if !config.smtp.password_file.as_os_str().is_empty()
            && config.smtp.password_file.is_relative()
        {
            config.smtp.password_file = parent.join(&config.smtp.password_file);
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const EXAMPLE: &str = include_str!("../../../deploy/rustymail.client.example.toml");
    #[test]
    fn schema_rejects_unknown_plaintext_and_unbounded_settings_without_echoing_values() {
        assert!(ClientConfig::parse(EXAMPLE).is_ok());
        for input in [
            EXAMPLE.replace("sender = \"alice@example.com\"", "sender = \"invalid\""),
            EXAMPLE.replace("starttls", "plaintext"),
            EXAMPLE.replace("port = 587", "port = 0"),
            EXAMPLE.replace("26214400", "26214401"),
            format!("password = 'SECRET-SENTINEL'\n{EXAMPLE}"),
        ] {
            let error = ClientConfig::parse(&input).unwrap_err().to_string();
            assert!(!error.contains("SECRET-SENTINEL"));
        }
        assert!(ClientConfig::parse(&"x".repeat(65537)).is_err());
    }
    #[test]
    fn material_paths_are_relative_to_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("client.toml");
        std::fs::write(
            &config,
            EXAMPLE.replace("/etc/ssl/certs/ca-certificates.crt", "ca.pem"),
        )
        .unwrap();
        let loaded = ClientConfig::load(&config).unwrap();
        assert_eq!(loaded.smtp.ca_file, directory.path().join("ca.pem"));
        assert_eq!(
            loaded.smtp.password_file,
            directory.path().join("secrets/alice.secret")
        );
    }
}
