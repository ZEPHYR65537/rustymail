use thiserror::Error;
#[derive(Debug, Error)]
#[error("invalid message header structure")]
pub struct HeaderError;

/// Validate only structural header boundaries; preserve every retained byte.
/// Return-Path and all its continuation lines are removed at final delivery.
#[derive(Default)]
pub struct HeaderFilter {
    field_seen: bool,
    removing: bool,
    remove_bcc: bool,
}

impl HeaderFilter {
    pub fn submission() -> Self {
        Self {
            remove_bcc: true,
            ..Self::default()
        }
    }
    pub fn retain(&mut self, line: &[u8]) -> Result<bool, HeaderError> {
        let content = line.strip_suffix(b"\r\n").ok_or(HeaderError)?;
        // 8BITMIME permits high-bit body bytes, not SMTPUTF8 message headers.
        if !content.is_ascii() || content.iter().any(|b| b.is_ascii_control() && *b != b'\t') {
            return Err(HeaderError);
        }
        if content.first().is_some_and(|b| matches!(b, b' ' | b'\t')) {
            return if self.field_seen {
                Ok(!self.removing)
            } else {
                Err(HeaderError)
            };
        }
        let colon = content.iter().position(|&b| b == b':').ok_or(HeaderError)?;
        let name = &content[..colon];
        if name.is_empty() || !name.iter().all(|b| (33..=126).contains(b)) {
            return Err(HeaderError);
        }
        self.field_seen = true;
        self.removing = name.eq_ignore_ascii_case(b"Return-Path")
            || (self.remove_bcc
                && (name.eq_ignore_ascii_case(b"Bcc") || name.eq_ignore_ascii_case(b"Resent-Bcc")));
        Ok(!self.removing)
    }
}
