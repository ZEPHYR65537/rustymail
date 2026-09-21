//! One concrete transport type keeps STARTTLS ownership explicit. No plaintext
//! read buffer or SMTP state is carried into the encrypted phase.
use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};
use tokio_rustls::{TlsAcceptor, rustls::ServerConfig, server::TlsStream};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Receiver,
    ImplicitSubmission,
    StartTlsSubmission,
}

impl Role {
    pub fn submission(self) -> bool {
        self != Self::Receiver
    }
}

pub(crate) enum Transport {
    Plain(TcpStream),
    // Keep the per-connection enum small; no allocation for a plain receiver.
    Tls(Box<TlsStream<TcpStream>>),
}

impl Transport {
    pub async fn upgrade(self, config: Arc<ServerConfig>, deadline: Duration) -> io::Result<Self> {
        let Self::Plain(stream) = self else {
            return Err(io::Error::other("TLS is already active"));
        };
        let stream = tokio::time::timeout(deadline, TlsAcceptor::from(config).accept(stream))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "TLS handshake deadline exceeded")
            })??;
        Ok(Self::Tls(Box::new(stream)))
    }
}

impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buffer),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buffer),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, bytes),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, bytes),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}
