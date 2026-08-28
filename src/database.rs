use crate::automation::AutomationRule;
use crate::group::DeviceGroup;
use rusqlite::Connection;
use serde::Deserialize;
use smarthome::SmartPlug;
use std::convert::TryFrom;
use std::error::Error;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

pub struct Database {
    connection: Mutex<Connection>,
}

pub struct WeatherObservation {
    pub latitude: i32,
    pub longitude: i32,
    pub observed_at: i64,
    pub temperature: f64,
    pub apparent_temperature: f64,
    pub precipitation: f64,
    pub weather_code: u8,
    pub cloud_cover: u8,
    pub is_day: bool,
    pub shortwave_radiation: f64,
}

#[derive(Deserialize)]
struct LegacyAutomationStore {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    rules: Vec<AutomationRule>,
}

#[derive(Deserialize)]
struct LegacyGroupStore {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    groups: Vec<DeviceGroup>,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;

             CREATE TABLE IF NOT EXISTS groups (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 name TEXT NOT NULL
             );

             CREATE TABLE IF NOT EXISTS group_devices (
                 group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
                 position INTEGER NOT NULL,
                 device_id TEXT NOT NULL,
                 PRIMARY KEY (group_id, position),
                 UNIQUE (group_id, device_id)
             );

             CREATE TABLE IF NOT EXISTS automations (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 device_id TEXT NOT NULL,
                 name TEXT NOT NULL DEFAULT '',
                 enabled INTEGER NOT NULL DEFAULT 1,
                 trigger_json TEXT NOT NULL,
                 turn_on INTEGER NOT NULL,
                 last_solar_day INTEGER
             );

             CREATE TABLE IF NOT EXISTS devices (
                 device_id TEXT PRIMARY KEY,
                 address TEXT NOT NULL,
                 model TEXT NOT NULL,
                 alias TEXT NOT NULL,
                 software_version TEXT NOT NULL,
                 relay_on INTEGER NOT NULL,
                 latitude REAL,
                 longitude REAL,
                 last_seen INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS open_meteo_history (
                 latitude INTEGER NOT NULL,
                 longitude INTEGER NOT NULL,
                 observed_at INTEGER NOT NULL,
                 fetched_at INTEGER NOT NULL,
                 temperature REAL NOT NULL,
                 apparent_temperature REAL NOT NULL,
                 precipitation REAL NOT NULL,
                 weather_code INTEGER NOT NULL,
                 cloud_cover INTEGER NOT NULL,
                 is_day INTEGER NOT NULL,
                 shortwave_radiation REAL NOT NULL,
                 PRIMARY KEY (latitude, longitude, observed_at)
             );

             CREATE INDEX IF NOT EXISTS automations_device_id
                 ON automations(device_id);
             CREATE INDEX IF NOT EXISTS open_meteo_history_fetched_at
                 ON open_meteo_history(fetched_at);",
        )?;
        let automation_columns = connection
            .prepare("PRAGMA table_info(automations)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !automation_columns.iter().any(|column| column == "name") {
            connection.execute(
                "ALTER TABLE automations ADD COLUMN name TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        if !automation_columns.iter().any(|column| column == "enabled") {
            connection.execute(
                "ALTER TABLE automations ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1",
                [],
            )?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn migrate_legacy_json(
        &self,
        automation_path: impl AsRef<Path>,
        group_path: impl AsRef<Path>,
    ) -> Result<()> {
        let (needs_automations, needs_groups) = self.with_connection(|connection| {
            let automation_count: i64 =
                connection.query_row("SELECT COUNT(*) FROM automations", [], |row| row.get(0))?;
            let group_count: i64 =
                connection.query_row("SELECT COUNT(*) FROM groups", [], |row| row.get(0))?;
            Ok((automation_count == 0, group_count == 0))
        })?;
        let automations: Option<LegacyAutomationStore> = needs_automations
            .then(|| read_legacy(automation_path))
            .transpose()?
            .flatten();
        let groups: Option<LegacyGroupStore> = needs_groups
            .then(|| read_legacy(group_path))
            .transpose()?
            .flatten();
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            let automation_count: i64 =
                transaction.query_row("SELECT COUNT(*) FROM automations", [], |row| row.get(0))?;
            if automation_count == 0 {
                if let Some(store) = automations {
                    for rule in store.rules {
                        transaction.execute(
                            "INSERT INTO automations
                                 (id, device_id, trigger_json, turn_on, last_solar_day)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            rusqlite::params![
                                i64::try_from(rule.id)?,
                                rule.device_id,
                                serde_json::to_string(&rule.trigger)?,
                                rule.turn_on,
                                rule.last_solar_day,
                            ],
                        )?;
                    }
                    set_sequence(&transaction, "automations", store.next_id)?;
                }
            }

            let group_count: i64 =
                transaction.query_row("SELECT COUNT(*) FROM groups", [], |row| row.get(0))?;
            if group_count == 0 {
                if let Some(store) = groups {
                    for group in store.groups {
                        let id = i64::try_from(group.id)?;
                        transaction.execute(
                            "INSERT INTO groups (id, name) VALUES (?1, ?2)",
                            rusqlite::params![id, group.name],
                        )?;
                        for (position, device_id) in group.device_ids.into_iter().enumerate() {
                            transaction.execute(
                                "INSERT INTO group_devices (group_id, position, device_id)
                                 VALUES (?1, ?2, ?3)",
                                rusqlite::params![id, position as i64, device_id],
                            )?;
                        }
                    }
                    set_sequence(&transaction, "groups", store.next_id)?;
                }
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn devices(&self) -> Result<Vec<SmartPlug>> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT address, model, alias, device_id, software_version, relay_on,
                        latitude, longitude
                 FROM devices ORDER BY alias COLLATE NOCASE, device_id",
            )?;
            let stored = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, bool>(5)?,
                        row.get::<_, Option<f64>>(6)?,
                        row.get::<_, Option<f64>>(7)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            stored
                .into_iter()
                .map(
                    |(
                        address,
                        model,
                        alias,
                        device_id,
                        software_version,
                        relay_on,
                        latitude,
                        longitude,
                    )| {
                        Ok(SmartPlug {
                            address: address.parse::<IpAddr>()?,
                            model,
                            alias,
                            device_id,
                            software_version,
                            relay_on,
                            latitude,
                            longitude,
                        })
                    },
                )
                .collect()
        })
    }

