use anyhow::Context;
use rusqlite::{Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;

// Separate from the expiring API cache and the HA "Purge Caches" action.
pub struct OfflineStore(Connection);

impl OfflineStore {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::from_connection(Connection::open(path).context("opening offline storage")?)
    }

    fn from_connection(conn: Connection) -> anyhow::Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS snapshots (
                name TEXT PRIMARY KEY,
                data TEXT NOT NULL
            );",
        )?;
        Ok(Self(conn))
    }

    pub fn load<T: DeserializeOwned + Default>(&self, name: &str) -> anyhow::Result<T> {
        let data: Option<String> = self
            .0
            .query_row("SELECT data FROM snapshots WHERE name = ?", [name], |row| {
                row.get(0)
            })
            .optional()?;
        match data {
            Some(data) => serde_json::from_str(&data).context("reading offline snapshot"),
            None => Ok(T::default()),
        }
    }

    pub fn save<T: Serialize>(&self, name: &str, data: &T) -> anyhow::Result<()> {
        let data = serde_json::to_string(data)?;
        self.0.execute(
            "INSERT INTO snapshots (name, data) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET data = excluded.data",
            (name, data),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn snapshots_survive_reopening_and_replace_previous_values() -> anyhow::Result<()> {
        let path =
            std::env::temp_dir().join(format!("govee-offline-{}.sqlite", uuid::Uuid::new_v4()));
        {
            let store = OfflineStore::open(&path)?;
            store.save("scenes", &BTreeMap::from([("night", 1)]))?;
            store.save("scenes", &BTreeMap::from([("night", 2)]))?;
            store.save("devices", &vec!["lamp"])?;
        }
        {
            let store = OfflineStore::open(&path)?;
            let scenes: BTreeMap<String, u8> = store.load("scenes")?;
            assert_eq!(scenes["night"], 2);
            assert_eq!(store.load::<Vec<String>>("devices")?, vec!["lamp"]);
            assert!(store.load::<Vec<String>>("missing")?.is_empty());
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn malformed_snapshot_is_not_silently_discarded() -> anyhow::Result<()> {
        let store = OfflineStore::from_connection(Connection::open_in_memory()?)?;
        store.save("scenes", &"invalid schema")?;
        assert!(store.load::<BTreeMap<String, u8>>("scenes").is_err());
        assert_eq!(store.load::<String>("scenes")?, "invalid schema");
        Ok(())
    }
}
