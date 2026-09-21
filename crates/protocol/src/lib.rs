//! Bounded SMTP framing and pure session transitions.
use rustymail_core::Address;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("line exceeds its byte limit")]
    TooLong,
    #[error("bare CR/LF or NUL in SMTP input")]
    InvalidLineEnding,
}

/// Incremental CRLF framing. Allocated input memory is capped by `limit`.
pub struct LineDecoder {
    buffer: Vec<u8>,
    limit: usize,
    complete: bool,
}

impl LineDecoder {
    pub fn new(limit: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(limit),
            limit,
            complete: false,
        }
    }

    /// Consume at most one line; the caller retains the unconsumed suffix.
    /// Returned lines exclude CRLF. Framing errors require closing the session.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(usize, Option<Vec<u8>>), FrameError> {
        let (used, _) = self.feed_buffered(bytes)?;
        Ok((used, self.take_line()))
    }

    /// Transfer a completed line without CRLF to its caller. Unlike `frame`,
    /// this transfers the allocation too; the next input will allocate again.
    pub fn take_line(&mut self) -> Option<Vec<u8>> {
        if !self.complete {
            return None;
        }
        self.complete = false;
        let mut line = std::mem::take(&mut self.buffer);
        line.truncate(line.len() - 2);
        Some(line)
    }

    /// Keep the completed CRLF frame in reusable storage. Call `clear` after
    /// consuming `frame`; no allocation is needed for each subsequent DATA line.
    pub fn feed_buffered(&mut self, bytes: &[u8]) -> Result<(usize, bool), FrameError> {
        if self.complete {
            return Ok((0, true));
        }
        if self.buffer.capacity() < self.limit {
            self.buffer.reserve_exact(self.limit - self.buffer.len());
        }
        for (index, &byte) in bytes.iter().enumerate() {
            if self.buffer.len() == self.limit {
                return Err(FrameError::TooLong);
            }
            if byte == 0
                || (byte == b'\n' && self.buffer.last() != Some(&b'\r'))
                || (self.buffer.last() == Some(&b'\r') && byte != b'\n')
            {
                return Err(FrameError::InvalidLineEnding);
            }
            self.buffer.push(byte);
            if byte == b'\n' {
                self.complete = true;
                return Ok((index + 1, true));
            }
        }
        Ok((bytes.len(), false))
    }

    pub fn frame(&self) -> Option<&[u8]> {
        self.complete.then_some(self.buffer.as_slice())
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.complete = false;
    }

    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Ehlo(String),
    Helo(String),
    Mail {
        sender: Option<Address>,
        size: Option<u64>,
    },
    Rcpt(Address),
    Data,
    Reset,
    Noop,
    StartTls,
    Quit,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    Syntax,
    UnsupportedParameter,
}

/// Parse only the explicitly supported laboratory SMTP subset.
pub fn parse_command(line: &[u8]) -> Result<Command, ParseError> {
    if !line.is_ascii() || line.iter().any(|b| b.is_ascii_control()) {
        return Err(ParseError::Syntax);
    }
    let text = std::str::from_utf8(line).map_err(|_| ParseError::Syntax)?;
    let (verb, args) = text.split_once(' ').unwrap_or((text, ""));
    let args = args.trim_matches(' ');
    match verb.to_ascii_uppercase().as_str() {
        "EHLO" | "HELO" => {
            if args.is_empty() || args.contains(' ') {
                return Err(ParseError::Syntax);
            }
            Ok(if verb.eq_ignore_ascii_case("EHLO") {
                Command::Ehlo(args.to_owned())
            } else {
                Command::Helo(args.to_owned())
            })
        }
        "MAIL" => {
            let (path, parameters) = parse_path(args, "FROM:")?;
            let sender = if path.is_empty() {
                None
            } else {
                Some(Address::parse(path).map_err(|_| ParseError::Syntax)?)
            };
            let mut size = None;
            let mut body_seen = false;
            for param in parameters.split_ascii_whitespace() {
                let (key, value) = param
                    .split_once('=')
                    .ok_or(ParseError::UnsupportedParameter)?;
                if key.eq_ignore_ascii_case("SIZE") && size.is_none() {
                    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()) {
                        return Err(ParseError::Syntax);
                    }
                    size = Some(value.parse().map_err(|_| ParseError::Syntax)?);
                } else if key.eq_ignore_ascii_case("BODY")
                    && !body_seen
                    && (value.eq_ignore_ascii_case("7BIT")
                        || value.eq_ignore_ascii_case("8BITMIME"))
                {
                    body_seen = true;
                } else {
                    return Err(ParseError::UnsupportedParameter);
                }
            }
            Ok(Command::Mail { sender, size })
        }
        "RCPT" => {
            let (path, parameters) = parse_path(args, "TO:")?;
            if !parameters.is_empty() {
                return Err(ParseError::UnsupportedParameter);
            }
            Ok(Command::Rcpt(
                Address::parse(path).map_err(|_| ParseError::Syntax)?,
            ))
        }
        "DATA" if args.is_empty() => Ok(Command::Data),
        "RSET" if args.is_empty() => Ok(Command::Reset),
        "NOOP" => Ok(Command::Noop),
        "QUIT" if args.is_empty() => Ok(Command::Quit),
        "STARTTLS" if args.is_empty() => Ok(Command::StartTls),
        "DATA" | "RSET" | "QUIT" | "STARTTLS" => Err(ParseError::Syntax),
        _ => Ok(Command::Unsupported),
    }
}

