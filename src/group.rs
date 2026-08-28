use crate::database::Database;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::convert::TryFrom;
use std::error::Error;
use std::io;
use std::sync::Arc;

const MAX_GROUPS: usize = 50;
const GROUP_TARGET_PREFIX: &str = "group:";

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceGroup {
    pub id: u64,
    pub name: String,
    pub device_ids: Vec<String>,
}

pub fn automation_target(id: u64) -> String {
    format!("{GROUP_TARGET_PREFIX}{id}")
}

pub fn automation_group_id(target: &str) -> Option<u64> {
    target.strip_prefix(GROUP_TARGET_PREFIX)?.parse().ok()
}

pub struct GroupEngine {
    database: Arc<Database>,
}

impl GroupEngine {
    pub fn new(database: Arc<Database>) -> Self {
        Self { database }
    }

    pub fn groups(&self) -> Result<Vec<DeviceGroup>> {
        self.database.with_connection(|connection| {
            let mut statement = connection.prepare("SELECT id, name FROM groups ORDER BY id")?;
            let headers = statement
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            load_groups(connection, headers)
        })
    }

    pub fn get(&self, id: u64) -> Result<Option<DeviceGroup>> {
        let Ok(id) = i64::try_from(id) else {
            return Ok(None);
        };
        self.database.with_connection(|connection| {
            let header = connection
                .query_row("SELECT id, name FROM groups WHERE id = ?1", [id], |row| {
                    Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?))
                })
                .optional()?;
            match header {
                Some(header) => Ok(load_groups(connection, vec![header])?.pop()),
                None => Ok(None),
            }
        })
    }

    pub fn add(&self, name: &str, device_ids: Vec<String>) -> Result<u64> {
        let group = validated_group(name, device_ids)?;
        self.database.with_connection(|connection| {
            let transaction = connection.transaction()?;
            let count: i64 =
                transaction.query_row("SELECT COUNT(*) FROM groups", [], |row| row.get(0))?;
            if count >= MAX_GROUPS as i64 {
                return Err(invalid_input("no more than 50 groups can be created"));
            }
            transaction.execute("INSERT INTO groups (name) VALUES (?1)", [&group.name])?;
            let id = transaction.last_insert_rowid();
            insert_devices(&transaction, id, &group.device_ids)?;
            transaction.commit()?;
            Ok(id as u64)
        })
    }

    pub fn update(&self, id: u64, name: &str, device_ids: Vec<String>) -> Result<bool> {
        let group = validated_group(name, device_ids)?;
        let Ok(id) = i64::try_from(id) else {
            return Ok(false);
        };
        self.database.with_connection(|connection| {
            let transaction = connection.transaction()?;
            let changed = transaction.execute(
                "UPDATE groups SET name = ?1 WHERE id = ?2",
                params![group.name, id],
            )?;
            if changed == 0 {
                return Ok(false);
            }
            transaction.execute("DELETE FROM group_devices WHERE group_id = ?1", [id])?;
            insert_devices(&transaction, id, &group.device_ids)?;
            transaction.commit()?;
            Ok(true)
        })
    }

    pub fn delete(&self, id: u64) -> Result<bool> {
        let Ok(id) = i64::try_from(id) else {
            return Ok(false);
        };
        self.database.with_connection(|connection| {
            let transaction = connection.transaction()?;
            let deleted = transaction.execute("DELETE FROM groups WHERE id = ?1", [id])? != 0;
            if deleted {
                transaction.execute(
                    "DELETE FROM automations WHERE device_id = ?1",
                    [automation_target(id as u64)],
                )?;
            }
            transaction.commit()?;
            Ok(deleted)
        })
    }
}

