use super::*;
use std::io::Cursor;
use tokio::io::AsyncReadExt;
fn config() -> Config {
    crate::config::ClientConfig::parse(include_str!(
        "../../../deploy/rustymail.client.example.toml"
    ))
    .unwrap()
    .smtp
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
        let client = SmtpClient::plaintext_lab("127.0.0.1:1".parse().unwrap(), &config()).unwrap();
        let mut metadata = Message {
            sender: None,
            recipient: Address::parse("target@remote.test").unwrap(),
            body: Body::SevenBit,
            stored_size: raw.len() as u64,
            omit_prefix: String::new(),
        };
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
async fn pipeline_drains_replies_while_commands_are_partially_written() {
    use tokio::io::AsyncBufReadExt;
    // Capacity one forces partial writes in both directions. The peer sends
    // its first reply before reading RCPT; write-all-then-read would deadlock.
    let (stream, peer) = tokio::io::duplex(1);
    let mut wire: Wire = BufReader::new(Box::new(stream));
    let client = SmtpClient::plaintext_lab("127.0.0.1:1".parse().unwrap(), &config()).unwrap();
    let server = async {
        let mut peer = BufReader::with_capacity(1, peer);
        for (command, reply) in [
            ("MAIL FROM:<>\r\n", "250 sender\r\n"),
            ("RCPT TO:<target@remote.test>\r\n", "250 recipient\r\n"),
            ("DATA\r\n", "354 body\r\n"),
        ] {
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            assert_eq!(line, command);
            peer.write_all(reply.as_bytes()).await.unwrap();
        }
    };
    let exchange = async {
        let (codes, ()) = tokio::join!(
            client.envelope_pipeline(
                &mut wire,
                "MAIL FROM:<>\r\nRCPT TO:<target@remote.test>\r\nDATA\r\n"
            ),
            server
        );
        assert_eq!(codes.ok(), Some([250, 250, 354]));
    };
    timeout(Duration::from_secs(2), exchange).await.unwrap();
}

#[tokio::test]
async fn cancelling_a_partial_pipeline_closes_the_owned_connection() {
    let (stream, mut peer) = tokio::io::duplex(1);
    let client = SmtpClient::plaintext_lab("127.0.0.1:1".parse().unwrap(), &config()).unwrap();
    let commands = b"MAIL FROM:<>\r\nRCPT TO:<target@remote.test>\r\nDATA\r\n";
    let attempt = tokio::spawn(async move {
        let mut wire: Wire = BufReader::new(Box::new(stream));
        client
            .envelope_pipeline(&mut wire, std::str::from_utf8(commands).unwrap())
            .await
            .ok()
    });
    assert_eq!(peer.read_u8().await.unwrap(), b'M');
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    let mut partial = vec![b'M'];
    timeout(Duration::from_secs(2), peer.read_to_end(&mut partial))
        .await
        .unwrap()
        .unwrap();
    assert!(commands.starts_with(&partial));
    assert!(partial.len() < commands.len());
}
