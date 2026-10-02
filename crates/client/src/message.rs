//! Validate once before networking; replay independent handles of a private snapshot.
use crate::config::InputLimits;
use rustymail_protocol::{Body, HeaderFilter, LineDecoder};
use std::{
    fs::File,
    io::{self, Read, Write},
};

pub struct Snapshot {
    file: tempfile::NamedTempFile,
    pub size: u64,
    pub body: Body,
}
impl Snapshot {
    pub fn reader(&self) -> io::Result<File> {
        self.file.reopen()
    }
}

pub fn prepare(mut input: impl Read, limits: &InputLimits) -> io::Result<Snapshot> {
    limits.validate()?;
    let mut file = tempfile::Builder::new()
        .prefix("rustymail-send-")
        .tempfile()?;
    let mut buffer = [0u8; 16384];
    let mut output = Vec::with_capacity(17408);
    let mut line = LineDecoder::new(1000);
    let mut filter = HeaderFilter::submission();
    let (mut input_size, mut size, mut header_bytes) = (0u64, 0u64, 0usize);
    let mut headers = true;
    let mut body = Body::SevenBit;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        input_size = input_size
            .checked_add(n as u64)
            .filter(|n| *n <= limits.message_bytes)
            .ok_or_else(|| io::Error::other("message exceeds input limit"))?;
        output.clear();
        let mut bytes = &buffer[..n];
        while !bytes.is_empty() {
            let (used, complete) = line.feed_buffered(bytes).map_err(io::Error::other)?;
            bytes = &bytes[used..];
            if complete {
                let frame = line
                    .frame()
                    .ok_or_else(|| io::Error::other("missing input frame"))?;
                let retain = if headers {
                    header_bytes += frame.len();
                    if header_bytes > limits.header_bytes {
                        return Err(io::Error::other("message headers exceed limit"));
                    }
                    if frame == b"\r\n" {
                        headers = false;
                        true
                    } else {
                        filter.retain(frame).map_err(io::Error::other)?
                    }
                } else {
                    if !frame.is_ascii() {
                        body = Body::EightBitMime;
                    }
                    true
                };
                if retain {
                    output.extend_from_slice(frame);
                }
                line.clear();
            }
        }
        file.write_all(&output)?;
        size += output.len() as u64;
    }
    if headers || line.buffered_bytes() != 0 {
        return Err(io::Error::other(
            "mail requires complete CRLF lines and a header/body separator",
        ));
    }
    file.flush()?;
    Ok(Snapshot { file, size, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn snapshot_filters_private_headers_and_reopens_independent_cursors() {
        let raw=b"From: alice@example.com\r\nBcc: secret@example.com\r\n\tfolded\r\nResent-Bcc: hidden@example.com\r\nReturn-Path: <fake@example.com>\r\n\r\n.dot\r\n\xff\r\n";
        let snapshot = prepare(Cursor::new(raw), &InputLimits::default()).unwrap();
        assert_eq!(snapshot.body, Body::EightBitMime);
        let expected = b"From: alice@example.com\r\n\r\n.dot\r\n\xff\r\n";
        assert_eq!(snapshot.size, expected.len() as u64);
        let mut first = snapshot.reader().unwrap();
        let mut second = snapshot.reader().unwrap();
        let mut prefix = [0; 5];
        first.read_exact(&mut prefix).unwrap();
        let mut bytes = Vec::new();
        second.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, expected);
        bytes.clear();
        first.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, &expected[5..]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                snapshot
                    .file
                    .as_file()
                    .metadata()
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
    }
    #[test]
    fn invalid_input_never_produces_a_snapshot() {
        for raw in [
            b"Subject: x\n\nbody\n".as_slice(),
            b" orphan\r\n\r\n",
            b"Subject: \xff\r\n\r\n",
            b"Subject: x\r\n\r\n\0\r\n",
            b"Subject: x\r\n\r\npartial",
            b"Subject: x\r\n",
        ] {
            assert!(prepare(Cursor::new(raw), &InputLimits::default()).is_err());
        }
        let raw = [b"Subject: x\r\n\r\n".as_slice(), &vec![b'x'; 999], b"\r\n"].concat();
        assert!(prepare(Cursor::new(raw), &InputLimits::default()).is_err());
        let limits = InputLimits {
            message_bytes: 1024,
            header_bytes: 64,
        };
        assert!(
            prepare(
                Cursor::new([b"\r\n".as_slice(), &b"x\r\n".repeat(342)].concat()),
                &limits
            )
            .is_err()
        );
        // Removed headers still count against the original input/header budget.
        assert!(
            prepare(
                Cursor::new(format!("Bcc: {}\r\n\r\n", "x".repeat(100))),
                &limits
            )
            .is_err()
        );
    }
}
