//! Database authority for identities. Password hashing runs outside this owner.
use crate::{Store, StoreError, blob::valid_id};
use rusqlite::{Connection, OptionalExtension, params};
use rustymail_core::Address;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Principal {
    pub account_id: i64,
    pub credential_id: i64,
    pub auth_epoch: i64,
    pub scope: String,
}

// Deliberately no Debug/Serialize: the verifier is not administration output.
pub struct CredentialRecord {
    pub principal: Principal,
    pub password_phc: String,
}

#[derive(Serialize)]
pub struct CredentialSummary {
    pub id: i64,
    pub selector: String,
    pub label: String,
    pub scope: String,
    pub revoked_at_ms: Option<i64>,
    pub last_used_at_ms: Option<i64>,
}

#[derive(Clone)]
pub struct SubmissionIdentity {
    pub principal: Principal,
    pub author: Address,
}

pub(crate) fn current(connection: &Connection, principal: &Principal) -> Result<bool, StoreError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM credential c JOIN account a ON a.id=c.account_id WHERE c.id=?1 AND a.id=?2 AND a.auth_epoch=?3 AND a.status='active' AND c.revoked_at_ms IS NULL AND c.scope=?4)",
        params![principal.credential_id,principal.account_id,principal.auth_epoch,principal.scope], |r|r.get(0))?)
}

pub(crate) fn may_send(
    connection: &Connection,
    principal: &Principal,
    address: &Address,
) -> Result<bool, StoreError> {
    Ok(principal.scope == "mail" && current(connection, principal)? && connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM address WHERE account_id=?1 AND address=?2 AND send_enabled=1)",
        params![principal.account_id,address.local_key()],|r|r.get(0))?)
}

impl Store {
    pub fn credential_lookup(
        &self,
        login: &Address,
        selector: &str,
    ) -> Result<Option<CredentialRecord>, StoreError> {
        if !valid_id(selector) {
            return Ok(None);
        }
        Ok(self.connection.query_row(
            "SELECT a.id,c.id,a.auth_epoch,c.scope,substr(c.password_phc,1,513) FROM credential c JOIN account a ON a.id=c.account_id WHERE a.login=?1 AND c.selector=?2 AND a.status='active' AND c.revoked_at_ms IS NULL",
            params![login.local_key(),selector], |r|Ok(CredentialRecord {principal:Principal{account_id:r.get(0)?,credential_id:r.get(1)?,auth_epoch:r.get(2)?,scope:r.get(3)?},password_phc:r.get(4)?})).optional()?)
    }

