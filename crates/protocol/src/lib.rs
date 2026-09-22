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

/// Transfer encoding is independent of MIME parsing; 8BITMIME is not BINARYMIME.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Body {
    #[default]
    SevenBit,
    EightBitMime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Ehlo(String),
    Helo(String),
    Mail {
        sender: Option<Address>,
        size: Option<u64>,
        body: Option<Body>,
    },
    Rcpt(Address),
    Data,
    Reset,
    Noop,
    Help,
    Verify,
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
            let mut body = None;
            for param in parameters.split_ascii_whitespace() {
                let (key, value) = param
                    .split_once('=')
                    .ok_or(ParseError::UnsupportedParameter)?;
                if key.eq_ignore_ascii_case("SIZE") {
                    if size.is_some()
                        || value.is_empty()
                        || value.len() > 20
                        || !value.bytes().all(|c| c.is_ascii_digit())
                    {
                        return Err(ParseError::Syntax);
                    }
                    // Every 20-digit decimal is legal syntax. Values above u64
                    // still mean "too large" for our bounded server, not 501.
                    size = Some(value.parse().unwrap_or(u64::MAX));
                } else if key.eq_ignore_ascii_case("BODY") {
                    if body.is_some() {
                        return Err(ParseError::Syntax);
                    }
                    body = Some(if value.eq_ignore_ascii_case("7BIT") {
                        Body::SevenBit
                    } else if value.eq_ignore_ascii_case("8BITMIME") {
                        Body::EightBitMime
                    } else {
                        return Err(ParseError::UnsupportedParameter);
                    });
                } else {
                    return Err(ParseError::UnsupportedParameter);
                }
            }
            Ok(Command::Mail { sender, size, body })
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
        "HELP" => Ok(Command::Help),
        "VRFY" if !args.is_empty() => Ok(Command::Verify),
        "QUIT" if args.is_empty() => Ok(Command::Quit),
        "STARTTLS" if args.is_empty() => Ok(Command::StartTls),
        "DATA" | "RSET" | "QUIT" | "STARTTLS" | "VRFY" => Err(ParseError::Syntax),
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
    pub body: Body,
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
    greeting: Option<String>,
    extended: bool,
    transaction: Option<Envelope>,
    max_message_bytes: u64,
    max_recipients: usize,
}

impl Session {
    pub fn greeting(&self) -> Option<&str> {
        self.greeting.as_deref()
    }
    pub fn extended(&self) -> bool {
        self.extended
    }
    pub fn authentication_allowed(&self) -> bool {
        self.extended && self.transaction.is_none()
    }
    pub fn new(max_message_bytes: u64, max_recipients: usize) -> Self {
        Self {
            greeting: None,
            extended: false,
            transaction: None,
            max_message_bytes,
            max_recipients,
        }
    }

