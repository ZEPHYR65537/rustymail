//! Shared configuration and deliberately narrow, validated identifiers.
pub mod config;

use std::fmt;
use thiserror::Error;

/// An ASCII dot-atom mailbox. Quoted local parts and SMTPUTF8 are not supported
/// by the initial laboratory receiver. Remote local-part case is preserved.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Address(String);

#[derive(Debug, Error)]
#[error("unsupported or invalid ASCII mailbox address")]
pub struct AddressError;

pub fn valid_domain(domain: &str) -> bool {
    domain.len() <= 253
        && !domain.is_empty()
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

impl Address {
    pub fn parse(input: &str) -> Result<Self, AddressError> {
        let (local, domain) = input.rsplit_once('@').ok_or(AddressError)?;
        if input.len() > 254
            || local.is_empty()
            || local.len() > 64
            || local.starts_with('.')
            || local.ends_with('.')
            || local.contains("..")
            || !local
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~.".contains(&c))
            || !valid_domain(domain)
        {
            return Err(AddressError);
        }
        Ok(Self(format!("{local}@{}", domain.to_ascii_lowercase())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn domain(&self) -> &str {
        // Construction guarantees exactly one @.
        self.0.rsplit_once('@').map_or("", |(_, domain)| domain)
    }

    /// Only use for this server's explicitly case-insensitive local namespace.
    pub fn local_key(&self) -> String {
        self.0.to_ascii_lowercase()
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_remote_case_from_local_routing() {
        let address = Address::parse("Alice+test@EXAMPLE.com").unwrap();
        assert_eq!(address.as_str(), "Alice+test@example.com");
        assert_eq!(address.local_key(), "alice+test@example.com");
    }

    #[test]
    fn rejects_ambiguous_or_injected_addresses() {
        for address in [
            "a@@b",
            "a..b@c",
            ".a@c",
            "a.@c",
            "a@-host",
            "a@host.",
            "a@host\r\nRCPT TO:<evil@host>",
            "中文@host",
            "\"a b\"@host",
        ] {
            assert!(Address::parse(address).is_err(), "{address:?}");
        }
    }
}
