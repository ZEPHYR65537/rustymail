use rustymail_core::{Address, config::Config};
use rustymail_store::{
    Acceptance, AcceptedMessage, PreparedMessage, StagedMessage, StorageRuntime, Store, StoreError,
    StoreOptions,
};
use std::{sync::mpsc as std_mpsc, thread};
use tokio::sync::{mpsc, oneshot};

enum Request {
    Recipient(Address, oneshot::Sender<Result<bool, StoreError>>),
    Stage(oneshot::Sender<Result<StagedMessage, StoreError>>),
    Accept(
        PreparedMessage,
        Acceptance,
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
                        Request::Recipient(address, reply) => {
                            let _ = reply.send(store.recipient_exists(&address));
                        }
                        Request::Stage(reply) => {
                            let _ = reply.send(store.stage());
                        }
                        Request::Accept(message, plan, reply) => {
                            // Once dispatched, acceptance finishes even if the
                            // client disconnects and drops its oneshot receiver.
                            let result = store.accept(message, plan);
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
    ) -> Result<AcceptedMessage, StoreError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(Request::Accept(message, plan, reply))
            .await
            .map_err(|_| StoreError::WorkerUnavailable)?;
        // Missing result is also ambiguous: it is never a definite rejection.
        result.await.map_err(|_| StoreError::OutcomeUnknown)?
    }
}
