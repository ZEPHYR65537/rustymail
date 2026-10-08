//! One current schema. Experimental stores are never upgraded or overwritten.
use crate::{FaultPoint, StorageRuntime, StoreError};
use rusqlite::Connection;

const SQL: &str = include_str!("../schema.sql");
const APPLICATION_ID: u32 = 0x52554D4C; // RUML; identifies the file, not a release.

type SchemaObject = (String, String, Option<String>);
fn shape(connection: &Connection) -> rusqlite::Result<Vec<SchemaObject>> {
    connection.prepare("SELECT type,name,sql FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY type,name")?
        .query_map([], |r| {
            let sql: Option<String> = r.get(2)?;
            Ok((r.get(0)?, r.get(1)?, sql.map(|s| s.replace("\r\n", "\n"))))
        })?.collect()
}

/// Called before changing database pragmas. False means a completely empty DB.
pub(crate) fn validate(connection: &Connection) -> Result<bool, StoreError> {
    let application: u32 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let version: u32 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let actual = shape(connection)?;
    if application == 0 && version == 0 && actual.is_empty() {
        return Ok(false);
    }
    if application != APPLICATION_ID || version != 0 {
        return Err(StoreError::UnsupportedSchema);
    }
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(SQL)?;
    if actual != shape(&expected)? {
        return Err(StoreError::UnsupportedSchema);
    }
    Ok(true)
}

pub(crate) fn initialize(
    connection: &mut Connection,
    runtime: &StorageRuntime,
) -> Result<(), StoreError> {
    let tx = connection.transaction()?;
    tx.execute_batch(SQL)?;
    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    runtime.hit(FaultPoint::SchemaApplied)?;
    tx.commit()
        .map_err(|_| StoreError::InitializationOutcomeUnknown)?;
    runtime
        .hit(FaultPoint::SchemaCommitted)
        .map_err(|_| StoreError::InitializationOutcomeUnknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Store, StoreOptions};

    #[test]
    fn initialization_is_atomic_and_reopening_is_idempotent() {
        for boundary in [FaultPoint::SchemaApplied, FaultPoint::SchemaCommitted] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("mail");
            let runtime = StorageRuntime::default().with_hook(move |point| {
                if point == boundary {
                    Err(std::io::Error::other("initialization interrupted"))
                } else {
                    Ok(())
                }
            });
            assert!(Store::open_with_runtime(&root, StoreOptions::default(), runtime).is_err());
            let db = Connection::open(root.join("meta.sqlite")).unwrap();
            assert_eq!(
                validate(&db).unwrap(),
                boundary == FaultPoint::SchemaCommitted
            );
            drop(db);
            drop(Store::open(&root, StoreOptions::default()).unwrap());
            let store = Store::open_existing(&root, StoreOptions::default()).unwrap();
            assert!(store.check_integrity().unwrap().healthy());
        }
    }

    #[test]
    fn incompatible_or_modified_databases_are_rejected_without_writes() {
        for statement in [
            "PRAGMA application_id=0; PRAGMA user_version=4",
            "CREATE TABLE unrelated(id INTEGER)",
            "CREATE TABLE sqliteXextra(id INTEGER)",
            "DROP INDEX queue_ready",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("mail");
            let store = Store::open(&root, StoreOptions::default()).unwrap();
            store.connection.execute_batch(statement).unwrap();
            drop(store);
            let path = root.join("meta.sqlite");
            let before = std::fs::read(&path).unwrap();
            assert!(matches!(
                Store::open(&root, StoreOptions::default()),
                Err(StoreError::UnsupportedSchema)
            ));
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn inspection_does_not_initialize_an_empty_existing_database() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("mail");
        crate::blob::private_directory(&root).unwrap();
        let db = Connection::open(root.join("meta.sqlite")).unwrap();
        assert!(matches!(
            Store::open_existing(&root, StoreOptions::default()),
            Err(StoreError::UnsupportedSchema)
        ));
        assert!(shape(&db).unwrap().is_empty());
    }
}
