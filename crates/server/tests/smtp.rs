use rustymail_core::{Address, config::Config};
use rustymail_server::{LabServer, store_options};
use rustymail_store::{StorageRuntime, Store};
use std::{io, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::oneshot,
    time::timeout,
};

const LAB: &str = include_str!("../../../deploy/rustymail.lab.toml");

struct Harness {
    directory: tempfile::TempDir,
    config: Config,
    address: SocketAddr,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), rustymail_server::ServerError>>,
}

impl Harness {
    async fn start(alice_quota: u64, bob_quota: u64, message_bytes: u64) -> Self {
        Self::start_with_runtime(
            alice_quota,
            bob_quota,
            message_bytes,
            StorageRuntime::default(),
        )
        .await
    }
    async fn start_with_runtime(
        alice_quota: u64,
        bob_quota: u64,
        message_bytes: u64,
        runtime: StorageRuntime,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::parse(LAB).unwrap();
        config.data_dir = directory.path().join("mail");
        config.limits.message_bytes = message_bytes;
        config.limits.header_bytes = 1024;
        config.limits.disk_reserve_bytes = 1;
        config.limits.disk_reserve_percent = 1;
        config.limits.connections_per_ip = 2;
        config.timeouts.shutdown_grace_seconds = 1;
        config.timeouts.smtp_command_seconds = 2;
        config.timeouts.data_idle_seconds = 2;
        {
            let mut store = Store::open(&config.data_dir, store_options(&config)).unwrap();
            store
                .create_account(&Address::parse("alice@example.com").unwrap(), alice_quota)
                .unwrap();
            store
                .create_account(&Address::parse("bob@example.com").unwrap(), bob_quota)
                .unwrap();
        }
        // Port reservation can race with another local process. Retry only an
        // address-in-use bind, rather than baking a fixed port into the suite.
        let mut server = None;
        for _ in 0..10 {
            let reserve = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            config.listeners.smtp = reserve.local_addr().unwrap();
            drop(reserve);
            match LabServer::bind_with_runtime(config.clone(), runtime.clone()).await {
                Ok(bound) => {
                    server = Some(bound);
                    break;
                }
                Err(rustymail_server::ServerError::Io(error))
                    if error.kind() == io::ErrorKind::AddrInUse => {}
                Err(error) => panic!("{error}"),
            }
        }
        let server = server.expect("ephemeral port available");
        let address = server.local_addr().unwrap();
        let (shutdown, signal) = oneshot::channel();
        let task = tokio::spawn(server.serve_until(async {
            let _ = signal.await;
        }));
        Self {
            directory,
            config,
            address,
            shutdown,
            task,
        }
    }

    async fn stop(self) -> (tempfile::TempDir, Config) {
        let _ = self.shutdown.send(());
        timeout(Duration::from_secs(10), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        (self.directory, self.config)
    }
}

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}
impl Client {
    async fn connect(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).await.unwrap();
        let (read, write) = stream.into_split();
        Self {
            reader: BufReader::new(read),
            writer: write,
        }
    }
    async fn response(&mut self) -> (u16, String) {
        timeout(Duration::from_secs(10), async {
            let mut output = String::new();
            loop {
                let mut line = String::new();
                assert!(
                    self.reader.read_line(&mut line).await.unwrap() > 0,
                    "unexpected EOF: {output}"
                );
                let done = line.as_bytes()[3] == b' ';
                output.push_str(&line);
                if done {
                    return (line[..3].parse().unwrap(), output);
                }
            }
        })
        .await
        .unwrap()
    }
    async fn command(&mut self, command: &str, expected: u16) -> String {
        self.writer.write_all(command.as_bytes()).await.unwrap();
        let (code, response) = self.response().await;
        assert_eq!(code, expected, "{response}");
        response
    }
    async fn greet(&mut self) {
        assert_eq!(self.response().await.0, 220);
        let response = self.command("EHLO test\r\n", 250).await;
        assert!(!response.contains("AUTH"));
        assert!(!response.contains("STARTTLS"));
        assert!(!response.contains("PIPELINING"));
    }
    async fn envelope(&mut self) {
        self.command("MAIL FROM:<sender@remote.test>\r\n", 250)
            .await;
        self.command("RCPT TO:<alice@example.com>\r\n", 250).await;
    }
}

