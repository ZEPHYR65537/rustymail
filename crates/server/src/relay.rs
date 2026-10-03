//! Queue orchestration around the shared SMTP transport.
use crate::worker::StoreClient;
use rustymail_client::{
    config::{Security, SmtpSettings, Timeouts},
    smtp::{AttemptResult, Message, SmtpClient},
};
use rustymail_core::{Address, config::Config};
use rustymail_store::{
    NotificationPolicy, NotificationWork, QueueBody, QueueLease, QueuePolicy, QueueResult,
};
use std::{
    future::Future,
    io::{self, Read},
    sync::Arc,
    time::Duration,
};
use tokio::time::timeout;
fn seconds(n: u64) -> Duration {
    Duration::from_secs(n)
}

pub fn notification_policy(config: &Config) -> NotificationPolicy {
    NotificationPolicy {
        hostname: config.hostname.clone(),
        local_domains: config.local_domains.clone(),
        max_age_seconds: config.delivery.max_age_seconds,
    }
}

async fn notify_failure(store: StoreClient, policy: NotificationPolicy) {
    let task = match store.call(move |s| s.notification_next(&policy)).await {
        Ok(NotificationWork::Prepare(task)) => task,
        Ok(_) => return,
        Err(_) => return, // Pending diagnostic/backoff is persisted when possible.
    };
    let id = task.delivery_id().to_owned();
    let result = match task.prepare().await {
        Ok(prepared) => store
            .call(move |s| s.notification_commit(prepared))
            .await
            .map(|_| ()),
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        let _ = store
            .call(move |s| s.notification_failed(&id, &error))
            .await;
        crate::log_event(
            "notification_pending",
            serde_json::json!({"retry":"bounded_backoff"}),
        );
    }
}

#[derive(Clone)]
pub struct RelayClient(SmtpClient);
pub struct RelayMessage {
    pub sender: Option<Address>,
    pub recipient: Address,
    pub body: QueueBody,
    pub stored_size: u64,
    pub omit_prefix: String,
}
fn settings(c: &Config) -> SmtpSettings {
    let d = &c.delivery;
    SmtpSettings {
        host: c.relay.host.clone(),
        port: c.relay.port,
        security: if c.relay.tls == "starttls" {
            Security::Starttls
        } else {
            Security::Implicit
        },
        ehlo: c.hostname.clone(),
        ca_file: c.relay.ca_file.clone().unwrap_or_default(),
        username: c.relay.username.clone(),
        password_file: if c.relay.username.is_empty() {
            Default::default()
        } else {
            c.relay.password_file.clone()
        },
        minimum_tls_version: c.tls.minimum_version.clone(),
        buffer_bytes: c.limits.stream_buffer_bytes,
        timeouts: Timeouts {
            connect_timeout_seconds: d.connect_timeout_seconds,
            banner_timeout_seconds: d.banner_timeout_seconds,
            command_timeout_seconds: d.command_timeout_seconds,
            data_command_timeout_seconds: d.data_command_timeout_seconds,
            data_write_timeout_seconds: d.data_write_timeout_seconds,
            final_reply_timeout_seconds: d.final_reply_timeout_seconds,
            handshake_timeout_seconds: c.tls.handshake_timeout_seconds,
            data_total_seconds: c.timeouts.data_total_seconds,
        },
    }
}
impl RelayClient {
    pub async fn load(config: &Config) -> io::Result<Self> {
        notification_policy(config)
            .validate()
            .map_err(io::Error::other)?;
        Ok(Self(SmtpClient::load(&settings(config)).await?))
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn plaintext_lab(target: std::net::SocketAddr, config: &Config) -> io::Result<Self> {
        Ok(Self(SmtpClient::plaintext_lab(target, &settings(config))?))
    }
    pub async fn attempt<R, F, Fut>(
        &self,
        m: RelayMessage,
        reader: R,
        before_body: F,
    ) -> QueueResult
    where
        R: Read + Send + 'static,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), rustymail_store::StoreError>>,
    {
        let m = Message {
            sender: m.sender,
            recipient: m.recipient,
            body: match m.body {
                QueueBody::SevenBit => rustymail_protocol::Body::SevenBit,
                QueueBody::EightBitMime => rustymail_protocol::Body::EightBitMime,
            },
            stored_size: m.stored_size,
            omit_prefix: m.omit_prefix,
        };
        match self
            .0
            .attempt(m, reader, || async {
                before_body().await.map_err(io::Error::other)
            })
            .await
        {
            AttemptResult::Delivered(code) => QueueResult::Delivered(code),
            AttemptResult::Temporary(code) => QueueResult::Temporary(code),
            AttemptResult::Permanent(code) => QueueResult::Permanent(code),
            AttemptResult::ConnectionLost => QueueResult::ConnectionLost,
            AttemptResult::Hold => QueueResult::Hold,
            AttemptResult::Deferred => QueueResult::Deferred,
        }
    }
}

#[cfg(test)]
#[path = "relay_tests.rs"]
mod tests;
pub(crate) async fn serve(
    store: StoreClient,
    client: Arc<RelayClient>,
    config: Arc<Config>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let policy = QueuePolicy {
        concurrency: config.delivery.concurrency,
        per_domain: config.delivery.per_domain_concurrency,
        lease_seconds: 300,
        retry_seconds: config.delivery.retry_seconds.clone(),
        jitter_percent: config.delivery.retry_jitter_percent,
    };
    let mut tasks = tokio::task::JoinSet::new();
    let mut notices = tokio::task::JoinSet::new();
    let mut tick = tokio::time::interval(seconds(1));
    let mut claim_failure_logged = false;
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _=stop.changed()=>break,
            joined=tasks.join_next(),if !tasks.is_empty()=> {
                if joined.is_some_and(|r|r.is_err()) {crate::log_event("relay_task_failed",serde_json::json!({}));}
            },
            joined=notices.join_next(),if !notices.is_empty()=> {
                if joined.is_some_and(|r|r.is_err()) {crate::log_event("notification_task_failed",serde_json::json!({}));}
            },
            _=tick.tick()=>{
                // Expiry is independent of network slots and the due cursor.
                let limit=config.delivery.batch_size;
                if store.call(move|s|s.queue_expire(limit)).await.is_err() {
                    if !claim_failure_logged {crate::log_event("relay_lifecycle_unavailable",serde_json::json!({}));claim_failure_logged=true;}
                    continue;
                }
                if notices.is_empty() {notices.spawn(notify_failure(store.clone(),notification_policy(&config)));}
                if tasks.len()>=policy.concurrency {continue;}
                let policy=policy.clone();let limit=config.delivery.batch_size;
                let leases=store.call(move|s|s.queue_claim(&policy,limit)).await;
                match leases {
                    Ok(leases)=> {
                        claim_failure_logged=false;
                        for lease in leases {tasks.spawn(execute(store.clone(),client.clone(),lease));}
                    },
                    Err(_) if !claim_failure_logged=> {
                        claim_failure_logged=true;
                        crate::log_event("relay_claim_unavailable",serde_json::json!({}));
                    },
                    Err(_)=>(),
                }
            },
        }
    }
    // Stop admission, allow actual attempts to finish, then close their sockets.
    // QueuedReader keeps the instance lock alive if a blocking read outlives abort.
    if timeout(seconds(config.timeouts.shutdown_grace_seconds), async {
        while tasks.join_next().await.is_some() {}
        while notices.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        notices.abort_all();
        while tasks.join_next().await.is_some() {}
        while notices.join_next().await.is_some() {}
    }
}