fn load_groups(
    connection: &rusqlite::Connection,
    headers: Vec<(u64, String)>,
) -> Result<Vec<DeviceGroup>> {
    let mut device_statement = connection
        .prepare("SELECT device_id FROM group_devices WHERE group_id = ?1 ORDER BY position")?;
    headers
        .into_iter()
        .map(|(id, name)| {
            let device_ids = device_statement
                .query_map([id as i64], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(DeviceGroup {
                id,
                name,
                device_ids,
            })
        })
        .collect()
}

fn insert_devices(
    transaction: &rusqlite::Transaction<'_>,
    group_id: i64,
    device_ids: &[String],
) -> Result<()> {
    let mut statement = transaction
        .prepare("INSERT INTO group_devices (group_id, position, device_id) VALUES (?1, ?2, ?3)")?;
    for (position, device_id) in device_ids.iter().enumerate() {
        statement.execute(params![group_id, position as i64, device_id])?;
    }
    Ok(())
}

struct ValidatedGroup {
    name: String,
    device_ids: Vec<String>,
}

fn validated_group(name: &str, device_ids: Vec<String>) -> Result<ValidatedGroup> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(invalid_input("group name cannot be empty"));
    }
    if name.chars().count() > 64 {
        return Err(invalid_input("group name cannot exceed 64 characters"));
    }
    if device_ids.is_empty() {
        return Err(invalid_input("select at least one device for the group"));
    }
    if device_ids.iter().any(String::is_empty) {
        return Err(invalid_input("group member IDs cannot be empty"));
    }
    let unique: HashSet<_> = device_ids.iter().collect();
    if unique.len() != device_ids.len() {
        return Err(invalid_input("a device can appear only once in a group"));
    }
    Ok(ValidatedGroup { name, device_ids })
}

fn invalid_input(message: &str) -> Box<dyn Error + Send + Sync> {
    Box::new(io::Error::new(io::ErrorKind::InvalidInput, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_store() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "smarthome-web-groups-{}-{unique}.sqlite3",
            std::process::id()
        ))
    }

    fn engine(path: &PathBuf) -> GroupEngine {
        GroupEngine::new(Arc::new(Database::open(path).unwrap()))
    }

    #[test]
    fn groups_survive_updates_deletes_and_restart() {
        let path = temporary_store();
        let groups = engine(&path);
        let first_id = groups
            .add(
                " Downstairs ",
                vec!["plug-1".to_owned(), "plug-2".to_owned()],
            )
            .unwrap();
        let second_id = groups.add("Outside", vec!["plug-3".to_owned()]).unwrap();
        assert!(groups
            .update(first_id, "Living room", vec!["plug-2".to_owned()])
            .unwrap());
        assert!(groups.delete(second_id).unwrap());
        drop(groups);

        let reloaded = engine(&path);
        assert_eq!(
            reloaded.groups().unwrap(),
            vec![DeviceGroup {
                id: first_id,
                name: "Living room".to_owned(),
                device_ids: vec!["plug-2".to_owned()],
            }]
        );
        assert_eq!(reloaded.get(second_id).unwrap(), None);

        drop(reloaded);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_names_and_memberships_are_rejected_without_changing_store() {
        let path = temporary_store();
        let groups = engine(&path);

        assert!(groups.add("  ", vec!["plug-1".to_owned()]).is_err());
        assert!(groups.add("Empty", Vec::new()).is_err());
        assert!(groups
            .add("Duplicate", vec!["plug-1".to_owned(), "plug-1".to_owned()])
            .is_err());
        assert!(groups.groups().unwrap().is_empty());
        assert!(path.exists());

        drop(groups);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn group_limit_matches_kasa() {
        let path = temporary_store();
        let groups = engine(&path);
        for index in 0..MAX_GROUPS {
            groups
                .add(&format!("Group {index}"), vec![format!("plug-{index}")])
                .unwrap();
        }

        assert!(groups
            .add("One too many", vec!["extra-plug".to_owned()])
            .is_err());
        assert_eq!(groups.groups().unwrap().len(), MAX_GROUPS);

        drop(groups);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn deleting_a_group_deletes_its_automations() {
        let path = temporary_store();
        let groups = engine(&path);
        let id = groups.add("Lights", vec!["plug-1".to_owned()]).unwrap();
        groups
            .database
            .with_connection(|connection| {
                connection.execute(
                    "INSERT INTO automations (device_id, trigger_json, turn_on) VALUES (?1, ?2, 1)",
                    params![automation_target(id), r#"{"type":"fixed_time","minute_of_day":420,"weekdays":[true,true,true,true,true,true,true]}"#],
                )?;
                Ok(())
            })
            .unwrap();

        assert!(groups.delete(id).unwrap());
        groups
            .database
            .with_connection(|connection| {
                let count: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM automations WHERE device_id = ?1",
                    [automation_target(id)],
                    |row| row.get(0),
                )?;
                assert_eq!(count, 0);
                Ok(())
            })
            .unwrap();

        drop(groups);
        fs::remove_file(path).unwrap();
    }
}
