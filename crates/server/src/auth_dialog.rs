//! SASL dialogue; the caller owns SMTP state and the connection attempt count.
use crate::{
    ServerError, auth, line, log_event, reply, timed_out, worker::StoreClient, write_response,
};
use rustymail_protocol::Reply;
use rustymail_store::Principal;
use std::{io, net::IpAddr};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};

pub(crate) enum AuthOutcome {
    Ignored,
    Failed,
    Authenticated(Principal),
}

pub(crate) async fn authenticate<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    read: &mut BufReader<R>,
    write: &mut W,
    bytes: &[u8],
    auth: &auth::AuthService,
    store: &StoreClient,
    peer: IpAddr,
    deadline: tokio::time::Instant,
) -> Result<AuthOutcome, ServerError> {
    let fields: Vec<_> = bytes.split(|&b| b == b' ').collect();
    if !(2..=3).contains(&fields.len()) || !fields[1].eq_ignore_ascii_case(b"PLAIN") {
        reply(write, Reply::new(504, "5.5.4 Use AUTH PLAIN")).await?;
        return Ok(AuthOutcome::Ignored);
    }
    let encoded = if fields.len() == 3 {
        zeroize::Zeroizing::new(fields[2].to_vec())
    } else {
        write_response(write, b"334 \r\n").await?;
        zeroize::Zeroizing::new(
            tokio::time::timeout_at(deadline, line(read, 1024))
                .await
                .map_err(|_| timed_out())??
                .ok_or_else(|| io::Error::other("AUTH EOF"))?,
        )
    };
    if encoded.as_slice() == b"*" {
        reply(write, Reply::new(501, "5.7.0 Authentication cancelled")).await?;
        return Ok(AuthOutcome::Ignored);
    }
    let result = match auth::decode_plain(&encoded) {
        Ok(credentials) => {
            match tokio::time::timeout_at(deadline, auth.authenticate(store, peer, credentials))
                .await
            {
                Ok(result) => result,
                Err(_) => Err(auth::AuthError::Busy),
            }
        }
        Err(error) => Err(error),
    };
    match result {
        Ok(identity) => {
            reply(write, Reply::new(235, "2.7.0 Authentication successful")).await?;
            log_event("authentication_succeeded", serde_json::json!({}));
            Ok(AuthOutcome::Authenticated(identity))
        }
        Err(auth::AuthError::Denied) => {
            reply(write, Reply::new(535, "5.7.8 Authentication failed")).await?;
            log_event("authentication_failed", serde_json::json!({}));
            Ok(AuthOutcome::Failed)
        }
        Err(auth::AuthError::Busy) => {
            reply(
                write,
                Reply::new(454, "4.7.0 Authentication temporarily unavailable"),
            )
            .await?;
            Ok(AuthOutcome::Failed)
        }
    }
}