async fn execute(store: StoreClient, client: Arc<RelayClient>, lease: QueueLease) {
    let lease = Arc::new(lease);
    let requested = lease.clone();
    let reader = store.call(move |s| s.queue_open_body(&requested)).await;
    let result = match reader {
        Err(_) => QueueResult::Hold,
        Ok(reader) => {
            let message = RelayMessage {
                sender: lease.sender().cloned(),
                recipient: lease.recipient().clone(),
                body: lease.body(),
                stored_size: lease.stored_size(),
                omit_prefix: lease.omitted_prefix(),
            };
            let marked = lease.clone();
            let owner = store.clone();
            let attempt = client.attempt(message, reader, move || async move {
                owner.call(move |s| s.queue_mark_body(&marked)).await
            });
            tokio::pin!(attempt);
            let mut renew = tokio::time::interval(seconds(60));
            renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            renew.tick().await;
            loop {
                tokio::select! {
                    result=&mut attempt=>break result,
                    _=renew.tick()=>{
                        let renewed=lease.clone();
                        if store.call(move|s|s.queue_renew(&renewed,300)).await.is_err() {break QueueResult::ConnectionLost;}
                    }
                }
            }
            // The pinned attempt (including socket) drops at this block boundary.
        }
    };
    let Ok(lease) = Arc::try_unwrap(lease) else {
        crate::log_event("relay_attempt_abandoned", serde_json::json!({}));
        return;
    };
    let classification = match &result {
        QueueResult::Delivered(_) => "delivered",
        QueueResult::Temporary(_) => "temporary",
        QueueResult::Permanent(_) => "permanent",
        QueueResult::ConnectionLost => "connection_lost",
        QueueResult::Hold => "hold",
        QueueResult::Deferred => "setup_unavailable",
    };
    if store
        .call(move |s| s.queue_finish(lease, result))
        .await
        .is_err()
    {
        crate::log_event("relay_result_unconfirmed", serde_json::json!({}));
    } else {
        crate::log_event(
            "relay_attempt_finished",
            serde_json::json!({"classification":classification}),
        );
    }
}