    pub fn apply(&mut self, command: Command) -> Action {
        let extended = matches!(command, Command::Ehlo(_));
        match command {
            Command::Ehlo(name) | Command::Helo(name) => {
                self.greeting = Some(name);
                self.extended = extended;
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
            Command::Help => Action::Reply(Reply::new(
                214,
                "2.0.0 EHLO HELO MAIL RCPT DATA RSET NOOP QUIT VRFY HELP STARTTLS",
            )),
            // Do not disclose whether an account exists, or modify the envelope.
            Command::Verify => Action::Reply(Reply::new(252, "2.5.2 Cannot verify user")),
            Command::Quit => Action::Quit,
            Command::StartTls if self.extended => Action::StartTls,
            Command::StartTls => Action::Reply(Reply::new(503, "5.5.1 Send EHLO first")),
            Command::Unsupported => Action::Reply(Reply::new(502, "5.5.1 Command not supported")),
            Command::Mail { sender, size, body } => {
                // A failed new MAIL must never retain recipients from an older transaction.
                self.transaction = None;
                if self.greeting.is_none() {
                    return Action::Reply(Reply::new(503, "5.5.1 Send HELO or EHLO first"));
                }
                if !self.extended && (size.is_some() || body.is_some()) {
                    return Action::Reply(Reply::new(555, "5.5.4 Parameters require EHLO"));
                }
                if size.is_some_and(|n| n > self.max_message_bytes) {
                    return Action::Reply(Reply::new(552, "5.3.4 Message too large"));
                }
                self.transaction = Some(Envelope {
                    sender,
                    recipients: Vec::new(),
                    body: body.unwrap_or_default(),
                });
                Action::Reply(Reply::new(250, "2.1.0 Sender accepted"))
            }
            Command::Rcpt(address) => match &self.transaction {
                None => Action::Reply(Reply::new(503, "5.5.1 Send MAIL first")),
                // Only the router knows whether local account case folding is
                // permitted. Deduplication and the cap belong in its callback.
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
        self.routed_recipient_result(address, exists, true)
    }

    pub fn routed_recipient_result(
        &mut self,
        address: Address,
        exists: bool,
        local: bool,
    ) -> Reply {
        let Some(tx) = self.transaction.as_mut() else {
            return Reply::new(503, "5.5.1 Send MAIL first");
        };
        if !exists {
            return Reply::new(550, "5.1.1 Recipient unavailable; relay denied");
        }
        if !tx.recipients.iter().any(|old| {
            if local {
                old.as_str().eq_ignore_ascii_case(address.as_str())
            } else {
                old == &address
            }
        }) {
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
    fn mail_parameters_preserve_body_and_enforce_size_grammar() {
        assert!(matches!(
            parse_command(b"mAiL FROM:<> bOdY=8bitmime SiZe=00000000000000000001"),
            Ok(Command::Mail {
                sender: None,
                size: Some(1),
                body: Some(Body::EightBitMime)
            })
        ));
        for input in [
            "MAIL FROM:<> SIZE=000000000000000000000",
            "MAIL FROM:<> SIZE=+1",
            "MAIL FROM:<> SIZE=-1",
            "MAIL FROM:<> SIZE=",
            "MAIL FROM:<> SIZE=1 SIZE=1",
            "MAIL FROM:<> BODY=7BIT BODY=8BITMIME",
        ] {
            assert_eq!(
                parse_command(input.as_bytes()),
                Err(ParseError::Syntax),
                "{input}"
            );
        }
        for input in [
            "MAIL FROM:<> BODY=BINARYMIME",
            "MAIL FROM:<> SMTPUTF8",
            "MAIL FROM:<> RET=FULL",
        ] {
            assert_eq!(
                parse_command(input.as_bytes()),
                Err(ParseError::UnsupportedParameter)
            );
        }
        assert!(matches!(
            parse_command(b"MAIL FROM:<> SIZE=99999999999999999999"),
            Ok(Command::Mail {
                size: Some(u64::MAX),
                ..
            })
        ));
    }

    #[test]
    fn duplicate_at_recipient_limit_and_information_commands_keep_envelope() {
        let mut state = Session::new(1024, 1);
        state.apply(parse_command(b"EHLO client.test").unwrap());
        state.apply(parse_command(b"MAIL FROM:<> BODY=8BITMIME").unwrap());
        let address = Address::parse("alice@example.test").unwrap();
        assert!(matches!(
            state.apply(Command::Rcpt(address.clone())),
            Action::CheckRecipient(_)
        ));
        state.recipient_result(address, true);
        let Action::CheckRecipient(duplicate) =
            state.apply(parse_command(b"RCPT TO:<ALICE@example.test>").unwrap())
        else {
            panic!("routing skipped");
        };
        assert_eq!(state.recipient_result(duplicate, true).code, 250);
        for input in [b"NOOP anything".as_slice(), b"HELP MAIL", b"VRFY alice"] {
            assert!(matches!(
                state.apply(parse_command(input).unwrap()),
                Action::Reply(Reply {
                    code: 200..=299,
                    ..
                })
            ));
        }
        let Action::CheckRecipient(other) =
            state.apply(parse_command(b"RCPT TO:<bob@example.test>").unwrap())
        else {
            panic!("routing skipped");
        };
        assert_eq!(state.recipient_result(other, true).code, 452);
        let Action::BeginData(envelope) = state.apply(Command::Data) else {
            panic!("envelope lost");
        };
        assert_eq!(envelope.body, Body::EightBitMime);
        assert_eq!(envelope.recipients.len(), 1);
    }

    #[test]
    fn remote_local_parts_are_distinct_but_domain_case_and_exact_duplicates_are_not() {
        let mut state = Session::new(1024, 2);
        state.apply(Command::Ehlo("client.test".into()));
        state.apply(parse_command(b"MAIL FROM:<>").unwrap());
        for (address, expected) in [
            ("Case@remote.test", 250),
            ("case@REMOTE.TEST", 250),
            ("Case@REMOTE.TEST", 250),
            ("third@remote.test", 452),
        ] {
            let Action::CheckRecipient(address) =
                state.apply(Command::Rcpt(Address::parse(address).unwrap()))
            else {
                panic!("routing skipped");
            };
            assert_eq!(
                state.routed_recipient_result(address, true, false).code,
                expected
            );
        }
        let Action::BeginData(envelope) = state.apply(Command::Data) else {
            panic!("envelope lost")
        };
        assert_eq!(
            envelope
                .recipients
                .iter()
                .map(Address::as_str)
                .collect::<Vec<_>>(),
            ["Case@remote.test", "case@remote.test"]
        );
    }

    #[test]
    fn helo_cannot_enable_extensions_or_retain_old_envelope() {
        let mut state = Session::new(1024, 1);
        for parameter in ["SIZE=1", "BODY=7BIT", "BODY=8BITMIME"] {
            state.apply(Command::Helo("client.test".into()));
            let command = parse_command(format!("MAIL FROM:<> {parameter}").as_bytes()).unwrap();
            assert!(matches!(
                state.apply(command),
                Action::Reply(Reply { code: 555, .. })
            ));
            assert!(matches!(
                state.apply(Command::Data),
                Action::Reply(Reply { code: 503, .. })
            ));
        }
        state.apply(parse_command(b"MAIL FROM:<>").unwrap());
        state.recipient_result(Address::parse("alice@example.test").unwrap(), true);
        let Action::BeginData(envelope) = state.apply(Command::Data) else {
            panic!("missing DATA");
        };
        assert_eq!(envelope.body, Body::SevenBit);
    }

    #[test]
    fn generated_state_sequences_never_deliver_without_current_mail_and_recipient() {
        // Independent three-boolean model over all 7^5 sequences. In particular,
        // EHLO/RSET/new MAIL/failed MAIL/DATA cannot retain a previous recipient.
        for sequence in 0usize..7usize.pow(5) {
            let (mut greeted, mut mail, mut recipient) = (false, false, false);
            let mut state = Session::new(1024, 1);
            let mut sequence = sequence;
            for _ in 0..5 {
                let step = sequence % 7;
                sequence /= 7;
                let command = match step {
                    0 => "EHLO client.test",
                    1 => "MAIL FROM:<>",
                    2 => "MAIL FROM:<> SIZE=1025",
                    3 => "RCPT TO:<alice@example.test>",
                    4 => "RSET",
                    5 => "DATA",
                    _ => "NOOP",
                };
                let expected_data = step == 5 && mail && recipient;
                let action = state.apply(parse_command(command.as_bytes()).unwrap());
                assert_eq!(matches!(action, Action::BeginData(_)), expected_data);
                if let Action::CheckRecipient(address) = action {
                    assert!(mail);
                    assert_eq!(state.recipient_result(address, true).code, 250);
                }
                match step {
                    0 => {
                        greeted = true;
                        mail = false;
                        recipient = false;
                    }
                    1 => {
                        mail = greeted;
                        recipient = false;
                    }
                    2 | 4 => {
                        mail = false;
                        recipient = false;
                    }
                    3 if mail => recipient = true,
                    5 if expected_data => {
                        mail = false;
                        recipient = false;
                    }
                    _ => (),
                }
            }
        }
    }

    #[test]
    fn malformed_and_limit_frames_are_invariant_under_every_single_split() {
        let mut corpus = vec![
            b"a\nb\r\n".to_vec(),
            b"a\rb\r\n".to_vec(),
            b"\0\r\n".to_vec(),
            b".\r\nNOOP\r\n".to_vec(),
        ];
        for length in [509, 510, 511, 535, 536, 537, 998, 999, 1000] {
            let mut bytes = vec![b'x'; length];
            bytes.extend_from_slice(b"\r\n");
            corpus.push(bytes);
        }
        for limit in [512, 538, 1001] {
            for wire in &corpus {
                let expected = LineDecoder::new(limit).feed(wire);
                for split in 0..=wire.len() {
                    let mut decoder = LineDecoder::new(limit);
                    let actual = match decoder.feed(&wire[..split]) {
                        Ok((used, None)) => decoder
                            .feed(&wire[split..])
                            .map(|(more, line)| (used + more, line)),
                        result => result,
                    };
                    assert_eq!(actual, expected);
                    assert!(decoder.buffered_bytes() <= limit);
                }
            }
        }
    }

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
            b"MAIL FROM:<a@b> SIZE=000000000000000000000".as_slice(),
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
                size: Some(0),
                body: None
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
            body: None,
        });
        state.recipient_result(Address::parse("a@b").unwrap(), true);
        state.apply(Command::Mail {
            sender: None,
            size: Some(1025),
            body: None,
        });
        assert!(matches!(
            state.apply(Command::Data),
            Action::Reply(Reply { code: 503, .. })
        ));
        state.apply(Command::Mail {
            sender: None,
            size: None,
            body: None,
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
            body: None,
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
            body: None,
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