#[tokio::test]
async fn actual_tcp_receives_dot_stuffed_utf8_and_persists_after_restart() {
    let harness = Harness::start(1_000_000, 1_000_000, 25 * 1024 * 1024).await;
    let mut client = Client::connect(harness.address).await;
    client.greet().await;
    client
        .command("MAIL FROM:<sender@remote.test> BODY=8BITMIME\r\n", 250)
        .await;
    client.command("RCPT TO:<alice@example.com>\r\n", 250).await;
    client.command("DATA\r\n", 354).await;
    let raw = "From: sender@remote.test\r\nSubject: lab\r\n\r\n你好\r\n.dot\r\n";
    let wire = raw.replace("\r\n.dot", "\r\n..dot") + ".\r\n";
    for byte in wire.bytes() {
        client.writer.write_all(&[byte]).await.unwrap();
    }
    assert_eq!(client.response().await.0, 250);
    client.command("QUIT\r\n", 221).await;
    drop(client);
    let (_directory, config) = harness.stop().await;
    let store = Store::open(&config.data_dir, store_options(&config)).unwrap();
    let address = Address::parse("alice@example.com").unwrap();
    let messages = store.list_messages(&address, 0, 10).unwrap();
    assert_eq!(messages.len(), 1);
    let mut output = Vec::new();
    store
        .export(&address, &messages[0].message_id, &mut output)
        .unwrap();
    assert!(output.starts_with(
        b"Return-Path: <sender@remote.test>\r\nReceived: from test ([127.0.0.1])\r\n"
    ));
    assert!(output.ends_with(raw.as_bytes()));
    assert_eq!(messages[0].size_bytes, output.len() as u64);
    assert!(store.check_integrity().unwrap().healthy());
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn storage_faults_never_return_250_and_unknown_commits_close_without_rejection() {
    use rustymail_store::FaultPoint;
    for point in [
        FaultPoint::Append,
        FaultPoint::FileSync,
        FaultPoint::BlobDirectorySync,
        FaultPoint::Commit,
        FaultPoint::AfterCommit,
    ] {
        let runtime = StorageRuntime::default().with_hook(move |at| {
            if at == point {
                Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "simulated disk failure",
                ))
            } else {
                Ok(())
            }
        });
        let harness = Harness::start_with_runtime(1_000_000, 1_000_000, 1024, runtime).await;
        let mut client = Client::connect(harness.address).await;
        client.greet().await;
        client.envelope().await;
        client.command("DATA\r\n", 354).await;
        client
            .writer
            .write_all(b"Subject: injected failure\r\n\r\nTest.\r\n.\r\n")
            .await
            .unwrap();
        let mut final_line = String::new();
        let count = timeout(
            Duration::from_secs(10),
            client.reader.read_line(&mut final_line),
        )
        .await
        .unwrap()
        .unwrap();
        if matches!(point, FaultPoint::Commit | FaultPoint::AfterCommit) {
            assert_eq!(
                count, 0,
                "unknown outcome must not claim rejection: {final_line}"
            );
        } else {
            assert!(final_line.starts_with("451 "), "{point:?}: {final_line}");
        }
        drop(client);
        let (_directory, config) = harness.stop().await;
        let store = Store::open_existing(&config.data_dir, store_options(&config)).unwrap();
        assert!(store.check_integrity().unwrap().healthy());
        let messages = store
            .list_messages(&Address::parse("alice@example.com").unwrap(), 0, 10)
            .unwrap();
        assert_eq!(
            messages.len(),
            usize::from(point == FaultPoint::AfterCommit)
        );
    }
}

#[tokio::test]
async fn rejects_relay_unknown_users_and_malformed_mail_clears_old_transaction() {
    let harness = Harness::start(1_000_000, 1_000_000, 1024).await;
    let mut client = Client::connect(harness.address).await;
    client.greet().await;
    client.command("AUTH PLAIN Zm9v\r\n", 502).await;
    client.command("DATA\r\n", 503).await;
    client.envelope().await;
    client
        .command("RCPT TO:<nobody@example.com>\r\n", 550)
        .await;
    client
        .command("RCPT TO:<victim@external.test>\r\n", 550)
        .await;
    client
        .command("MAIL FROM:<sender@remote.test> SIZE=oops\r\n", 501)
        .await;
    client.command("DATA\r\n", 503).await;
    client.command("QUIT\r\n", 221).await;
    drop(client);
    let (_directory, config) = harness.stop().await;
    let store = Store::open(&config.data_dir, store_options(&config)).unwrap();
    assert_eq!(store.check_integrity().unwrap().referenced_blobs, 0);
}

