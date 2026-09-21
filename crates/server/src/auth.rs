//! Bounded password work. Permits move into blocking jobs and survive cancellation.
use crate::worker::StoreClient;
use argon2::{
    Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version,
    password_hash::SaltString,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustymail_core::{Address, config::Authentication};
use rustymail_store::Principal;
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    time::timeout,
};
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("authentication failed")]
    Denied,
    #[error("authentication temporarily unavailable")]
    Busy,
}

pub struct PlainCredentials {
    pub login: Address,
    pub token: Zeroizing<String>,
}

pub fn decode_plain(encoded: &[u8]) -> Result<PlainCredentials, AuthError> {
    if encoded.len() > 1024 {
        return Err(AuthError::Denied);
    }
    let raw = Zeroizing::new(STANDARD.decode(encoded).map_err(|_| AuthError::Denied)?);
    let fields: Vec<_> = raw.split(|&b| b == 0).collect();
    if fields.len() != 3 || fields[1].len() > 254 || fields[2].len() > 128 {
        return Err(AuthError::Denied);
    }
    let login = Address::parse(std::str::from_utf8(fields[1]).map_err(|_| AuthError::Denied)?)
        .map_err(|_| AuthError::Denied)?;
    if !fields[0].is_empty() {
        let authz = Address::parse(std::str::from_utf8(fields[0]).map_err(|_| AuthError::Denied)?)
            .map_err(|_| AuthError::Denied)?;
        if authz.local_key() != login.local_key() {
            return Err(AuthError::Denied);
        }
    }
    Ok(PlainCredentials {
        login,
        token: Zeroizing::new(
            std::str::from_utf8(fields[2])
                .map_err(|_| AuthError::Denied)?
                .to_owned(),
        ),
    })
}

struct RateEntry {
    started: Instant,
    attempts: u32,
}
struct Rates {
    ips: HashMap<IpAddr, RateEntry>,
    accounts: HashMap<String, RateEntry>,
}
pub struct AuthService {
    settings: Authentication,
    active: Arc<Semaphore>,
    admission: Arc<Semaphore>,
    dummy: String,
    rates: Mutex<Rates>,
    pub changes: watch::Sender<u64>,
}

fn random_hex(bytes: usize) -> Result<Zeroizing<String>, AuthError> {
    let mut raw = Zeroizing::new(vec![0; bytes]);
    getrandom::fill(&mut raw).map_err(|_| AuthError::Busy)?;
    Ok(Zeroizing::new(
        raw.iter().map(|b| format!("{b:02x}")).collect(),
    ))
}

pub fn hash_token(token: &str, settings: &Authentication) -> Result<String, AuthError> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|_| AuthError::Busy)?;
    let salt = SaltString::encode_b64(&salt).map_err(|_| AuthError::Busy)?;
    let params = Params::new(
        settings.argon2_memory_kib,
        settings.argon2_iterations,
        settings.argon2_lanes,
        Some(32),
    )
    .map_err(|_| AuthError::Busy)?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(token.as_bytes(), &salt)
        .map(|phc| phc.to_string())
        .map_err(|_| AuthError::Busy)
}

pub fn generate_credential(
    settings: &Authentication,
) -> Result<(String, Zeroizing<String>, String), AuthError> {
    let selector = random_hex(16)?.to_string();
    let token = Zeroizing::new(format!("{}.{}", selector, *random_hex(32)?));
    let phc = hash_token(&token, settings)?;
    Ok((selector, token, phc))
}

fn verify_token(token: &str, phc: &str, settings: &Authentication) -> bool {
    if phc.len() > 512 {
        return false;
    }
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    let Ok(params) = Params::try_from(&parsed) else {
        return false;
    };
    // PHC parameters override the verifier instance: validate before allocating.
    if parsed.algorithm.as_str() != "argon2id"
        || parsed.version != Some(19)
        || !(65536..=settings.argon2_memory_kib).contains(&params.m_cost())
        || !(3..=settings.argon2_iterations).contains(&params.t_cost())
        || !(1..=settings.argon2_lanes).contains(&params.p_cost())
        || parsed.hash.as_ref().is_none_or(|h| h.len() != 32)
    {
        return false;
    }
    Argon2::default()
        .verify_password(token.as_bytes(), &parsed)
        .is_ok()
}

impl AuthService {
    pub async fn new(settings: Authentication) -> Result<Arc<Self>, AuthError> {
        let for_hash = settings.clone();
        let dummy = tokio::task::spawn_blocking(move || hash_token(&random_hex(32)?, &for_hash))
            .await
            .map_err(|_| AuthError::Busy)??;
        let (changes, _) = watch::channel(0);
        Ok(Arc::new(Self {
            active: Arc::new(Semaphore::new(settings.concurrency)),
            admission: Arc::new(Semaphore::new(
                settings.concurrency + settings.waiting_requests,
            )),
            settings,
            dummy,
            rates: Mutex::new(Rates {
                ips: HashMap::new(),
                accounts: HashMap::new(),
            }),
            changes,
        }))
    }