    pub fn remember_devices(&self, devices: &[SmartPlug]) -> Result<()> {
        let seen_at = unix_timestamp()?;
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            {
                let mut statement = transaction.prepare(
                    "INSERT INTO devices
                         (device_id, address, model, alias, software_version, relay_on,
                          latitude, longitude, last_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(device_id) DO UPDATE SET
                         address = excluded.address,
                         model = excluded.model,
                         alias = excluded.alias,
                         software_version = excluded.software_version,
                         relay_on = excluded.relay_on,
                         latitude = excluded.latitude,
                         longitude = excluded.longitude,
                         last_seen = excluded.last_seen",
                )?;
                for device in devices {
                    statement.execute(rusqlite::params![
                        device.device_id,
                        device.address.to_string(),
                        device.model,
                        device.alias,
                        device.software_version,
                        device.relay_on,
                        device.latitude,
                        device.longitude,
                        seen_at,
                    ])?;
                }
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn update_relay(&self, address: IpAddr, relay_on: bool) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE devices SET relay_on = ?1 WHERE address = ?2",
                rusqlite::params![relay_on, address.to_string()],
            )?;
            Ok(())
        })
    }

    pub fn remove_device(&self, device_id: &str) -> Result<bool> {
        self.with_connection(|connection| {
            Ok(connection.execute("DELETE FROM devices WHERE device_id = ?1", [device_id])? != 0)
        })
    }

    pub fn record_weather(&self, observation: &WeatherObservation) -> Result<()> {
        let fetched_at = unix_timestamp()?;
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO open_meteo_history
                     (latitude, longitude, observed_at, fetched_at, temperature,
                      apparent_temperature, precipitation, weather_code, cloud_cover,
                      is_day, shortwave_radiation)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(latitude, longitude, observed_at) DO UPDATE SET
                     fetched_at = excluded.fetched_at,
                     temperature = excluded.temperature,
                     apparent_temperature = excluded.apparent_temperature,
                     precipitation = excluded.precipitation,
                     weather_code = excluded.weather_code,
                     cloud_cover = excluded.cloud_cover,
                     is_day = excluded.is_day,
                     shortwave_radiation = excluded.shortwave_radiation",
                rusqlite::params![
                    observation.latitude,
                    observation.longitude,
                    observation.observed_at,
                    fetched_at,
                    observation.temperature,
                    observation.apparent_temperature,
                    observation.precipitation,
                    observation.weather_code,
                    observation.cloud_cover,
                    observation.is_day,
                    observation.shortwave_radiation,
                ],
            )?;
            Ok(())
        })
    }

    pub fn purge_weather_history(&self, fetched_before: i64) -> Result<usize> {
        self.with_connection(|connection| {
            Ok(connection.execute(
                "DELETE FROM open_meteo_history WHERE fetched_at < ?1",
                [fetched_before],
            )?)
        })
    }

    pub fn with_connection<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        let mut connection = self.connection.lock().map_err(|_| {
            Box::new(io::Error::other("database lock is poisoned")) as Box<dyn Error + Send + Sync>
        })?;
        operation(&mut connection)
    }
}

pub fn unix_timestamp() -> Result<i64> {
    Ok(i64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    )?)
}

