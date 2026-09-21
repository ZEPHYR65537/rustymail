use rustymail_core::{Address, config::Config};
use rustymail_store::{
    Acceptance, AcceptedMessage, PreparedMessage, StagedMessage, StorageRuntime, Store, StoreError,
    StoreOptions, SubmissionIdentity,
};
use std::{sync::mpsc as std_mpsc, thread};
use tokio::sync::{mpsc, oneshot};

enum Request {
    Call(Box<dyn FnOnce(&mut Store) + Send>),
    Recipient(Address, oneshot::Sender<Result<bool, StoreError>>),
    Stage(oneshot::Sender<Result<StagedMessage, StoreError>>),
    Accept(
        PreparedMessage,
        Acceptance,
        Option<Box<SubmissionIdentity>>,
        oneshot::Sender<Result<AcceptedMessage, StoreError>>,
    ),
}

#[derive(Clone)]
pub struct StoreClient {
    sender: mpsc::Sender<Request>,
}

pub struct StoreWorker {
    pub client: StoreClient,
    join: thread::JoinHandle<()>,
}

pub fn store_options(config: &Config) -> StoreOptions {
    StoreOptions {
        max_message_bytes: config.limits.message_bytes,
        stream_buffer_bytes: config.limits.stream_buffer_bytes,
        disk_reserve_bytes: config.limits.disk_reserve_bytes,
        disk_reserve_percent: config.limits.disk_reserve_percent,
        temporary_reserved_bytes: config.limits.temporary_reserved_bytes,
        cache_kib: config.store.writer_cache_kib,
    }
}

impl StoreWorker {
    /// Start before listening for commands. SQLite ownership never crosses from
    /// the dedicated OS thread onto an asynchronous runtime worker.
    pub fn start(config: &Config, runtime: StorageRuntime) -> Result<Self, StoreError> {
        let (sender, mut receiver) = mpsc::channel(config.store.writer_queue);
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let root = config.data_dir.clone();
        let options = store_options(config);
        let join = thread::Builder::new()
            .name("rustymail-store".into())
            .spawn(move || {
                let mut store =
                    match Store::open_with_runtime(root, options, runtime).and_then(|store| {
                        if !store.check_integrity()?.healthy() {
                            return Err(StoreError::Integrity);
                        }
                        Ok(store)
                    }) {
                        Ok(store) => store,
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                            return;
                        }
                    };
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                while let Some(request) = receiver.blocking_recv() {
                    match request {
                        Request::Call(call) => call(&mut store),
                        Request::Recipient(address, reply) => {
                            let _ = reply.send(store.recipient_exists(&address));
                        }
                        Request::Stage(reply) => {
                            let _ = reply.send(store.stage());
                        }
                        Request::Accept(message, plan, identity, reply) => {
                            // Once dispatched, acceptance finishes even if the
                            // client disconnects and drops its oneshot receiver.
                            let result = match identity {
                                Some(identity) => store.accept_submission(message, plan, *identity),
                                None => store.accept(message, plan),
                            };
                            let _ = reply.send(result);
                        }
                    }
                }
            })?;
        match ready_rx.recv().map_err(|_| StoreError::WorkerUnavailable)? {
            Ok(()) => Ok(Self {
                client: StoreClient { sender },
                join,
            }),
            Err(error) => {
                let _ = join.join();
                Err(error)
            }
        }
    }

    pub async fn shutdown(self) -> Result<(), StoreError> {
        drop(self.client);
        tokio::task::spawn_blocking(move || self.join.join())
            .await
            .map_err(|_| StoreError::WorkerUnavailable)?
            .map_err(|_| StoreError::WorkerUnavailable)
    }
}

impl StoreClient {
    #[cfg(unix)]
    pub async fn change_authority(
        &self,
        changes: tokio::sync::watch::Sender<u64>,
        change: impl FnOnce(&mut Store) -> Result<i64, StoreError> + Send + 'static,
    ) -> Result<i64, StoreError> {
        self.call(move |store| {
            let id = change(store)?;
            // Publish in the owner, even when the request's reply was cancelled.
            changes.send_modify(|version| *version = version.wrapping_add(1));
            Ok(id)
        })
        .await
    }
    pub async fn call<T: Send + 'static>(
        &self,
        call: impl FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(Request::Call(Box::new(move |store| {
                let _ = reply.send(call(store));
            })))
            .await
            .map_err(|_| StoreError::WorkerUnavailable)?;
        result.await.map_err(|_| StoreError::WorkerUnavailable)?
    }
    pub async fn recipient_exists(&self, address: Address) -> Result<bool, StoreError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(Request::Recipient(address, reply))
            .await
            .map_err(|_| StoreError::WorkerUnavailable)?;
        result.await.map_err(|_| StoreError::WorkerUnavailable)?
    }

    pub async fn stage(&self) -> Result<StagedMessage, StoreError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(Request::Stage(reply))
            .await
            .map_err(|_| StoreError::WorkerUnavailable)?;
        result.await.map_err(|_| StoreError::WorkerUnavailable)?
    }

    pub async fn accept(
        &self,
        message: PreparedMessage,
        plan: Acceptance,
        identity: Option<SubmissionIdentity>,
    ) -> Result<AcceptedMessage, StoreError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(Request::Accept(
                message,
                plan,
                identity.map(Box::new),
                reply,
            ))
            .await
            .map_err(|_| StoreError::WorkerUnavailable)?;
        // Missing result is also ambiguous: it is never a definite rejection.
        result.await.map_err(|_| StoreError::OutcomeUnknown)?
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancellation_of_admin_reply_does_not_skip_revocation_notification() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::parse(include_str!("../../../deploy/rustymail.lab.toml")).unwrap();
        config.data_dir = directory.path().join("mail");
        config.limits.disk_reserve_bytes = 0;
        config.limits.disk_reserve_percent = 0;
        let login = Address::parse("alice@example.com").unwrap();
        {
            let mut store = Store::open(&config.data_dir, store_options(&config)).unwrap();
            store.create_account(&login, 10000).unwrap();
        }
        let worker = StoreWorker::start(&config, StorageRuntime::default()).unwrap();
        let client = worker.client.clone();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let blocking = tokio::spawn(async move {
            client
                .call(move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .await
                .unwrap();
        });
        entered_rx.await.unwrap();
        let (changes, mut notified) = tokio::sync::watch::channel(0);
        let client = worker.client.clone();
        let mut pending =
            Box::pin(client.change_authority(changes, move |store| store.disable_account(&login)));
        tokio::select! {
            _=&mut pending=>panic!("blocked owner returned early"),
            _=tokio::time::sleep(std::time::Duration::from_millis(30))=>(),
        }
        drop(pending);
        drop(client);
        release_tx.send(()).unwrap();
        blocking.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), notified.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*notified.borrow(), 1);
        worker.shutdown().await.unwrap();
    }
}
