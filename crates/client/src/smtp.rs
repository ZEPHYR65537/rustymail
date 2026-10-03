//! Bounded single-recipient SMTP transport shared by CLI and queue workers.
use crate::config::{Security, SmtpSettings as Config, Timeouts};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustymail_core::Address;
use rustymail_protocol::{Body, LineDecoder};
use std::{
    future::Future,
    io::{self, Read},
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::{
    TlsConnector,
    rustls::{
        self, ClientConfig, RootCertStore,
        pki_types::{CertificateDer, ServerName, pem::PemObject},
    },
};
use zeroize::Zeroizing;

#[derive(Debug)]
pub enum AttemptResult {
    Delivered(u16),
    Temporary(u16),
    Permanent(u16),
    ConnectionLost,
    Hold,
    Deferred,
}
trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
type Wire = BufReader<Box<dyn Stream>>;

#[derive(Clone)]
pub struct SmtpClient {
    targets: Vec<SocketAddr>,
    hostname: String,
    greeting: String,
    tls: Option<Arc<ClientConfig>>,
    starttls: bool,
    authorization: Option<Arc<Zeroizing<String>>>,
    limits: Timeouts,
    handshake_seconds: u64,
    body_seconds: u64,
    buffer_bytes: usize,
}

/// Immutable metadata for ONE attempt. The optional prefix is verified and
/// omitted on the wire; the complete stored file is still read through EOF.
pub struct Message {
    pub sender: Option<Address>,
    pub recipient: Address,
    pub body: Body,
    pub stored_size: u64,
    pub omit_prefix: String,
}

#[derive(Default)]
struct Capabilities {
    pipelining: bool,
    starttls: bool,
    eight_bit: bool,
    plain: bool,
    size: Option<Option<u64>>,
}
struct Response {
    code: u16,
    capabilities: Capabilities,
}
enum Failure {
    Connection,
    Setup,
    Content,
}
impl From<io::Error> for Failure {
    fn from(_: io::Error) -> Self {
        Self::Connection
    }
}
fn seconds(n: u64) -> Duration {
    Duration::from_secs(n)
}

impl SmtpClient {
    /// Resolve only the configured upstream at startup. DNS refresh requires a
    /// restart; no recipient-controlled name becomes a connection destination.
    pub async fn load(config: &Config) -> io::Result<Self> {
        config.validate()?;
        let settings = config.clone();
        let built = tokio::task::spawn_blocking(move || {
            let pem = crate::tls::material(&settings.ca_file, 1024 * 1024, false)?;
            let mut roots = RootCertStore::empty();
            for certificate in CertificateDer::pem_slice_iter(&pem) {
                roots
                    .add(certificate.map_err(|_| io::Error::other("invalid relay CA"))?)
                    .map_err(|_| io::Error::other("invalid relay CA"))?;
            }
            if roots.is_empty() {
                return Err(io::Error::other("empty relay trust store"));
            }
            let versions: &[&rustls::SupportedProtocolVersion] =
                if settings.minimum_tls_version == "1.3" {
                    &[&rustls::version::TLS13]
                } else {
                    &[&rustls::version::TLS13, &rustls::version::TLS12]
                };
            let tls = ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_protocol_versions(versions)
            .map_err(|_| io::Error::other("invalid relay TLS versions"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
            let authorization = if settings.username.is_empty() {
                None
            } else {
                if settings.username.len() > 256
                    || settings.username.bytes().any(|b| b.is_ascii_control())
                {
                    return Err(io::Error::other("invalid relay username"));
                }
                let secret =
                    Zeroizing::new(crate::tls::material(&settings.password_file, 1024, true)?);
                let secret = secret
                    .strip_suffix(b"\r\n")
                    .or_else(|| secret.strip_suffix(b"\n"))
                    .unwrap_or(&secret);
                if secret.is_empty() || secret.iter().any(|b| b.is_ascii_control()) {
                    return Err(io::Error::other("invalid relay secret"));
                }
                let raw = Zeroizing::new(
                    [
                        b"\0".as_slice(),
                        settings.username.as_bytes(),
                        b"\0",
                        secret,
                    ]
                    .concat(),
                );
                Some(Arc::new(Zeroizing::new(STANDARD.encode(&*raw))))
            };
            let targets: Vec<_> = (settings.host.as_str(), settings.port)
                .to_socket_addrs()?
                .take(17)
                .collect();
            if targets.is_empty() || targets.len() > 16 {
                return Err(io::Error::other("relay requires 1..16 resolved addresses"));
            }
            Ok(Self {
                targets,
                hostname: settings.host,
                greeting: settings.ehlo,
                tls: Some(Arc::new(tls)),
                starttls: settings.security == Security::Starttls,
                authorization,
                limits: settings.timeouts.clone(),
                handshake_seconds: settings.timeouts.handshake_timeout_seconds,
                body_seconds: settings.timeouts.data_total_seconds,
                buffer_bytes: settings.buffer_bytes,
            })
        });
        timeout(seconds(config.timeouts.connect_timeout_seconds), built)
            .await
            .map_err(|_| io::Error::other("relay initialization deadline exceeded"))?
            .map_err(|_| io::Error::other("relay initialization failed"))?
    }

    /// Test-only plaintext endpoint. Normal binaries cannot construct one.
    #[cfg(any(test, feature = "test-support"))]
    pub fn plaintext_lab(target: SocketAddr, config: &Config) -> io::Result<Self> {
        if !target.ip().is_loopback() {
            return Err(io::Error::other("test peer must be loopback"));
        }
        Ok(Self {
            targets: vec![target],
            hostname: "localhost".into(),
            greeting: config.ehlo.clone(),
            tls: None,
            starttls: false,
            authorization: None,
            limits: config.timeouts.clone(),
            handshake_seconds: 3,
            body_seconds: config.timeouts.data_total_seconds,
            buffer_bytes: config.buffer_bytes,
        })
    }

    pub async fn attempt<R, F, Fut>(
        &self,
        message: Message,
        reader: R,
        before_body: F,
    ) -> AttemptResult
    where
        R: Read + Send + 'static,
        F: FnOnce() -> Fut,
        Fut: Future<Output = io::Result<()>>,
    {
        match self.try_attempt(message, reader, before_body).await {
            Ok(result) => result,
            Err(Failure::Connection) => AttemptResult::ConnectionLost,
            Err(Failure::Setup) => AttemptResult::Deferred,
            Err(Failure::Content) => AttemptResult::Hold,
        }
        // try_attempt owns and drops the socket before a result is returned.
    }

    async fn upgrade(&self, wire: Wire) -> Result<Wire, Failure> {
        // Drop all plaintext read-ahead. Never reinterpret it after the handshake.
        let stream = wire.into_inner();
        let name = ServerName::try_from(self.hostname.clone()).map_err(|_| Failure::Setup)?;
        let config = self.tls.clone().ok_or(Failure::Setup)?;
        let secured = timeout(
            seconds(self.handshake_seconds),
            TlsConnector::from(config).connect(name, stream),
        )
        .await
        .map_err(|_| Failure::Setup)?
        .map_err(|_| Failure::Setup)?;
        Ok(BufReader::with_capacity(4096, Box::new(secured)))
    }

    async fn try_attempt<R, F, Fut>(
        &self,
        message: Message,
        reader: R,
        before_body: F,
    ) -> Result<AttemptResult, Failure>
    where
        R: Read + Send + 'static,
        F: FnOnce() -> Fut,
        Fut: Future<Output = io::Result<()>>,
    {
        let stream = timeout(
            seconds(self.limits.connect_timeout_seconds),
            TcpStream::connect(self.targets.as_slice()),
        )
        .await
        .map_err(|_| Failure::Connection)??;
        stream.set_nodelay(true)?;
        let mut wire: Wire = BufReader::with_capacity(4096, Box::new(stream));
        if self.tls.is_some() && !self.starttls {
            wire = self.upgrade(wire).await?;
        }
        if response(&mut wire, self.limits.banner_timeout_seconds, false)
            .await?
            .code
            != 220
        {
            return Err(Failure::Setup);
        }
        let mut ehlo = self
            .command(&mut wire, &format!("EHLO {}\r\n", self.greeting), true)
            .await?;
        if ehlo.code != 250 {
            return Err(Failure::Setup);
        }
        if self.starttls {
            if !ehlo.capabilities.starttls {
                return Err(Failure::Setup);
            }
            if self.command(&mut wire, "STARTTLS\r\n", false).await?.code != 220 {
                return Err(Failure::Setup);
            }
            wire = self.upgrade(wire).await?;
            ehlo = self
                .command(&mut wire, &format!("EHLO {}\r\n", self.greeting), true)
                .await?;
            if ehlo.code != 250 {
                return Err(Failure::Setup);
            }
        }
        if let Some(token) = &self.authorization {
            if !ehlo.capabilities.plain || self.tls.is_none() {
                return Err(Failure::Setup);
            }
            let command = Zeroizing::new(format!("AUTH PLAIN {}\r\n", token.as_str()));
            let mut code = self.command(&mut wire, &command, false).await?.code;
            if code == 334 {
                let answer = Zeroizing::new(format!("{}\r\n", token.as_str()));
                code = self.command(&mut wire, &answer, false).await?.code;
            }
            if code != 235 {
                return Err(Failure::Setup);
            }
        }
        let size = message
            .stored_size
            .checked_sub(message.omit_prefix.len() as u64)
            .ok_or(Failure::Content)?;
        if message.body == Body::EightBitMime && !ehlo.capabilities.eight_bit
            || ehlo
                .capabilities
                .size
                .flatten()
                .is_some_and(|max| size > max)
        {
            return Err(Failure::Content);
        }
        let mut mail = format!(
            "MAIL FROM:<{}>",
            message.sender.as_ref().map_or("", Address::as_str)
        );
        if ehlo.capabilities.size.is_some() {
            mail.push_str(&format!(" SIZE={size}"));
        }
        if ehlo.capabilities.eight_bit {
            mail.push_str(if message.body == Body::EightBitMime {
                " BODY=8BITMIME"
            } else {
                " BODY=7BIT"
            });
        }
        mail.push_str("\r\n");
        let rcpt = format!("RCPT TO:<{}>\r\n", message.recipient.as_str());
        let code = if ehlo.capabilities.pipelining {
            let codes = self
                .envelope_pipeline(&mut wire, &format!("{mail}{rcpt}DATA\r\n"))
                .await?;
            // Correlate by command POSITION, never by the text or status value.
            // A failure closes this one-use socket, including a stray 354 after
            // a rejected MAIL/RCPT; no empty or real message is committed.
            for code in &codes[..2] {
                if !(200..300).contains(code) {
                    return rejected(*code);
                }
            }
            codes[2]
        } else {
            for command in [&mail, &rcpt] {
                let code = self.command(&mut wire, command, false).await?.code;
                if !(200..300).contains(&code) {
                    return rejected(code);
                }
            }
            write(&mut wire, b"DATA\r\n", self.limits.command_timeout_seconds).await?;
            response(&mut wire, self.limits.data_command_timeout_seconds, false)
                .await?
                .code
        };
        if code != 354 {
            return rejected(code);
        }
        before_body().await.map_err(|_| Failure::Connection)?;
        timeout(
            seconds(self.body_seconds),
            self.send_body(&mut wire, reader, &message),
        )
        .await
        .map_err(|_| Failure::Connection)??;
        // Verified EOF and valid framing precede the final dot. No queued reader
        // failure can be followed by this terminator.
        write(&mut wire, b".\r\n", self.limits.data_write_timeout_seconds).await?;
        let code = response(&mut wire, self.limits.final_reply_timeout_seconds, false)
            .await?
            .code;
        if (200..300).contains(&code) {
            Ok(AttemptResult::Delivered(code))
        } else {
            rejected(code)
        }
    }

    async fn envelope_pipeline(
        &self,
        wire: &mut Wire,
        commands: &str,
    ) -> Result<[u16; 3], Failure> {
        let codes = {
            // Drain replies while writing, so a small TCP window cannot deadlock
            // a command group against a server already producing responses.
            let (read, mut output) = tokio::io::split(&mut *wire);
            let mut input = BufReader::with_capacity(4096, read);
            let receive = async {
                let mut codes = [0; 3];
                for (index, code) in codes.iter_mut().enumerate() {
                    let deadline = if index == 2 {
                        self.limits.data_command_timeout_seconds
                    } else {
                        self.limits.command_timeout_seconds
                    };
                    *code = response(&mut input, deadline, false).await?.code;
                    if *code == 421 {
                        break;
                    }
                }
                if !input.buffer().is_empty() {
                    return Err(io::Error::other("unexpected pipeline response"));
                }
                Ok::<_, io::Error>(codes)
            };
            let (_, codes) = tokio::try_join!(
                write(
                    &mut output,
                    commands.as_bytes(),
                    self.limits.command_timeout_seconds
                ),
                receive
            )?;
            codes
        };
        if !wire.buffer().is_empty() {
            return Err(Failure::Setup);
        }
        Ok(codes)
    }

    async fn command(&self, wire: &mut Wire, command: &str, capture: bool) -> io::Result<Response> {
        // One total deadline covers write + multiline reply, not one per line.
        timeout(seconds(self.limits.command_timeout_seconds), async {
            wire.write_all(command.as_bytes()).await?;
            wire.flush().await?;
            response(wire, self.limits.command_timeout_seconds, capture).await
        })
        .await
        .map_err(|_| crate::timed_out())?
    }

    async fn send_body<R: Read + Send + 'static>(
        &self,
        wire: &mut Wire,
        reader: R,
        message: &Message,
    ) -> Result<(), Failure> {
        let mut pump = Pump(Some((reader, vec![0; self.buffer_bytes])));
        let mut decoder = LineDecoder::new(1000);
        // Every shortest dot-prefixed line (".\r\n") adds one byte. Include
        // this worst-case expansion and a carried line without reallocating.
        let mut output = Vec::with_capacity(self.buffer_bytes + self.buffer_bytes / 3 + 2048);
        let mut total = 0u64;
        let mut first = true;
        let mut headers = true;
        loop {
            let n = timeout(seconds(self.limits.data_write_timeout_seconds), pump.read())
                .await
                .map_err(|_| Failure::Connection)?
                .map_err(|_| Failure::Content)?;
            if n == 0 {
                break;
            }
            total = total
                .checked_add(n as u64)
                .filter(|n| *n <= message.stored_size)
                .ok_or(Failure::Content)?;
            let mut bytes = &pump.0.as_ref().ok_or(Failure::Content)?.1[..n];
            output.clear();
            while !bytes.is_empty() {
                let (used, complete) =
                    decoder.feed_buffered(bytes).map_err(|_| Failure::Content)?;
                bytes = &bytes[used..];
                if complete {
                    let frame = decoder.frame().ok_or(Failure::Content)?;
                    if (headers || message.body == Body::SevenBit) && !frame.is_ascii() {
                        return Err(Failure::Content);
                    }
                    if first && !message.omit_prefix.is_empty() {
                        if frame != message.omit_prefix.as_bytes() {
                            return Err(Failure::Content);
                        }
                    } else {
                        if frame.starts_with(b".") {
                            output.push(b'.');
                        }
                        output.extend_from_slice(frame);
                    }
                    first = false;
                    if frame == b"\r\n" {
                        headers = false;
                    }
                    decoder.clear();
                }
            }
            write(wire, &output, self.limits.data_write_timeout_seconds).await?;
        }
        if total != message.stored_size || decoder.buffered_bytes() != 0 || headers {
            return Err(Failure::Content);
        }
        Ok(())
    }
}

struct Pump<R>(Option<(R, Vec<u8>)>);
impl<R: Read + Send + 'static> Pump<R> {
    async fn read(&mut self) -> io::Result<usize> {
        let (mut reader, mut buffer) = self
            .0
            .take()
            .ok_or_else(|| io::Error::other("body reader unavailable"))?;
        // Cancellation leaves the real reader/lease in the blocking task until
        // read ends. It never releases the storage lock prematurely.
        let (reader, buffer, result) = tokio::task::spawn_blocking(move || {
            let result = reader.read(&mut buffer);
            (reader, buffer, result)
        })
        .await
        .map_err(|_| io::Error::other("body worker failed"))?;
        self.0 = Some((reader, buffer));
        result
    }
}
fn rejected(code: u16) -> Result<AttemptResult, Failure> {
    match code {
        400..=499 => Ok(AttemptResult::Temporary(code)),
        500..=599 => Ok(AttemptResult::Permanent(code)),
        _ => Err(Failure::Connection),
    }
}
async fn write<W: AsyncWrite + Unpin>(wire: &mut W, bytes: &[u8], deadline: u64) -> io::Result<()> {
    timeout(seconds(deadline), async {
        wire.write_all(bytes).await?;
        wire.flush().await
    })
    .await
    .map_err(|_| crate::timed_out())?
}
async fn response<R: AsyncRead + Unpin>(
    wire: &mut BufReader<R>,
    deadline: u64,
    capture: bool,
) -> io::Result<Response> {
    timeout(seconds(deadline), async {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid or oversized relay response",
            )
        };
        let mut code = None;
        let mut total = 0;
        let mut capabilities = Capabilities::default();
        for index in 0..64 {
            let line = crate::line(wire, 512).await?.ok_or_else(invalid)?;
            total += line.len() + 2;
            if total > 16384
                || line.len() < 3
                || !line[..3].iter().all(u8::is_ascii_digit)
                || !line.is_ascii()
                || line.iter().any(|b| b.is_ascii_control() && *b != b'\t')
            {
                return Err(invalid());
            }
            let number = u16::from(line[0] - b'0') * 100
                + u16::from(line[1] - b'0') * 10
                + u16::from(line[2] - b'0');
            if !(200..600).contains(&number) || code.is_some_and(|old| old != number) {
                return Err(invalid());
            }
            code = Some(number);
            let separator = line.get(3).copied().unwrap_or(b' ');
            if !matches!(separator, b' ' | b'-') {
                return Err(invalid());
            }
            if capture && index > 0 && number == 250 {
                let text = std::str::from_utf8(line.get(4..).unwrap_or_default())
                    .map_err(|_| invalid())?;
                let mut words = text.split_ascii_whitespace();
                match words.next().unwrap_or("").to_ascii_uppercase().as_str() {
                    "PIPELINING" => capabilities.pipelining = true,
                    "STARTTLS" => capabilities.starttls = true,
                    "8BITMIME" => capabilities.eight_bit = true,
                    "AUTH" => capabilities.plain = words.any(|s| s.eq_ignore_ascii_case("PLAIN")),
                    "SIZE" => {
                        let limit = words
                            .next()
                            .map(|s| s.parse::<u64>().map_err(|_| invalid()))
                            .transpose()?;
                        capabilities.size = Some(limit);
                    }
                    _ => (),
                }
            }
            if separator == b' ' {
                return Ok(Response {
                    code: number,
                    capabilities,
                });
            }
        }
        Err(invalid())
    })
    .await
    .map_err(|_| crate::timed_out())?
}

#[cfg(test)]
#[path = "smtp_tests.rs"]
mod tests;