fn parse_path<'a>(args: &'a str, prefix: &str) -> Result<(&'a str, &'a str), ParseError> {
    if !args
        .get(..prefix.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(prefix))
    {
        return Err(ParseError::Syntax);
    }
    let rest = args[prefix.len()..]
        .strip_prefix('<')
        .ok_or(ParseError::Syntax)?;
    let (path, suffix) = rest.split_once('>').ok_or(ParseError::Syntax)?;
    if !suffix.is_empty() && !suffix.starts_with(' ') {
        return Err(ParseError::Syntax);
    }
    Ok((path, suffix.trim_matches(' ')))
}

#[derive(Clone, Debug)]
pub struct Envelope {
    pub sender: Option<Address>,
    pub recipients: Vec<Address>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    pub text: &'static str,
}

impl Reply {
    pub const fn new(code: u16, text: &'static str) -> Self {
        Self { code, text }
    }
}

#[derive(Debug)]
pub enum Action {
    Reply(Reply),
    Hello { extended: bool },
    CheckRecipient(Address),
    BeginData(Envelope),
    Quit,
    StartTls,
}

/// No sockets, DNS, files or database calls: transitions can be exhaustively
/// exercised without a runtime. RCPT lookup has an explicit completion step.
pub struct Session {
    greeted: bool,
    extended: bool,
    transaction: Option<Envelope>,
    max_message_bytes: u64,
    max_recipients: usize,
}

impl Session {
    pub fn authentication_allowed(&self) -> bool {
        self.extended && self.transaction.is_none()
    }
    pub fn new(max_message_bytes: u64, max_recipients: usize) -> Self {
        Self {
            greeted: false,
            extended: false,
            transaction: None,
            max_message_bytes,
            max_recipients,
        }
    }

    pub fn apply(&mut self, command: Command) -> Action {
        match command {
            Command::Ehlo(_) | Command::Helo(_) => {
                self.greeted = true;
                self.extended = matches!(command, Command::Ehlo(_));
                self.transaction = None;
                Action::Hello {
                    extended: self.extended,
                }
            }
            Command::Reset => {
                self.transaction = None;
                Action::Reply(Reply::new(250, "2.0.0 Reset"))
            }
            Command::Noop => Action::Reply(Reply::new(250, "2.0.0 OK")),
            Command::Quit => Action::Quit,
            Command::StartTls if self.extended => Action::StartTls,
            Command::StartTls => Action::Reply(Reply::new(503, "5.5.1 Send EHLO first")),
            Command::Unsupported => Action::Reply(Reply::new(502, "5.5.1 Command not supported")),
            Command::Mail { sender, size } => {
                // A failed new MAIL must never retain recipients from an older transaction.
                self.transaction = None;
                if !self.greeted {
                    return Action::Reply(Reply::new(503, "5.5.1 Send HELO or EHLO first"));
                }
                if size.is_some_and(|n| n > self.max_message_bytes) {
                    return Action::Reply(Reply::new(552, "5.3.4 Message too large"));
                }
                self.transaction = Some(Envelope {
                    sender,
                    recipients: Vec::new(),
                });
                Action::Reply(Reply::new(250, "2.1.0 Sender accepted"))
            }
            Command::Rcpt(address) => match &self.transaction {
                None => Action::Reply(Reply::new(503, "5.5.1 Send MAIL first")),
                Some(tx) if tx.recipients.len() >= self.max_recipients => {
                    Action::Reply(Reply::new(452, "4.5.3 Too many recipients"))
                }
                Some(_) => Action::CheckRecipient(address),
            },
            Command::Data => {
                if self
                    .transaction
                    .as_ref()
                    .is_none_or(|tx| tx.recipients.is_empty())
                {
                    return Action::Reply(Reply::new(503, "5.5.1 Need accepted recipient"));
                }
                match self.transaction.take() {
                    Some(envelope) => Action::BeginData(envelope),
                    None => Action::Reply(Reply::new(451, "4.3.0 State unavailable")),
                }
            }
        }
    }

