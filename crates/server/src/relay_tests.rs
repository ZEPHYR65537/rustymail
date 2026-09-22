use super::*;
use rustymail_store::{QueuePlan, Store, StoreError, StoreOptions};
use std::io::Cursor;
use tokio::{io::AsyncReadExt, net::TcpListener};

const RAW: &[u8] = b"From: sender@example.test\r\n\r\n.dot\r\nbody\r\n";
fn config() -> Config {
    Config::load(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/rustymail.lab.toml"
    ))
    .unwrap()
}
fn message(raw: &[u8]) -> RelayMessage {
    RelayMessage {
        sender: None,
        recipient: Address::parse("target@remote.test").unwrap(),
        body: QueueBody::SevenBit,
        stored_size: raw.len() as u64,
        omit_prefix: String::new(),
    }
}

// This peer waits for socket EOF, deliberately never replying after DATA.
async fn peer() -> (RelayClient, tokio::task::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = RelayClient::plaintext_lab(listener.local_addr().unwrap(), &config()).unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut wire = BufReader::new(stream);
        wire.write_all(b"220 peer.test\r\n").await.unwrap();
        for (prefix, reply) in [
            ("EHLO ", b"250 peer.test\r\n".as_slice()),
            ("MAIL FROM:", b"250 sender\r\n"),
            ("RCPT TO:", b"250 recipient\r\n"),
            ("DATA", b"354 body\r\n"),
        ] {
            assert!(
                crate::line(&mut wire, 512)
                    .await
                    .unwrap()
                    .unwrap()
                    .starts_with(prefix.as_bytes())
            );
            wire.write_all(reply).await.unwrap();
        }
        let mut body = Vec::new();
        timeout(seconds(5), wire.read_to_end(&mut body))
            .await
            .unwrap()
            .unwrap();
        body
    });
    (client, task)
}

struct Chunks {
    bytes: Cursor<Vec<u8>>,
    maximum: usize,
    fail_eof: bool,
}
impl Read for Chunks {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        let len = target.len().min(self.maximum);
        let n = Read::read(&mut self.bytes, &mut target[..len])?;
        if n == 0 && self.fail_eof {
            Err(io::Error::other("integrity failure at EOF"))
        } else {
            Ok(n)
        }
    }
}

#[tokio::test]
async fn fragmented_body_preserves_projection_transparency_and_fixed_reads() {
    let raw = b"Return-Path: <>\r\nFrom: sender@example.test\r\n\r\n.\r\n..two\r\nend\r\n";
    for maximum in 1..=raw.len() {
        let (stream, mut received) = tokio::io::duplex(4096);
        let mut wire: Wire = BufReader::new(Box::new(stream));
        let client = RelayClient::plaintext_lab("127.0.0.1:1".parse().unwrap(), &config()).unwrap();
        let mut metadata = message(raw);
        metadata.omit_prefix = "Return-Path: <>\r\n".into();
        let reader = Chunks {
            bytes: Cursor::new(raw.to_vec()),
            maximum,
            fail_eof: false,
        };
        assert!(client.send_body(&mut wire, reader, &metadata).await.is_ok());
        drop(wire);
        let mut result = Vec::new();
        received.read_to_end(&mut result).await.unwrap();
        assert_eq!(
            result,
            b"From: sender@example.test\r\n\r\n..\r\n...two\r\nend\r\n"
        );
    }
}

#[tokio::test]
async fn failed_integrity_framing_or_phase_commit_closes_without_final_dot() {
    for (raw, fail_eof, bad_size, bad_prefix, mark_fails) in [
        (RAW, true, false, false, false),
        (RAW, false, true, false, false),
        (RAW, false, false, true, false),
        (RAW, false, false, false, true),
        (
            b"From: x\r\n\r\nbad\n".as_slice(),
            false,
            false,
            false,
            false,
        ),
        (b"From: x\r\n\r\nbad\0\r\n", false, false, false, false),
        (b"From: x\r\n\r\nbad\xff\r\n", false, false, false, false),
        (b"From: x\r\n", false, false, false, false),
    ] {
        let (client, peer) = peer().await;
        let mut metadata = message(raw);
        if bad_size {
            metadata.stored_size += 1;
        }
        if bad_prefix {
            metadata.omit_prefix = "Return-Path: <>\r\n".into();
        }
        let reader = Chunks {
            bytes: Cursor::new(raw.to_vec()),
            maximum: 5,
            fail_eof,
        };
        let result = client
            .attempt(metadata, reader, move || async move {
                if mark_fails {
                    Err(StoreError::PermissionDenied)
                } else {
                    Ok(())
                }
            })
            .await;
        assert!(if mark_fails {
            matches!(result, QueueResult::ConnectionLost)
        } else {
            matches!(result, QueueResult::Hold)
        });
        let body = peer.await.unwrap();
        assert!(!body.windows(5).any(|window| window == b"\r\n.\r\n"));
        if mark_fails {
            assert!(body.is_empty());
        }
    }
}

struct Blocked<R> {
    reader: R,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}
impl<R: Read> Read for Blocked<R> {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
            self.release
                .recv_timeout(seconds(5))
                .map_err(|_| io::Error::other("test release missing"))?;
        }
        self.reader.read(target)
    }
}

#[tokio::test]
async fn cancelled_network_attempt_closes_socket_but_running_read_keeps_store_lock() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("mail");
    let options = StoreOptions {
        disk_reserve_bytes: 1,
        disk_reserve_percent: 0,
        ..StoreOptions::default()
    };
    let mut store = Store::open(&root, options.clone()).unwrap();
    let mut stage = store.stage().unwrap();
    stage.append(RAW).await.unwrap();
    store
        .enqueue(
            stage.prepare().await.unwrap(),
            QueuePlan {
                operation_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                sender: None,
                recipients: vec![Address::parse("target@remote.test").unwrap()],
                body: QueueBody::SevenBit,
                max_age_seconds: 432000,
            },
        )
        .unwrap();
    let lease = store
        .queue_claim(&QueuePolicy::default(), 1)
        .unwrap()
        .pop()
        .unwrap();
    let reader = store.queue_open_body(&lease).unwrap();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, receiver) = std::sync::mpsc::channel();
    let reader = Blocked {
        reader,
        entered: Some(entered),
        release: receiver,
    };
    let (client, peer) = peer().await;
    let attempt = tokio::spawn(async move {
        client
            .attempt(message(RAW), reader, || async { Ok(()) })
            .await
    });
    timeout(seconds(5), ready).await.unwrap().unwrap();
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    assert!(peer.await.unwrap().is_empty()); // Socket is already closed.
    drop(lease);
    drop(store);
    assert!(Store::open_existing(&root, options.clone()).is_err()); // Real reader remains.
    release.send(()).unwrap();
    timeout(seconds(5), async {
        loop {
            if let Ok(store) = Store::open_existing(&root, options.clone()) {
                drop(store);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
