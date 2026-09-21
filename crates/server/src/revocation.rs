//! The pending check belongs to the session, not to a cancellable select branch.
use crate::worker::StoreClient;
use rustymail_store::Principal;
use tokio::sync::watch;

pub(crate) struct Revocations {
    changes: watch::Receiver<u64>,
    pending: bool,
}

impl Revocations {
    pub fn new(changes: watch::Receiver<u64>) -> Self {
        Self {
            changes,
            pending: false,
        }
    }

    pub async fn wait(&mut self, principal: Option<&Principal>, store: &StoreClient) {
        let Some(principal) = principal else {
            return std::future::pending().await;
        };
        loop {
            if !self.pending {
                if self.changes.changed().await.is_err() {
                    return;
                }
                self.pending = true;
            }
            let principal = principal.clone();
            if !store
                .call(move |store| store.principal_current(&principal))
                .await
                .unwrap_or(false)
            {
                return;
            }
            // No await between a successful check and acknowledging it. A newer
            // notification that arrived during the query remains unread.
            self.pending = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::StoreWorker;
    use rustymail_core::{Address, config::Config};
    use rustymail_store::StorageRuntime;
    use std::{future::Future, task::Poll, time::Duration};

    #[tokio::test]
    async fn cancelled_permission_check_is_retried_without_a_second_notification() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::parse(include_str!("../../../deploy/rustymail.lab.toml")).unwrap();
        config.data_dir = directory.path().join("mail");
        let worker = StoreWorker::start(&config, StorageRuntime::default()).unwrap();
        let client = worker.client.clone();
        let principal = client
            .call(|store| {
                let login = Address::parse("alice@example.com").unwrap();
                store.create_account(&login, 10000)?;
                let selector = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
                store.create_credential(
                    &login,
                    selector,
                    "probe",
                    "mail",
                    "$argon2id$v=19$test",
                )?;
                let principal = store
                    .credential_lookup(&login, selector)?
                    .unwrap()
                    .principal;
                store.disable_account(&login)?;
                Ok(principal)
            })
            .await
            .unwrap();
        let (changes, receiver) = watch::channel(0);
        let mut revocations = Revocations::new(receiver);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_client = client.clone();
        let blocked = tokio::spawn(async move {
            blocked_client
                .call(move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                })
                .await
                .unwrap();
        });
        entered_rx.await.unwrap();
        changes.send_replace(1);
        let mut waiting = Box::pin(revocations.wait(Some(&principal), &client));
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx).is_pending())).await
        );
        drop(waiting); // Command/DATA branch wins its select.
        release_tx.send(()).unwrap();
        blocked.await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            revocations.wait(Some(&principal), &client),
        )
        .await
        .unwrap();
        drop(client);
        worker.shutdown().await.unwrap();
    }
}