fn read_legacy<T: serde::de::DeserializeOwned>(path: impl AsRef<Path>) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(serde_json::from_slice(&contents)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn set_sequence(transaction: &rusqlite::Transaction<'_>, table: &str, next_id: u64) -> Result<()> {
    let next_id = i64::try_from(next_id)?;
    let changed = transaction.execute(
        "UPDATE sqlite_sequence SET seq = ?1 WHERE name = ?2",
        rusqlite::params![next_id, table],
    )?;
    if changed == 0 {
        transaction.execute(
            "INSERT INTO sqlite_sequence (name, seq) VALUES (?1, ?2)",
            rusqlite::params![table, next_id],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn smart_plug() -> SmartPlug {
        SmartPlug {
            address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
            model: "HS105(US)".to_owned(),
            alias: "Desk lamp".to_owned(),
            device_id: "device-1".to_owned(),
            software_version: "1.5.6".to_owned(),
            relay_on: false,
            latitude: Some(46.4106),
            longitude: Some(-81.0171),
        }
    }

    #[test]
    fn remembered_devices_survive_updates_until_removed() {
        let database = Database::open(":memory:").unwrap();
        database.remember_devices(&[smart_plug()]).unwrap();
        database.update_relay(smart_plug().address, true).unwrap();

        let devices = database.devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device_id, "device-1");
        assert!(devices[0].relay_on);

        assert!(database.remove_device("device-1").unwrap());
        assert!(!database.remove_device("device-1").unwrap());
        assert!(database.devices().unwrap().is_empty());
    }

    #[test]
    fn existing_automation_tables_gain_server_schedule_metadata() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("smarthome-web-schema-{unique}.sqlite3"));
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE automations (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    device_id TEXT NOT NULL,
                    trigger_json TEXT NOT NULL,
                    turn_on INTEGER NOT NULL,
                    last_solar_day INTEGER
                );
                INSERT INTO automations (device_id, trigger_json, turn_on)
                VALUES ('plug', '{}', 1);",
            )
            .unwrap();
        drop(connection);

        let database = Database::open(&path).unwrap();
        database
            .with_connection(|connection| {
                let metadata: (String, bool) = connection.query_row(
                    "SELECT name, enabled FROM automations WHERE device_id = 'plug'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                assert_eq!(metadata, (String::new(), true));
                Ok(())
            })
            .unwrap();

        drop(database);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn weather_observations_are_upserted_and_expired_history_is_purged() {
        let database = Database::open(":memory:").unwrap();
        let observation = WeatherObservation {
            latitude: 464_106,
            longitude: -810_171,
            observed_at: 1_777_000_000,
            temperature: 18.5,
            apparent_temperature: 17.0,
            precipitation: 0.0,
            weather_code: 2,
            cloud_cover: 40,
            is_day: true,
            shortwave_radiation: 250.0,
        };
        database.record_weather(&observation).unwrap();
        database.record_weather(&observation).unwrap();

        database
            .with_connection(|connection| {
                let count: i64 =
                    connection.query_row("SELECT COUNT(*) FROM open_meteo_history", [], |row| {
                        row.get(0)
                    })?;
                assert_eq!(count, 1);
                connection.execute("UPDATE open_meteo_history SET fetched_at = 100", [])?;
                Ok(())
            })
            .unwrap();

        assert_eq!(database.purge_weather_history(101).unwrap(), 1);
        assert_eq!(database.purge_weather_history(101).unwrap(), 0);
    }

    #[test]
    fn legacy_json_is_imported_once() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir();
        let database_path = directory.join(format!("smarthome-web-migration-{unique}.sqlite3"));
        let automation_path =
            directory.join(format!("smarthome-web-migration-{unique}-rules.json"));
        let group_path = directory.join(format!("smarthome-web-migration-{unique}-groups.json"));
        fs::write(
            &automation_path,
            r#"{
                "next_id": 4,
                "rules": [{
                    "id": 4,
                    "device_id": "plug-1",
                    "trigger": {"type": "solar", "event": "sunset", "offset_minutes": -15},
                    "turn_on": true,
                    "last_solar_day": 86400
                }]
            }"#,
        )
        .unwrap();
        fs::write(
            &group_path,
            r#"{
                "next_id": 2,
                "groups": [{
                    "id": 2,
                    "name": "Living room",
                    "device_ids": ["plug-1", "plug-2"]
                }]
            }"#,
        )
        .unwrap();

        let database = Database::open(&database_path).unwrap();
        database
            .migrate_legacy_json(&automation_path, &group_path)
            .unwrap();
        database
            .migrate_legacy_json(&automation_path, &group_path)
            .unwrap();
        database
            .with_connection(|connection| {
                let automations: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM automations WHERE id = 4 AND device_id = 'plug-1'",
                    [],
                    |row| row.get(0),
                )?;
                let devices: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM group_devices WHERE group_id = 2",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(automations, 1);
                assert_eq!(devices, 2);
                Ok(())
            })
            .unwrap();

        drop(database);
        fs::remove_file(database_path).unwrap();
        fs::remove_file(automation_path).unwrap();
        fs::remove_file(group_path).unwrap();
    }
}