    /// Revalidate after the expensive hash, since revocation may have raced it.
    pub fn finish_authentication(&mut self, principal: &Principal) -> Result<(), StoreError> {
        let now = self.runtime.now_ms()?;
        let transaction = self.connection.transaction()?;
        if !current(&transaction, principal)? {
            return Err(StoreError::PermissionDenied);
        }
        transaction.execute(
            "UPDATE credential SET last_used_at_ms=?1 WHERE id=?2",
            params![now, principal.credential_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn principal_current(&self, principal: &Principal) -> Result<bool, StoreError> {
        current(&self.connection, principal)
    }

    pub fn authorize_sender(
        &self,
        principal: &Principal,
        address: &Address,
    ) -> Result<bool, StoreError> {
        may_send(&self.connection, principal, address)
    }

    pub fn create_credential(
        &mut self,
        login: &Address,
        selector: &str,
        label: &str,
        scope: &str,
        phc: &str,
    ) -> Result<i64, StoreError> {
        if !valid_id(selector)
            || label.is_empty()
            || label.len() > 80
            || label.chars().any(char::is_control)
            || !matches!(scope, "mail" | "read_only")
            || phc.len() > 512
            || !phc.starts_with("$argon2id$v=19$")
        {
            return Err(StoreError::InvalidInput);
        }
        let account: Option<i64> = self
            .connection
            .query_row(
                "SELECT id FROM account WHERE login=?1 AND status='active'",
                [login.local_key()],
                |r| r.get(0),
            )
            .optional()?;
        self.connection.execute("INSERT INTO credential(account_id,selector,label,password_phc,scope,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![account.ok_or(StoreError::NotFound)?,selector,label,phc,scope,self.runtime.now_ms()?])?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn list_credentials(
        &self,
        login: &Address,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<CredentialSummary>, StoreError> {
        if after_id < 0 || !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidInput);
        }
        Ok(self.connection.prepare("SELECT c.id,c.selector,c.label,c.scope,c.revoked_at_ms,c.last_used_at_ms FROM credential c JOIN account a ON a.id=c.account_id WHERE a.login=?1 AND c.id>?2 ORDER BY c.id LIMIT ?3")?
            .query_map(params![login.local_key(),after_id,limit as i64],|r|Ok(CredentialSummary{id:r.get(0)?,selector:r.get(1)?,label:r.get(2)?,scope:r.get(3)?,revoked_at_ms:r.get(4)?,last_used_at_ms:r.get(5)?}))?.collect::<Result<_,_>>()?)
    }

    /// Bumping the account epoch also invalidates existing sessions using other
    /// credentials. They may reconnect with any credential that remains active.
    pub fn revoke_credential(&mut self, selector: &str) -> Result<i64, StoreError> {
        if !valid_id(selector) {
            return Err(StoreError::InvalidInput);
        }
        let now = self.runtime.now_ms()?;
        let transaction = self.connection.transaction()?;
        let account: Option<i64> = transaction
            .query_row(
                "SELECT account_id FROM credential WHERE selector=?1",
                [selector],
                |r| r.get(0),
            )
            .optional()?;
        let account = account.ok_or(StoreError::NotFound)?;
        transaction.execute(
            "UPDATE credential SET revoked_at_ms=COALESCE(revoked_at_ms,?1) WHERE selector=?2",
            params![now, selector],
        )?;
        transaction.execute(
            "UPDATE account SET auth_epoch=auth_epoch+1 WHERE id=?1",
            [account],
        )?;
        transaction.commit()?;
        Ok(account)
    }

    pub fn disable_account(&mut self, login: &Address) -> Result<i64, StoreError> {
        let now = self.runtime.now_ms()?;
        let transaction = self.connection.transaction()?;
        let account: Option<i64> = transaction
            .query_row(
                "SELECT id FROM account WHERE login=?1",
                [login.local_key()],
                |r| r.get(0),
            )
            .optional()?;
        let account = account.ok_or(StoreError::NotFound)?;
        transaction.execute(
            "UPDATE account SET status='disabled',auth_epoch=auth_epoch+1 WHERE id=?1",
            [account],
        )?;
        transaction.execute(
            "UPDATE credential SET revoked_at_ms=COALESCE(revoked_at_ms,?1) WHERE account_id=?2",
            params![now, account],
        )?;
        transaction.commit()?;
        Ok(account)
    }

    pub fn set_send_as(
        &mut self,
        login: &Address,
        address: &Address,
        enabled: bool,
    ) -> Result<i64, StoreError> {
        let transaction = self.connection.transaction()?;
        let account: Option<i64> = transaction
            .query_row(
                "SELECT id FROM account WHERE login=?1 AND status='active'",
                [login.local_key()],
                |r| r.get(0),
            )
            .optional()?;
        let account = account.ok_or(StoreError::NotFound)?;
        let owner: Option<i64> = transaction
            .query_row(
                "SELECT account_id FROM address WHERE address=?1",
                [address.local_key()],
                |r| r.get(0),
            )
            .optional()?;
        if owner.is_some_and(|owner| owner != account) {
            return Err(StoreError::PermissionDenied);
        }
        transaction.execute("INSERT INTO address(address,account_id,receive_enabled,send_enabled) VALUES(?1,?2,0,?3) ON CONFLICT(address) DO UPDATE SET send_enabled=excluded.send_enabled",params![address.local_key(),account,enabled])?;
        transaction.execute(
            "UPDATE account SET auth_epoch=auth_epoch+1 WHERE id=?1",
            [account],
        )?;
        transaction.commit()?;
        Ok(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SELECTOR: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PHC: &str = "$argon2id$v=19$m=65536,t=3,p=4$test$test";
    fn address(raw: &str) -> Address {
        Address::parse(raw).unwrap()
    }
    fn options() -> crate::StoreOptions {
        crate::StoreOptions {
            disk_reserve_bytes: 0,
            disk_reserve_percent: 0,
            ..crate::StoreOptions::default()
        }
    }
    #[tokio::test]
    async fn authorization_is_rechecked_in_the_accepting_transaction() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mail");
        let mut store = Store::open(&root, options()).unwrap();
        let alice = address("alice@example.com");
        let bob = address("bob@example.com");
        store.create_account(&alice, 10000).unwrap();
        store.create_account(&bob, 10000).unwrap();
        store.set_send_as(&alice, &alice, true).unwrap();
        assert!(matches!(
            store.set_send_as(&alice, &bob, true),
            Err(StoreError::PermissionDenied)
        ));
        store
            .create_credential(&alice, SELECTOR, "test", "mail", PHC)
            .unwrap();
        let principal = store
            .credential_lookup(&alice, SELECTOR)
            .unwrap()
            .unwrap()
            .principal;
        assert!(store.credential_lookup(&bob, SELECTOR).unwrap().is_none());
        assert!(store.authorize_sender(&principal, &alice).unwrap());
        assert!(!store.authorize_sender(&principal, &bob).unwrap());
        let mut stage = store.stage().unwrap();
        stage
            .append(b"From: alice@example.com\r\n\r\ntest\r\n")
            .await
            .unwrap();
        let ready = stage.prepare().await.unwrap();
        store.revoke_credential(SELECTOR).unwrap();
        assert!(!store.principal_current(&principal).unwrap());
        assert!(store.finish_authentication(&principal).is_err());
        assert!(matches!(
            store.accept_submission(
                ready,
                crate::Acceptance {
                    operation_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                    sender: Some(alice.clone()),
                    recipients: vec![bob.clone()]
                },
                SubmissionIdentity {
                    principal,
                    author: alice.clone()
                }
            ),
            Err(StoreError::PermissionDenied)
        ));
        assert!(store.list_messages(&bob, 0, 10).unwrap().is_empty());
        assert!(store.check_integrity().unwrap().healthy());
        assert!(store.credential_lookup(&alice, SELECTOR).unwrap().is_none());
        store
            .create_credential(
                &alice,
                "cccccccccccccccccccccccccccccccc",
                "read",
                "read_only",
                PHC,
            )
            .unwrap();
        let principal = store
            .credential_lookup(&alice, "cccccccccccccccccccccccccccccccc")
            .unwrap()
            .unwrap()
            .principal;
        assert!(!store.authorize_sender(&principal, &alice).unwrap());
        store.disable_account(&alice).unwrap();
        assert!(!store.principal_current(&principal).unwrap());
    }
}