    pub fn recipient_result(&mut self, address: Address, exists: bool) -> Reply {
        let Some(tx) = self.transaction.as_mut() else {
            return Reply::new(503, "5.5.1 Send MAIL first");
        };
        if !exists {
            return Reply::new(550, "5.1.1 Recipient unavailable; relay denied");
        }
        if !tx
            .recipients
            .iter()
            .any(|old| old.local_key() == address.local_key())
        {
            if tx.recipients.len() >= self.max_recipients {
                return Reply::new(452, "4.5.3 Too many recipients");
            }
            tx.recipients.push(address);
        }
        Reply::new(250, "2.1.5 Recipient accepted")
    }
}

/// The DATA terminator and transparency octet are interpreted only in DATA.
/// `None` means end-of-DATA. All other output is a CRLF-terminated raw line.
pub fn decode_data_line(mut line: Vec<u8>) -> Result<Option<Vec<u8>>, FrameError> {
    if line == b"." {
        return Ok(None);
    }
    if line.first() == Some(&b'.') {
        line.remove(0);
    }
    if line.len() + 2 > 1000 {
        return Err(FrameError::TooLong);
    }
    line.extend_from_slice(b"\r\n");
    Ok(Some(line))
}

/// Borrow a complete, already validated CRLF frame, including its line ending.
/// SMTP transparency only advances the slice; it does not shift/copy the line.
pub fn decode_data_frame(frame: &[u8]) -> Result<Option<&[u8]>, FrameError> {
    if !frame.ends_with(b"\r\n") {
        return Err(FrameError::InvalidLineEnding);
    }
    if frame == b".\r\n" {
        return Ok(None);
    }
    let data = frame.strip_prefix(b".").unwrap_or(frame);
    if data.len() > 1000 {
        return Err(FrameError::TooLong);
    }
    Ok(Some(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrowed_frames_preserve_chunk_boundaries_transparency_and_limits() {
        let wire = b"..hello\r\n\r\n.\r\n";
        for split in 0..=wire.len() {
            let mut decoder = LineDecoder::new(1001);
            let mut decoded = Vec::new();
            for mut input in [&wire[..split], &wire[split..]] {
                while !input.is_empty() {
                    let (used, complete) = decoder.feed_buffered(input).unwrap();
                    input = &input[used..];
                    if complete {
                        decoded.push(
                            decode_data_frame(decoder.frame().unwrap())
                                .unwrap()
                                .map(<[u8]>::to_vec),
                        );
                        decoder.clear();
                    }
                }
            }
            assert_eq!(
                decoded,
                [Some(b".hello\r\n".to_vec()), Some(b"\r\n".to_vec()), None]
            );
        }
        let mut decoder = LineDecoder::new(1001);
        let mut boundary = vec![b'.'];
        boundary.extend(std::iter::repeat_n(b'x', 998));
        boundary.extend_from_slice(b"\r\n");
        assert!(decoder.feed_buffered(&boundary).unwrap().1);
        assert_eq!(
            decode_data_frame(decoder.frame().unwrap())
                .unwrap()
                .unwrap()
                .len(),
            1000
        );
        decoder.clear();
        boundary[0] = b'x';
        assert!(decoder.feed_buffered(&boundary).unwrap().1);
        assert_eq!(
            decode_data_frame(decoder.frame().unwrap()),
            Err(FrameError::TooLong)
        );
    }

    fn framed(chunks: &[&[u8]]) -> Result<Vec<Vec<u8>>, FrameError> {
        let mut decoder = LineDecoder::new(512);
        let mut lines = Vec::new();
        for chunk in chunks {
            let mut remaining = *chunk;
            while !remaining.is_empty() {
                let (used, line) = decoder.feed(remaining)?;
                if let Some(line) = line {
                    lines.push(line);
                }
                remaining = &remaining[used..];
            }
        }
        assert_eq!(decoder.buffered_bytes(), 0);
        Ok(lines)
    }

    #[test]
    fn every_split_and_one_byte_chunks_are_equivalent() {
        let bytes = b"EHLO localhost\r\nMAIL FROM:<a@example.com>\r\nRCPT TO:<b@example.com>\r\n";
        let expected = framed(&[bytes]).unwrap();
        for split in 0..=bytes.len() {
            assert_eq!(
                framed(&[&bytes[..split], &bytes[split..]]).unwrap(),
                expected
            );
        }
        assert_eq!(
            framed(&bytes.chunks(1).collect::<Vec<_>>()).unwrap(),
            expected
        );
    }

    #[test]
    fn rejects_smuggling_and_bounded_line_overflow() {
        for bytes in [b"EHLO a\n".as_slice(), b"EHLO a\rNOOP\r\n", b"EHLO a\0\r\n"] {
            assert!(LineDecoder::new(512).feed(bytes).is_err());
        }
        let mut decoder = LineDecoder::new(8);
        decoder.feed(b"12345678").unwrap();
        assert_eq!(decoder.feed(b"9"), Err(FrameError::TooLong));
        assert_eq!(decoder.buffered_bytes(), 8);
    }

    #[test]
    fn rejects_parameters_and_numeric_overflow() {
        for bytes in [
            b"MAIL FROM:<a@b> SIZE=18446744073709551616".as_slice(),
            b"MAIL FROM:<a@b> SIZE=1 SIZE=2",
            b"MAIL FROM:<a@b> SMTPUTF8",
            b"RCPT TO:<a@b> ORCPT=x",
            b"DATA evil",
            b"MAIL FROM:<a@b>SIZE=1",
        ] {
            assert!(parse_command(bytes).is_err());
        }
        assert!(matches!(
            parse_command(b"MAIL FROM:<> SIZE=0"),
            Ok(Command::Mail {
                sender: None,
                size: Some(0)
            })
        ));
    }

    #[test]
    fn reset_and_rejected_sender_cannot_reuse_recipients() {
        let mut state = Session::new(1024, 2);
        assert!(matches!(
            state.apply(Command::Data),
            Action::Reply(Reply { code: 503, .. })
        ));
        state.apply(Command::Ehlo("test".into()));
        state.apply(Command::Mail {
            sender: None,
            size: None,
        });
        state.recipient_result(Address::parse("a@b").unwrap(), true);
        state.apply(Command::Mail {
            sender: None,
            size: Some(1025),
        });
        assert!(matches!(
            state.apply(Command::Data),
            Action::Reply(Reply { code: 503, .. })
        ));
        state.apply(Command::Mail {
            sender: None,
            size: None,
        });
        state.recipient_result(Address::parse("a@b").unwrap(), true);
        state.apply(Command::Reset);
        assert!(matches!(
            state.apply(Command::Data),
            Action::Reply(Reply { code: 503, .. })
        ));
    }

    #[test]
    fn rejected_and_duplicate_recipients_have_correct_envelope() {
        let mut state = Session::new(1024, 2);
        state.apply(Command::Ehlo("test".into()));
        state.apply(Command::Mail {
            sender: None,
            size: None,
        });
        assert_eq!(
            state
                .recipient_result(Address::parse("evil@remote").unwrap(), false)
                .code,
            550
        );
        state.recipient_result(Address::parse("a@b").unwrap(), true);
        state.recipient_result(Address::parse("A@b").unwrap(), true);
        if let Action::BeginData(env) = state.apply(Command::Data) {
            assert_eq!(env.recipients.len(), 1);
        } else {
            panic!("expected DATA");
        }
        assert!(matches!(
            state.apply(Command::Data),
            Action::Reply(Reply { code: 503, .. })
        ));
    }

    #[test]
    fn extensions_require_ehlo_and_a_new_tls_session_has_no_old_envelope() {
        assert_eq!(parse_command(b"STARTTLS"), Ok(Command::StartTls));
        assert_eq!(parse_command(b"STARTTLS extra"), Err(ParseError::Syntax));
        let mut state = Session::new(1024, 2);
        assert!(matches!(
            state.apply(Command::StartTls),
            Action::Reply(Reply { code: 503, .. })
        ));
        state.apply(Command::Helo("client".into()));
        assert!(!state.authentication_allowed());
        assert!(matches!(
            state.apply(Command::StartTls),
            Action::Reply(Reply { code: 503, .. })
        ));
        state.apply(Command::Ehlo("client".into()));
        assert!(state.authentication_allowed());
        state.apply(Command::Mail {
            sender: None,
            size: None,
        });
        state.recipient_result(Address::parse("a@b").unwrap(), true);
        assert!(!state.authentication_allowed());
        assert!(matches!(state.apply(Command::StartTls), Action::StartTls));
        // Successful transport upgrade constructs a fresh session.
        state = Session::new(1024, 2);
        assert!(matches!(
            state.apply(Command::Data),
            Action::Reply(Reply { code: 503, .. })
        ));
        assert!(!state.authentication_allowed());
    }

    #[test]
    fn data_transparency_and_limits_include_crlf() {
        assert_eq!(decode_data_line(b".".to_vec()).unwrap(), None);
        assert_eq!(
            decode_data_line(b"..hello".to_vec()).unwrap().unwrap(),
            b".hello\r\n"
        );
        assert_eq!(decode_data_line(Vec::new()).unwrap().unwrap(), b"\r\n");
        assert_eq!(decode_data_line(vec![b'x'; 999]), Err(FrameError::TooLong));
        let mut escaped = vec![b'.'];
        escaped.extend(vec![b'x'; 998]);
        assert_eq!(decode_data_line(escaped).unwrap().unwrap().len(), 1000);
    }
}
