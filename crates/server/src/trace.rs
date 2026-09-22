//! Local delivery representation. No whole-message buffering or MIME rewriting.
use crate::ServerError;
use rustymail_core::{Address, LOCAL_DELIVERY_OVERHEAD_BYTES, valid_domain};
use std::{io, net::IpAddr};
use time::{OffsetDateTime, format_description::well_known::Rfc2822};
use uuid::Uuid;

pub(crate) struct Trace<'a> {
    pub hostname: &'a str,
    pub greeting: Option<&'a str>,
    pub peer: IpAddr,
    pub extended: bool,
    pub encrypted: bool,
    pub authenticated: bool,
}

impl Trace<'_> {
    pub fn prefix(
        &self,
        sender: Option<&Address>,
        operation: Uuid,
        when: OffsetDateTime,
    ) -> Result<String, ServerError> {
        // Domain syntax is validated, not DNS identity. Never interpolate an
        // arbitrary EHLO token as header syntax or disclose recipient lists.
        if !valid_domain(self.hostname) {
            return Err(io::Error::other("invalid trace hostname").into());
        }
        let literal = match self.peer {
            IpAddr::V4(ip) => format!("[{ip}]"),
            IpAddr::V6(ip) => format!("[IPv6:{ip}]"),
        };
        let from = match self.greeting.filter(|name| valid_domain(name)) {
            Some(name) => format!("{name} ({literal})"),
            None => literal,
        };
        let protocol = match (self.extended, self.encrypted, self.authenticated) {
            (false, _, _) => "SMTP",
            (true, true, true) => "ESMTPSA",
            (true, true, false) => "ESMTPS",
            (true, false, true) => "ESMTPA",
            (true, false, false) => "ESMTP",
        };
        let date = when
            .to_offset(time::UtcOffset::UTC)
            .format(&Rfc2822)
            .map_err(|_| io::Error::other("clock cannot represent a mail timestamp"))?;
        let prefix = format!(
            "Return-Path: <{}>\r\nReceived: from {from}\r\n\tby {} with {protocol}\r\n\tid {}; {date}\r\n",
            sender.map_or("", Address::as_str),
            self.hostname,
            operation.simple(),
        );
        // Two further bytes may be needed for empty/header-only DATA. Bound
        // both physical line lengths and total expansion before writing bytes.
        if prefix.len() as u64 + 2 > LOCAL_DELIVERY_OVERHEAD_BYTES
            || prefix.split("\r\n").any(|line| line.len() > 998)
        {
            return Err(io::Error::other("generated trace exceeds reserved space").into());
        }
        Ok(prefix)
    }
}

/// Validate only structural header boundaries; preserve every retained byte.
/// Return-Path and all its continuation lines are removed at final delivery.
#[derive(Default)]
pub(crate) struct HeaderFilter {
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
    pub fn retain(&mut self, line: &[u8]) -> Result<bool, ServerError> {
        let content = line
            .strip_suffix(b"\r\n")
            .ok_or(ServerError::InvalidHeaders)?;
        // 8BITMIME permits high-bit body bytes, not SMTPUTF8 message headers.
        if !content.is_ascii() || content.iter().any(|b| b.is_ascii_control() && *b != b'\t') {
            return Err(ServerError::InvalidHeaders);
        }
        if content.first().is_some_and(|b| matches!(b, b' ' | b'\t')) {
            return if self.field_seen {
                Ok(!self.removing)
            } else {
                Err(ServerError::InvalidHeaders)
            };
        }
        let colon = content
            .iter()
            .position(|&b| b == b':')
            .ok_or(ServerError::InvalidHeaders)?;
        let name = &content[..colon];
        if name.is_empty() || !name.iter().all(|b| (33..=126).contains(b)) {
            return Err(ServerError::InvalidHeaders);
        }
        self.field_seen = true;
        self.removing = name.eq_ignore_ascii_case(b"Return-Path")
            || (self.remove_bcc && name.eq_ignore_ascii_case(b"Bcc"));
        Ok(!self.removing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_formats_observed_peer_null_path_and_utc_without_injection() {
        let mut trace = Trace {
            hostname: "mail.example.test",
            greeting: Some("evil);for<x>"),
            peer: "::1".parse().unwrap(),
            extended: true,
            encrypted: true,
            authenticated: true,
        };
        let prefix = trace
            .prefix(None, Uuid::nil(), OffsetDateTime::UNIX_EPOCH)
            .unwrap();
        assert_eq!(
            prefix,
            "Return-Path: <>\r\nReceived: from [IPv6:::1]\r\n\tby mail.example.test with ESMTPSA\r\n\tid 00000000000000000000000000000000; Thu, 01 Jan 1970 00:00:00 +0000\r\n"
        );
        trace.greeting = Some("client.example.test");
        trace.extended = false;
        let prefix = trace
            .prefix(
                Some(&Address::parse("Bounce@remote.test").unwrap()),
                Uuid::nil(),
                OffsetDateTime::UNIX_EPOCH,
            )
            .unwrap();
        assert!(prefix.starts_with(
            "Return-Path: <Bounce@remote.test>\r\nReceived: from client.example.test ([IPv6:::1])"
        ));
        assert!(prefix.contains(" with SMTP\r\n"));
        trace.hostname = "bad\r\nInjected: yes";
        assert!(
            trace
                .prefix(None, Uuid::nil(), OffsetDateTime::UNIX_EPOCH)
                .is_err()
        );
    }

    #[test]
    fn longest_supported_addresses_fit_the_fixed_expansion_budget() {
        let domain = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61)
        );
        assert_eq!(domain.len(), 253);
        let sender_domain = format!("{}.{}.{}", "a".repeat(63), "b".repeat(63), "c".repeat(61));
        let sender = Address::parse(&format!("{}@{sender_domain}", "x".repeat(64))).unwrap();
        let trace = Trace {
            hostname: &domain,
            greeting: Some(&domain),
            peer: "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap(),
            extended: true,
            encrypted: true,
            authenticated: true,
        };
        let prefix = trace
            .prefix(Some(&sender), Uuid::nil(), OffsetDateTime::UNIX_EPOCH)
            .unwrap();
        assert!(prefix.len() as u64 + 2 <= LOCAL_DELIVERY_OVERHEAD_BYTES);
        assert!(prefix.split("\r\n").all(|line| line.len() <= 998));
    }

    #[test]
    fn folded_duplicate_return_paths_are_removed_without_touching_other_fields() {
        let mut filter = HeaderFilter::default();
        assert!(
            !filter
                .retain(b"rEtUrN-pAtH: <fake@remote.test>\r\n")
                .unwrap()
        );
        assert!(!filter.retain(b"\tcontinued\r\n").unwrap());
        assert!(
            filter
                .retain(b"Received: from previous.example\r\n")
                .unwrap()
        );
        assert!(filter.retain(b"\tby previous.example; date\r\n").unwrap());
        assert!(!filter.retain(b"Return-Path: <>\r\n").unwrap());
        assert!(filter.retain(b"X_Valid: opaque\r\n").unwrap());
        for bad in [
            b" orphan\r\n".as_slice(),
            b"Return-Path : <>\r\n",
            b"No colon\r\n",
            b"X: \0\r\n",
        ] {
            assert!(HeaderFilter::default().retain(bad).is_err());
        }
    }
}
