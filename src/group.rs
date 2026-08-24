use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_GROUPS: usize = 50;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceGroup {
    pub id: u64,
    pub name: String,
    pub device_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GroupStore {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    groups: Vec<DeviceGroup>,
}

pub struct GroupEngine {
    path: PathBuf,
    store: Mutex<GroupStore>,
}

impl GroupEngine {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let store = match fs::read(&path) {
            Ok(contents) => serde_json::from_slice(&contents)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => GroupStore::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            store: Mutex::new(store),
        })
    }

    pub fn groups(&self) -> Result<Vec<DeviceGroup>> {
        Ok(self.lock_store()?.groups.clone())
    }

    pub fn get(&self, id: u64) -> Result<Option<DeviceGroup>> {
        Ok(self
            .lock_store()?
            .groups
            .iter()
            .find(|group| group.id == id)
            .cloned())
    }

    pub fn add(&self, name: &str, device_ids: Vec<String>) -> Result<u64> {
        let group = validated_group(name, device_ids)?;
        let mut store = self.lock_store()?;
        if store.groups.len() >= MAX_GROUPS {
            return Err(invalid_input("no more than 50 groups can be created"));
        }

        let mut updated = store.clone();
        updated.next_id = updated.next_id.saturating_add(1);
        let id = updated.next_id;
        updated.groups.push(DeviceGroup {
            id,
            name: group.name,
            device_ids: group.device_ids,
        });
        self.save(&updated)?;
        *store = updated;
        Ok(id)
    }

    pub fn update(&self, id: u64, name: &str, device_ids: Vec<String>) -> Result<bool> {
        let group = validated_group(name, device_ids)?;
        let mut store = self.lock_store()?;
        let mut updated = store.clone();
        let Some(existing) = updated.groups.iter_mut().find(|existing| existing.id == id) else {
            return Ok(false);
        };
        existing.name = group.name;
        existing.device_ids = group.device_ids;
        self.save(&updated)?;
        *store = updated;
        Ok(true)
    }

    pub fn delete(&self, id: u64) -> Result<bool> {
        let mut store = self.lock_store()?;
        let mut updated = store.clone();
        let original_len = updated.groups.len();
        updated.groups.retain(|group| group.id != id);
        if updated.groups.len() == original_len {
            return Ok(false);
        }
        self.save(&updated)?;
        *store = updated;
        Ok(true)
    }

    fn lock_store(&self) -> Result<std::sync::MutexGuard<'_, GroupStore>> {
        self.store.lock().map_err(|_| {
            Box::new(io::Error::other("group store lock is poisoned"))
                as Box<dyn Error + Send + Sync>
        })
    }

    fn save(&self, store: &GroupStore) -> Result<()> {
        let contents = serde_json::to_vec_pretty(store)?;
        let temporary = temporary_path(&self.path);
        fs::write(&temporary, contents)?;
        fs::rename(temporary, &self.path)?;
        Ok(())
    }
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

fn temporary_path(path: &Path) -> PathBuf {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    temporary.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_store() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "tddp-client-groups-{}-{unique}.json",
            std::process::id()
        ))
    }

    #[test]
    fn groups_survive_updates_deletes_and_restart() {
        let path = temporary_store();
        let groups = GroupEngine::load(&path).unwrap();
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

        let reloaded = GroupEngine::load(&path).unwrap();
        assert_eq!(
            reloaded.groups().unwrap(),
            vec![DeviceGroup {
                id: first_id,
                name: "Living room".to_owned(),
                device_ids: vec!["plug-2".to_owned()],
            }]
        );
        assert_eq!(reloaded.get(second_id).unwrap(), None);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_names_and_memberships_are_rejected_without_changing_store() {
        let path = temporary_store();
        let groups = GroupEngine::load(&path).unwrap();

        assert!(groups.add("  ", vec!["plug-1".to_owned()]).is_err());
        assert!(groups.add("Empty", Vec::new()).is_err());
        assert!(groups
            .add("Duplicate", vec!["plug-1".to_owned(), "plug-1".to_owned()])
            .is_err());
        assert!(groups.groups().unwrap().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn group_limit_matches_kasa() {
        let path = temporary_store();
        let groups = GroupEngine::load(&path).unwrap();
        for index in 0..MAX_GROUPS {
            groups
                .add(&format!("Group {index}"), vec![format!("plug-{index}")])
                .unwrap();
        }

        assert!(groups
            .add("One too many", vec!["extra-plug".to_owned()])
            .is_err());
        assert_eq!(groups.groups().unwrap().len(), MAX_GROUPS);

        fs::remove_file(path).unwrap();
    }
}
