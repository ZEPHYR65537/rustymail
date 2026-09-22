use crate::{FaultPoint, StorageRuntime, StoreError};
use rusqlite::{Connection, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub const CURRENT_VERSION: u32 = 3;
pub(crate) const BASE: &str = include_str!("../migrations/0001.sql");
const MAINTENANCE: &str = include_str!("../migrations/0002.sql");
const MIGRATIONS: [(u32, &str, &str); 3] = [
    (1, "initial_store", BASE),
    (2, "maintenance_history", MAINTENANCE),
    (3, "outbound_queue", include_str!("../migrations/0003.sql")),
];

#[derive(Debug, Serialize)]
pub struct MigrationRecord {
    pub version: u32,
    pub name: String,
    pub sha256: String,
    pub applied_at_ms: i64,
    pub adopted: bool,
}

fn checksum(sql: &str) -> String {
    format!("{:x}", Sha256::digest(sql.replace("\r\n", "\n").as_bytes()))
}

type SchemaObject = (String, String, Option<String>);
fn shape(connection: &Connection) -> rusqlite::Result<Vec<SchemaObject>> {
    // GLOB treats '_' literally; LIKE would hide user objects named sqliteX...
    connection.prepare("SELECT type,name,sql FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY type,name")?
        .query_map([], |r| {
            let sql: Option<String> = r.get(2)?;
            Ok((r.get(0)?, r.get(1)?, sql.map(|sql|sql.replace("\r\n","\n"))))
        })?
        .collect()
}

/// Validate before changing pragmas or migrating. A legacy version number alone
/// cannot authorize adoption of an unrelated or manually altered database.
pub(crate) fn validate(connection: &Connection) -> Result<u32, StoreError> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > CURRENT_VERSION {
        return Err(StoreError::SchemaVersion);
    }
    let expected = Connection::open_in_memory()?;
    for &(number, _, sql) in &MIGRATIONS {
        if number <= version {
            expected.execute_batch(sql)?;
        }
    }
    if shape(connection)? != shape(&expected)? {
        return Err(StoreError::SchemaVersion);
    }
    if version >= 2 {
        let records = history(connection)?;
        if records.len() != version as usize {
            return Err(StoreError::SchemaVersion);
        }
        for (record, &(number, name, sql)) in records.iter().zip(&MIGRATIONS) {
            if record.version != number || record.name != name || record.sha256 != checksum(sql) {
                return Err(StoreError::SchemaVersion);
            }
        }
    }
    Ok(version)
}

pub(crate) fn upgrade(
    connection: &mut Connection,
    previous: u32,
    runtime: &StorageRuntime,
) -> Result<(), StoreError> {
    if previous == CURRENT_VERSION {
        return Ok(());
    }
    let now = runtime.now_ms()?;
    let transaction = connection.transaction()?;
    for &(number, _, sql) in &MIGRATIONS {
        if number > previous {
            transaction.execute_batch(sql)?;
        }
    }
    for &(number, name, sql) in &MIGRATIONS {
        if previous >= 2 && number <= previous {
            continue;
        }
        transaction.execute(
            "INSERT INTO schema_migration VALUES(?1,?2,?3,?4,?5)",
            params![number, name, checksum(sql), now, number <= previous],
        )?;
    }
    transaction.pragma_update(None, "user_version", CURRENT_VERSION)?;
    runtime.hit(FaultPoint::MigrationApplied)?;
    transaction
        .commit()
        .map_err(|_| StoreError::MigrationOutcomeUnknown)?;
    runtime
        .hit(FaultPoint::MigrationCommitted)
        .map_err(|_| StoreError::MigrationOutcomeUnknown)?;
    Ok(())
}

pub(crate) fn history(connection: &Connection) -> Result<Vec<MigrationRecord>, StoreError> {
    Ok(connection.prepare("SELECT version,name,sha256,applied_at_ms,adopted FROM schema_migration ORDER BY version")?
        .query_map([], |r| Ok(MigrationRecord {
            version: r.get(0)?, name: r.get(1)?, sha256: r.get(2)?,
            applied_at_ms: r.get(3)?, adopted: r.get(4)?,
        }))?.collect::<Result<_, _>>()?)
}
