//! Strict, versioned configuration. Parsing errors never echo source text.
use crate::valid_domain;
use serde::Deserialize;
use std::{
    collections::HashSet,
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "invalid TOML, unknown field or wrong value type near line {line}; source values are redacted"
    )]
    Parse { line: usize },
    #[error("invalid configuration: {0}")]
    Invalid(&'static str),
}

macro_rules! config_struct {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Clone, Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $(pub $field: $ty),* }
    };
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Lab,
    Production,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    Disabled,
    Relay,
    Direct,
}

config_struct!(Config {
    schema_version: u32, mode: Mode, hostname: String, local_domains: Vec<String>,
    data_dir: PathBuf, admin_socket: PathBuf, metrics_bind: SocketAddr,
    listeners: Listeners, tls: Tls, limits: Limits, timeouts: Timeouts,
    store: Store, authentication: Authentication, parser_worker: ParserWorker,
    spam: Spam, dns: Dns, delivery: Delivery, relay: Relay, direct: Direct,
    dkim: Dkim, logging: Logging,
});
config_struct!(Listeners {
    smtp: SocketAddr,
    submissions: SocketAddr,
    submission: SocketAddr,
    imaps: SocketAddr
});
config_struct!(Tls {
    certificate_file: PathBuf,
    private_key_file: PathBuf,
    minimum_version: String,
    handshake_timeout_seconds: u64
});
config_struct!(Limits {
    connections: usize,
    connections_per_ip: usize,
    imap_sessions_per_account: usize,
    tls_handshakes: usize,
    message_bytes: u64,
    header_bytes: usize,
    recipients_per_message: usize,
    ingest_concurrency: usize,
    stream_buffer_bytes: usize,
    selected_view_total_bytes: usize,
    selected_mailbox_messages: usize,
    imap_event_bytes_per_session: usize,
    disk_reserve_bytes: u64,
    disk_reserve_percent: u8,
    temporary_reserved_bytes: u64,
});
config_struct!(Timeouts {
    smtp_command_seconds: u64,
    data_idle_seconds: u64,
    data_total_seconds: u64,
    submission_unauthenticated_seconds: u64,
    imap_authenticated_idle_seconds: u64,
    append_idle_seconds: u64,
    append_total_seconds: u64,
    shutdown_grace_seconds: u64,
});
config_struct!(Store {
    journal_mode: String,
    synchronous: String,
    writer_queue: usize,
    reader_threads: usize,
    writer_cache_kib: u32,
    reader_cache_kib_each: u32,
    gc_mode: String
});
config_struct!(Authentication {
    argon2_memory_kib: u32,
    argon2_iterations: u32,
    argon2_lanes: u32,
    concurrency: usize,
    waiting_requests: usize,
    queue_timeout_seconds: u64
});
config_struct!(ParserWorker {
    socket: PathBuf,
    concurrency: usize,
    memory_limit_bytes: u64,
    timeout_seconds: u64,
    mime_max_depth: usize,
    mime_max_parts: usize,
    metadata_max_bytes: usize
});
config_struct!(Spam {
    required: bool,
    endpoint: String,
    timeout_seconds: u64,
    concurrency: usize,
    response_max_bytes: usize,
    on_error: String
});
config_struct!(Dns {
    concurrency: usize,
    timeout_seconds: u64,
    cache_entries: usize,
    cache_bytes: usize
});
config_struct!(Delivery {
    mode: DeliveryMode, concurrency: usize, per_domain_concurrency: usize,
    batch_size: usize, retry_seconds: Vec<u64>, retry_jitter_percent: u8,
    max_age_seconds: u64, connect_timeout_seconds: u64, banner_timeout_seconds: u64,
    command_timeout_seconds: u64, data_command_timeout_seconds: u64,
    data_write_timeout_seconds: u64, final_reply_timeout_seconds: u64,
});
config_struct!(Relay {
    host: String,
    port: u16,
    tls: String,
    username: String,
    password_file: PathBuf,
    ca_file: Option<PathBuf>
});
config_struct!(Direct { mta_sts: bool, tls_policy: String, plaintext_exception_domains: Vec<String>, allow_private_mx_addresses: bool });
config_struct!(Dkim {
    domain: String,
    selector: String,
    private_key_file: PathBuf
});
config_struct!(Logging {
    level: String,
    format: String,
    include_message_bodies: bool,
    include_auth_payloads: bool
});

