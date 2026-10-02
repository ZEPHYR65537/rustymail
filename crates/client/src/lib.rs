//! SMTP submission without a database, listener or durable queue.
pub mod config;
pub mod message;
pub mod smtp;
pub mod tls;

use rustymail_protocol::LineDecoder;
use std::io;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "SMTP phase deadline exceeded")
}
async fn line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    limit: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut decoder = LineDecoder::new(limit);
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return if decoder.buffered_bytes() == 0 {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial SMTP line",
                ))
            };
        }
        let (used, complete) = decoder.feed_buffered(bytes).map_err(io::Error::other)?;
        reader.consume(used);
        if complete {
            return Ok(decoder.take_line());
        }
    }
}