#[tokio::test]
async fn multi_recipient_quota_failure_never_partially_delivers() {
    // The input fits Bob's quota, but final trace + content does not.
    let harness = Harness::start(1_000_000, 100, 1024).await;
    let mut client = Client::connect(harness.address).await;
    client.greet().await;
    client.envelope().await;
    client.command("RCPT TO:<bob@example.com>\r\n", 250).await;
    client.command("DATA\r\n", 354).await;
    client
        .command("Subject: test\r\n\r\nmail\r\n.\r\n", 452)
        .await;
    client.command("DATA\r\n", 503).await;
    client.command("QUIT\r\n", 221).await;
    drop(client);
    let (_directory, config) = harness.stop().await;
    let store = Store::open(&config.data_dir, store_options(&config)).unwrap();
    assert_eq!(store.check_integrity().unwrap().referenced_blobs, 0);
}

#[tokio::test]
async fn local_delivery_shares_final_bytes_and_replays_only_identical_representation() {
    let harness = Harness::start(1_000_000, 1_000_000, 1024).await;
    let mut client = Client::connect(harness.address).await;
    client.greet().await;
    client.command("MAIL FROM:<>\r\n", 250).await;
    for rcpt in ["alice@example.com", "bob@example.com", "ALICE@example.com"] {
        client.command(&format!("RCPT TO:<{rcpt}>\r\n"), 250).await;
    }
    client.command("DATA\r\n", 354).await;
    client.command("Return-Path: <forged@remote.test>\r\n\told continuation\r\nReceived: from old.test\r\n\tby previous.test; date\r\nRETURN-PATH: <>\r\nSubject: shared\r\n\r\nReturn-Path: body stays\r\n.\r\n", 250).await;
    client.command("QUIT\r\n", 221).await;
    drop(client);
    let (_directory, config) = harness.stop().await;
    let mut store = Store::open(&config.data_dir, store_options(&config)).unwrap();
    let alice = Address::parse("alice@example.com").unwrap();
    let bob = Address::parse("bob@example.com").unwrap();
    let a = store.list_messages(&alice, 0, 10).unwrap();
    let b = store.list_messages(&bob, 0, 10).unwrap();
    assert_eq!((a.len(), b.len()), (1, 1));
    assert_eq!(a[0].message_id, b[0].message_id);
    let mut bytes = Vec::new();
    store.export(&alice, &a[0].message_id, &mut bytes).unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.starts_with("Return-Path: <>\r\nReceived: from test ([127.0.0.1])\r\n"));
    assert!(text.ends_with("Received: from old.test\r\n\tby previous.test; date\r\nSubject: shared\r\n\r\nReturn-Path: body stays\r\n"));
    assert!(!text.contains("forged") && !text.contains("old continuation"));
    assert!(
        !text.contains(" for ")
            && !text.contains("alice@example.com")
            && !text.contains("bob@example.com")
    );
    let operation = text
        .split("\r\n\tid ")
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let plan = rustymail_store::Acceptance {
        operation_id: operation.to_owned(),
        sender: None,
        recipients: vec![alice.clone(), bob],
    };
    let mut replay = store.stage().unwrap();
    replay.append(&bytes).await.unwrap();
    let accepted = store
        .accept(replay.prepare().await.unwrap(), plan.clone())
        .unwrap();
    assert!(accepted.already_committed);
    assert_eq!(accepted.message_id, a[0].message_id);
    let mut changed = store.stage().unwrap();
    changed.append(&bytes).await.unwrap();
    changed.append(b"changed\r\n").await.unwrap();
    assert!(matches!(
        store.accept(changed.prepare().await.unwrap(), plan),
        Err(rustymail_store::StoreError::IdempotencyConflict)
    ));
    assert_eq!(store.list_messages(&alice, 0, 10).unwrap().len(), 1);
    assert!(store.check_integrity().unwrap().healthy());
}