fn require(condition: bool, reason: &'static str) -> Result<(), ConfigError> {
    if condition {
        Ok(())
    } else {
        Err(ConfigError::Invalid(reason))
    }
}

fn placeholder(domain: &str) -> bool {
    [
        "example.com",
        "example.net",
        "example.org",
        "example",
        "invalid",
        "localhost",
        "test",
    ]
    .iter()
    .any(|suffix| {
        domain.eq_ignore_ascii_case(suffix)
            || domain.to_ascii_lowercase().ends_with(&format!(".{suffix}"))
    })
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let mut text = String::new();
        std::fs::File::open(path)?
            .take(1024 * 1024 + 1)
            .read_to_string(&mut text)?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        require(text.len() <= 1024 * 1024, "configuration exceeds 1 MiB")?;
        let config: Self = toml::from_str(text).map_err(|error: toml::de::Error| {
            let start = error.span().map_or(0, |range| range.start.min(text.len()));
            ConfigError::Parse {
                line: text.as_bytes()[..start]
                    .iter()
                    .filter(|&&b| b == b'\n')
                    .count()
                    + 1,
            }
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        require(self.schema_version == 1, "unsupported schema_version")?;
        require(valid_domain(&self.hostname), "hostname must be a DNS name")?;
        require(
            !self.local_domains.is_empty() && self.local_domains.len() <= 100,
            "local_domains must contain 1..100 domains",
        )?;
        let domains: HashSet<_> = self
            .local_domains
            .iter()
            .map(|d| d.to_ascii_lowercase())
            .collect();
        require(
            domains.len() == self.local_domains.len() && domains.iter().all(|d| valid_domain(d)),
            "local_domains contains duplicates or invalid names",
        )?;
        for path in [
            &self.data_dir,
            &self.admin_socket,
            &self.tls.certificate_file,
            &self.tls.private_key_file,
            &self.parser_worker.socket,
            &self.relay.password_file,
            &self.dkim.private_key_file,
        ] {
            require(
                !path.as_os_str().is_empty(),
                "configured paths must not be empty",
            )?;
        }
        let listeners = [
            self.listeners.smtp,
            self.listeners.submissions,
            self.listeners.submission,
            self.listeners.imaps,
            self.metrics_bind,
        ];
        for (i, address) in listeners.iter().enumerate() {
            require(address.port() != 0, "listener port must not be zero")?;
            for other in listeners.iter().skip(i + 1) {
                let overlaps = address.port() == other.port()
                    && (address.ip() == other.ip()
                        || address.ip().is_unspecified()
                        || other.ip().is_unspecified());
                require(!overlaps, "listener addresses overlap")?;
            }
        }
        require(
            matches!(self.tls.minimum_version.as_str(), "1.2" | "1.3"),
            "tls.minimum_version must be 1.2 or 1.3",
        )?;
        let l = &self.limits;
        require(
            (1..=4096).contains(&l.connections),
            "limits.connections must be 1..4096",
        )?;
        for count in [
            l.connections_per_ip,
            l.imap_sessions_per_account,
            l.tls_handshakes,
            l.ingest_concurrency,
        ] {
            require(
                count > 0 && count <= l.connections,
                "connection sub-limits must be positive and <= connections",
            )?;
        }
        require(
            (1024..=25 * 1024 * 1024).contains(&l.message_bytes),
            "limits.message_bytes must be 1 KiB..25 MiB",
        )?;
        require(
            l.header_bytes >= 1024 && l.header_bytes as u64 <= l.message_bytes,
            "limits.header_bytes must be 1 KiB..message_bytes",
        )?;
        require(
            (1..=100).contains(&l.recipients_per_message),
            "limits.recipients_per_message must be 1..100",
        )?;
        require(
            (1024..=64 * 1024).contains(&l.stream_buffer_bytes),
            "limits.stream_buffer_bytes must be 1..64 KiB",
        )?;
        require(
            l.selected_mailbox_messages > 0 && l.selected_mailbox_messages <= 100_000,
            "selected_mailbox_messages must be 1..100000",
        )?;
        require(
            (4 * l.selected_mailbox_messages..=128 * 1024 * 1024)
                .contains(&l.selected_view_total_bytes),
            "selected_view_total_bytes cannot fit the selected mailbox or exceeds 128 MiB",
        )?;
        require(
            (1024..=1024 * 1024).contains(&l.imap_event_bytes_per_session),
            "imap_event_bytes_per_session must be 1 KiB..1 MiB",
        )?;
        require(
            l.disk_reserve_percent > 0 && l.disk_reserve_percent < 100,
            "disk_reserve_percent must be 1..99",
        )?;
        require(
            l.disk_reserve_bytes > 0
                && l.temporary_reserved_bytes
                    >= l.message_bytes + crate::LOCAL_DELIVERY_OVERHEAD_BYTES
                && l.temporary_reserved_bytes <= i64::MAX as u64,
            "invalid disk reservation limits",
        )?;
        let t = &self.timeouts;
        let durations = [
            t.smtp_command_seconds,
            t.data_idle_seconds,
            t.data_total_seconds,
            t.submission_unauthenticated_seconds,
            t.imap_authenticated_idle_seconds,
            t.append_idle_seconds,
            t.append_total_seconds,
            t.shutdown_grace_seconds,
            self.tls.handshake_timeout_seconds,
            self.authentication.queue_timeout_seconds,
            self.parser_worker.timeout_seconds,
            self.spam.timeout_seconds,
            self.dns.timeout_seconds,
            self.delivery.connect_timeout_seconds,
            self.delivery.banner_timeout_seconds,
            self.delivery.command_timeout_seconds,
            self.delivery.data_command_timeout_seconds,
            self.delivery.data_write_timeout_seconds,
            self.delivery.final_reply_timeout_seconds,
        ];
        require(
            durations.iter().all(|&v| (1..=86400).contains(&v)),
            "timeouts must be 1..86400 seconds",
        )?;
        require(
            t.data_total_seconds >= t.data_idle_seconds
                && t.append_total_seconds >= t.append_idle_seconds,
            "total transfer deadline cannot be shorter than idle deadline",
        )?;
        require(
            self.store.journal_mode == "wal" && self.store.synchronous == "full",
            "store requires WAL and FULL; durability cannot be disabled",
        )?;
        require(
            self.store.gc_mode == "offline",
            "only offline GC is designed for this version",
        )?;
        require(
            (1..=1024).contains(&self.store.writer_queue)
                && (1..=8).contains(&self.store.reader_threads),
            "invalid database queue/thread budget",
        )?;
        require(
            (128..=65536).contains(&self.store.writer_cache_kib)
                && (128..=65536).contains(&self.store.reader_cache_kib_each),
            "database cache must be 128..65536 KiB per connection",
        )?;
        let a = &self.authentication;
        require(
            (65536..=1048576).contains(&a.argon2_memory_kib)
                && (3..=10).contains(&a.argon2_iterations)
                && (1..=16).contains(&a.argon2_lanes),
            "invalid Argon2 memory/iteration/lane budget",
        )?;
        require(
            (1..=2).contains(&a.concurrency) && (1..=64).contains(&a.waiting_requests),
            "invalid authentication concurrency/queue budget",
        )?;
        require(
            u64::from(a.argon2_memory_kib) * a.concurrency as u64 <= 256 * 1024,
            "combined Argon2 working memory must not exceed 256 MiB in this release",
        )?;
        require(
            (1..=4).contains(&self.parser_worker.concurrency)
                && (32 * 1024 * 1024..=1024 * 1024 * 1024)
                    .contains(&self.parser_worker.memory_limit_bytes)
                && (1..=32).contains(&self.parser_worker.mime_max_depth)
                && (1..=5000).contains(&self.parser_worker.mime_max_parts)
                && (1024..=1024 * 1024).contains(&self.parser_worker.metadata_max_bytes),
            "invalid MIME worker limits",
        )?;
        require(
            self.spam.on_error == "temporary_failure",
            "spam.on_error must be temporary_failure",
        )?;
        require(
            self.spam.endpoint.starts_with("http://127.0.0.1:")
                && self.spam.endpoint.ends_with("/checkv2")
                && !self.spam.endpoint.contains(['\r', '\n', '@']),
            "spam.endpoint must be a loopback checkv2 endpoint",
        )?;
        require(
            (1..=16).contains(&self.spam.concurrency)
                && (1024..=1024 * 1024).contains(&self.spam.response_max_bytes),
            "invalid spam scanner budgets",
        )?;
        require(
            (1..=128).contains(&self.dns.concurrency)
                && (1..=65536).contains(&self.dns.cache_entries)
                && (1024..=64 * 1024 * 1024).contains(&self.dns.cache_bytes),
            "invalid DNS budgets",
        )?;
        let d = &self.delivery;
        require(
            (1..=128).contains(&d.concurrency)
                && d.per_domain_concurrency > 0
                && d.per_domain_concurrency <= d.concurrency
                && (1..=128).contains(&d.batch_size),
            "invalid delivery concurrency/batch",
        )?;
        require(
            !d.retry_seconds.is_empty()
                && d.retry_seconds.len() <= 32
                && d.retry_seconds.iter().all(|&v| (1800..=86400).contains(&v))
                && d.retry_seconds.windows(2).all(|pair| pair[0] <= pair[1]),
            "retry_seconds must be a nondecreasing schedule of 1800..86400 seconds",
        )?;
        require(
            d.retry_jitter_percent <= 50 && (432000..=604800).contains(&d.max_age_seconds),
            "invalid delivery jitter/max_age",
        )?;
        require(
            valid_domain(&self.relay.host)
                && self.relay.port != 0
                && matches!(self.relay.tls.as_str(), "implicit" | "starttls"),
            "invalid relay host/port/TLS mode",
        )?;
        require(
            self.direct.mta_sts
                && self.direct.tls_policy == "verified_or_defer"
                && !self.direct.allow_private_mx_addresses,
            "direct delivery requires MTA-STS, verified_or_defer and public MX addresses",
        )?;
        require(
            self.direct
                .plaintext_exception_domains
                .iter()
                .all(|d| valid_domain(d)),
            "invalid TLS exception domain",
        )?;
        require(
            valid_domain(&self.dkim.domain) && valid_domain(&self.dkim.selector),
            "invalid DKIM domain/selector",
        )?;
        require(
            matches!(
                self.logging.level.as_str(),
                "error" | "warn" | "info" | "debug"
            ) && self.logging.format == "json",
            "invalid logging level/format",
        )?;
        require(
            !self.logging.include_message_bodies && !self.logging.include_auth_payloads,
            "sensitive payload logging is prohibited",
        )?;
        if self.mode == Mode::Lab {
            require(
                listeners.iter().all(|a| a.ip().is_loopback()),
                "lab listeners must bind loopback",
            )?;
        } else {
            require(
                !placeholder(&self.hostname) && domains.iter().all(|d| !placeholder(d)),
                "production domains must not be placeholders",
            )?;
            require(
                self.spam.required && self.delivery.mode != DeliveryMode::Disabled,
                "production requires scanning and a delivery mode",
            )?;
            require(
                self.data_dir.is_absolute(),
                "production data_dir must be absolute",
            )?;
            require(
                self.metrics_bind.ip().is_loopback(),
                "metrics must bind loopback",
            )?;
            if d.mode == DeliveryMode::Relay {
                require(
                    !placeholder(&self.relay.host)
                        && self.relay.username != "REPLACE_ME"
                        && !self.relay.username.is_empty(),
                    "replace production relay placeholders",
                )?;
            }
        }
        Ok(())
    }

    /// Lab support is intentionally narrower than the future configuration.
    pub fn require_lab_receiver(&self) -> Result<(), ConfigError> {
        self.require_lab_storage()?;
        require(
            self.delivery.mode == DeliveryMode::Disabled,
            "use serve-lab-relay to enable fixed-upstream delivery",
        )
    }

    pub fn require_lab_relay(&self) -> Result<(), ConfigError> {
        self.require_lab_storage()?;
        require(
            self.delivery.mode == DeliveryMode::Relay && self.relay.ca_file.is_some(),
            "relay laboratory requires delivery.mode=relay and relay.ca_file",
        )
    }

    pub fn require_lab_storage(&self) -> Result<(), ConfigError> {
        self.validate()?;
        require(
            self.mode == Mode::Lab && self.listeners.smtp.ip().is_loopback(),
            "only lab mode on loopback can run in this release",
        )?;
        require(
            self.delivery.mode != DeliveryMode::Direct,
            "direct delivery is not implemented",
        )?;
        require(
            !self.spam.required,
            "scanning is not implemented; explicitly disable it only in a lab config",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const EXAMPLE: &str = include_str!("../../../deploy/rustymail.example.toml");

    #[test]
    fn parses_full_design_but_requires_explicit_lab_scanning_bypass() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert!(config.require_lab_receiver().is_err());
        Config::parse(&EXAMPLE.replace("required = true", "required = false"))
            .unwrap()
            .require_lab_receiver()
            .unwrap();
        let mut relay =
            Config::parse(include_str!("../../../deploy/rustymail.relay-lab.toml")).unwrap();
        relay.require_lab_relay().unwrap();
        assert!(relay.require_lab_receiver().is_err());
        relay.relay.ca_file = None;
        assert!(relay.require_lab_relay().is_err());
    }

    #[test]
    fn rejects_unknown_fields_and_redacts_bad_values() {
        let secret = "secret-that-must-not-appear";
        let bad = EXAMPLE.replace(
            "schema_version = 1",
            &format!("schema_version = '{secret}'"),
        );
        let error = Config::parse(&bad).unwrap_err().to_string();
        assert!(!error.contains(secret));
        assert!(Config::parse(&format!("unknown = true\n{EXAMPLE}")).is_err());
        assert!(
            Config::parse(&EXAMPLE.replace("connections = 256", "connections_typo = 256")).is_err()
        );
    }

    #[test]
    fn rejects_unsafe_or_unbounded_settings() {
        for (old, new) in [
            ("127.0.0.1:2525", "0.0.0.0:2525"),
            ("synchronous = \"full\"", "synchronous = \"off\""),
            ("ingest_concurrency = 16", "ingest_concurrency = 0"),
            ("header_bytes = 262144", "header_bytes = 30000000"),
            (
                "temporary_reserved_bytes = 419430400",
                "temporary_reserved_bytes = 26214400",
            ),
            ("smtp = \"127.0.0.1:2525\"", "smtp = \"127.0.0.1:2465\""),
            (
                "include_auth_payloads = false",
                "include_auth_payloads = true",
            ),
            ("mode = \"lab\"", "mode = \"production\""),
            ("argon2_memory_kib = 65536", "argon2_memory_kib = 1048576"),
        ] {
            assert!(Config::parse(&EXAMPLE.replace(old, new)).is_err(), "{new}");
        }
        assert!(
            Config::parse(
                &EXAMPLE.replace("argon2_memory_kib = 65536", "argon2_memory_kib = 131072")
            )
            .is_ok()
        );
        assert!(
            Config::parse(
                &EXAMPLE.replace("argon2_memory_kib = 65536", "argon2_memory_kib = 131073")
            )
            .is_err()
        );
    }
}