    fn rate(&self, ip: IpAddr, account: &str) -> Result<(), AuthError> {
        let now = Instant::now();
        let mut rates = self.rates.lock().map_err(|_| AuthError::Busy)?;
        rates
            .ips
            .retain(|_, v| now.duration_since(v.started) < Duration::from_secs(60));
        rates
            .accounts
            .retain(|_, v| now.duration_since(v.started) < Duration::from_secs(60));
        if (!rates.ips.contains_key(&ip) && rates.ips.len() >= 4096)
            || (!rates.accounts.contains_key(account) && rates.accounts.len() >= 4096)
        {
            return Err(AuthError::Busy);
        }
        let ip_allowed = {
            let entry = rates.ips.entry(ip).or_insert(RateEntry {
                started: now,
                attempts: 0,
            });
            entry.attempts = entry.attempts.saturating_add(1);
            entry.attempts <= 30
        };
        let entry = rates.accounts.entry(account.into()).or_insert(RateEntry {
            started: now,
            attempts: 0,
        });
        entry.attempts = entry.attempts.saturating_add(1);
        if !ip_allowed || entry.attempts > 10 {
            return Err(AuthError::Busy);
        }
        Ok(())
    }

    async fn permits(&self) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), AuthError> {
        let admission = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| AuthError::Busy)?;
        let active = timeout(
            Duration::from_secs(self.settings.queue_timeout_seconds),
            self.active.clone().acquire_owned(),
        )
        .await
        .map_err(|_| AuthError::Busy)?
        .map_err(|_| AuthError::Busy)?;
        Ok((admission, active))
    }

    pub(crate) async fn authenticate(
        &self,
        store: &StoreClient,
        ip: IpAddr,
        credentials: PlainCredentials,
    ) -> Result<Principal, AuthError> {
        self.rate(ip, &credentials.login.local_key())?;
        let permits = self.permits().await?;
        let selector = credentials
            .token
            .split_once('.')
            .map_or("", |(selector, _)| selector)
            .to_owned();
        let record = store
            .call(move |store| store.credential_lookup(&credentials.login, &selector))
            .await
            .map_err(|_| AuthError::Busy)?;
        let principal = record.as_ref().map(|r| r.principal.clone());
        let phc = record.map_or_else(|| self.dummy.clone(), |r| r.password_phc);
        let settings = self.settings.clone();
        let valid = tokio::task::spawn_blocking(move || {
            let _permits = permits;
            verify_token(&credentials.token, &phc, &settings)
        })
        .await
        .map_err(|_| AuthError::Busy)?;
        let principal = principal.filter(|_| valid).ok_or(AuthError::Denied)?;
        let check = principal.clone();
        store
            .call(move |store| store.finish_authentication(&check))
            .await
            .map_err(|_| AuthError::Denied)?;
        Ok(principal)
    }

    pub async fn generate(&self) -> Result<(String, Zeroizing<String>, String), AuthError> {
        let permits = self.permits().await?;
        let settings = self.settings.clone();
        tokio::task::spawn_blocking(move || {
            let _permits = permits;
            generate_credential(&settings)
        })
        .await
        .map_err(|_| AuthError::Busy)?
    }

    pub fn notify_change(&self) {
        self.changes
            .send_modify(|version| *version = version.wrapping_add(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> Authentication {
        rustymail_core::config::Config::parse(include_str!("../../../deploy/rustymail.lab.toml"))
            .unwrap()
            .authentication
    }

    #[test]
    fn plain_rejects_identity_substitution_and_malformed_frames() {
        let good = STANDARD.encode(b"\0alice@example.com\0secret");
        assert_eq!(
            decode_plain(good.as_bytes()).unwrap().login.local_key(),
            "alice@example.com"
        );
        for bad in [
            b"bob@example.com\0alice@example.com\0secret".as_slice(),
            b"\0alice@example.com\0secret\0extra",
            b"\0invalid\0secret",
        ] {
            assert!(decode_plain(STANDARD.encode(bad).as_bytes()).is_err());
        }
        assert!(decode_plain(b"!!!!").is_err());
    }

    #[test]
    fn phc_work_parameters_are_bounded_before_verification() {
        let mut settings = settings();
        settings.argon2_iterations = 4;
        let phc = hash_token("test-only", &settings).unwrap();
        assert!(verify_token("test-only", &phc, &settings));
        assert!(!verify_token("wrong", &phc, &settings));
        settings.argon2_iterations = 3;
        assert!(!verify_token("test-only", &phc, &settings));
    }

    #[tokio::test]
    async fn cancelled_caller_does_not_release_a_running_hash_budget() {
        let mut settings = settings();
        settings.concurrency = 1;
        settings.waiting_requests = 1;
        let service = AuthService::new(settings).await.unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let worker = service.clone();
        let task = tokio::spawn(async move {
            let permits = worker.permits().await.unwrap();
            tokio::task::spawn_blocking(move || {
                let _permits = permits;
                started_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
            })
            .await
            .unwrap();
        });
        started_rx.await.unwrap();
        task.abort();
        let _ = task.await;
        assert_eq!(service.active.available_permits(), 0);
        assert!(
            timeout(Duration::from_millis(30), service.permits())
                .await
                .is_err()
        );
        finish_tx.send(()).unwrap();
        let _permits = timeout(Duration::from_secs(5), service.permits())
            .await
            .unwrap()
            .unwrap();
    }
}
