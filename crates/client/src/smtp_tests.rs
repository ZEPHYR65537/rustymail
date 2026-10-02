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