#[tokio::test]
async fn exact_client_limit_excludes_generated_trace_and_empty_data_is_well_formed() {
    let harness = Harness::start(1_000_000, 1_000_000, 1024).await;
    let mut client = Client::connect(harness.address).await;
    client.greet().await;
    // SIZE is an estimate, never the frame length. Actual input has its own bound.
    client.command("MAIL FROM:<> SIZE=1\r\n", 250).await;
    client.command("RCPT TO:<alice@example.com>\r\n", 250).await;
    client.command("DATA\r\n", 354).await;
    let raw = format!("\r\n{}\r\n{}\r\n", "x".repeat(998), "y".repeat(20));
    assert_eq!(raw.len(), 1024);
    client.command(&(raw.clone() + ".\r\n"), 250).await;
    client.envelope().await;
    client.command("DATA\r\n", 354).await;
    client.command(".\r\n", 250).await;
    client.command("QUIT\r\n", 221).await;
    drop(client);
    let (_directory, config) = harness.stop().await;
    let store = Store::open(&config.data_dir, store_options(&config)).unwrap();
    let alice = Address::parse("alice@example.com").unwrap();
    let messages = store.list_messages(&alice, 0, 10).unwrap();
    assert_eq!(messages.len(), 2);
    let mut bytes = Vec::new();
    store
        .export(&alice, &messages[0].message_id, &mut bytes)
        .unwrap();
    assert!(bytes.ends_with(raw.as_bytes()) && bytes.len() > 1024);
    bytes.clear();
    store
        .export(&alice, &messages[1].message_id, &mut bytes)
        .unwrap();
    assert!(bytes.ends_with(b"\r\n\r\n"));
    assert!(store.check_integrity().unwrap().healthy());
}

#[tokio::test]
async fn oversize_actual_data_closes_without_executing_trailing_commands() {
    let harness = Harness::start(1_000_000, 1_000_000, 1024).await;
    let mut client = Client::connect(harness.address).await;
    client.greet().await;
    client.command("MAIL FROM:<> SIZE=1025\r\n", 552).await;
    client.envelope().await;
    client.command("DATA\r\n", 354).await;
    let data = format!(
        "\r\n{}\r\n{}\r\n.\r\nMAIL FROM:<>\r\n",
        "x".repeat(600),
        "y".repeat(600)
    );
    client.command(&data, 552).await;
    let mut end = String::new();
    let result = timeout(Duration::from_secs(2), client.reader.read_line(&mut end))
        .await
        .unwrap();
    assert!(matches!(result, Ok(0)) || result.is_err());
    drop(client);
    let (_directory, config) = harness.stop().await;
    assert_eq!(
        Store::open(&config.data_dir, store_options(&config))
            .unwrap()
            .check_integrity()
            .unwrap()
            .referenced_blobs,
        0
    );
}

#[tokio::test]
async fn bare_lf_and_partial_data_never_become_messages() {
    let harness = Harness::start(1_000_000, 1_000_000, 1024).await;
    let mut client = Client::connect(harness.address).await;
    client.greet().await;
    client.command("MAIL FROM:<>\n", 500).await;
    drop(client);
    let mut client = Client::connect(harness.address).await;
    client.greet().await;
    client.envelope().await;
    client.command("DATA\r\n", 354).await;
    client
        .writer
        .write_all(b"Subject: unfinished\r\n\r\npartial")
        .await
        .unwrap();
    drop(client);
    let (_directory, config) = harness.stop().await;
    let store = Store::open(&config.data_dir, store_options(&config)).unwrap();
    assert_eq!(store.check_integrity().unwrap().referenced_blobs, 0);
}

#[tokio::test]
async fn per_ip_connection_limit_rejects_before_starting_a_session() {
    let harness = Harness::start(1_000_000, 1_000_000, 1024).await;
    let mut first = Client::connect(harness.address).await;
    first.greet().await;
    let mut second = Client::connect(harness.address).await;
    second.greet().await;
    let mut third = Client::connect(harness.address).await;
    assert_eq!(third.response().await.0, 421);
    drop((first, second, third));
    harness.stop().await;
}

#[tokio::test]
async fn slow_command_expires_instead_of_extending_deadline_per_byte() {
    let harness = Harness::start(1_000_000, 1_000_000, 1024).await;
    let mut client = Client::connect(harness.address).await;
    assert_eq!(client.response().await.0, 220);
    client.writer.write_all(b"EH").await.unwrap();
    assert_eq!(client.response().await.0, 421);
    drop(client);
    harness.stop().await;
}
