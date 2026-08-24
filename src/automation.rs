use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tddp_client::{SmartHomeClient, SmartPlug};

const EVALUATION_INTERVAL: Duration = Duration::from_secs(5 * 60);
const SOLAR_TRIGGER_WINDOW_SECONDS: i64 = 20 * 60;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SolarEvent {
    Sunrise,
    Sunset,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutomationTrigger {
    Solar {
        event: SolarEvent,
        offset_minutes: i16,
    },
    LightLevel {
        on_below: f64,
        off_above: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationRule {
    pub id: u64,
    pub device_id: String,
    pub trigger: AutomationTrigger,
    pub turn_on: bool,
    #[serde(default)]
    pub last_solar_day: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewAutomation {
    pub device_id: String,
    pub trigger: AutomationTrigger,
    pub turn_on: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WeatherStatus {
    pub local_time: String,
    pub timezone: String,
    pub condition: &'static str,
    pub is_day: bool,
    pub shortwave_radiation: f64,
    pub cloud_cover: u8,
    pub temperature: f64,
    pub apparent_temperature: f64,
    pub precipitation: f64,
    pub sunrise: String,
    pub sunset: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AutomationStore {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    rules: Vec<AutomationRule>,
}

pub struct AutomationEngine {
    path: PathBuf,
    store: Mutex<AutomationStore>,
    weather: reqwest::Client,
}

impl AutomationEngine {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let store = match fs::read(&path) {
            Ok(contents) => serde_json::from_slice(&contents)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => AutomationStore::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            store: Mutex::new(store),
            weather: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
        })
    }

    pub fn rules_for(&self, device_id: &str) -> Result<Vec<AutomationRule>> {
        let store = self.lock_store()?;
        Ok(store
            .rules
            .iter()
            .filter(|rule| rule.device_id == device_id)
            .cloned()
            .collect())
    }

    pub fn add(&self, automation: NewAutomation) -> Result<()> {
        let mut store = self.lock_store()?;
        let mut updated = store.clone();
        updated.next_id = updated.next_id.saturating_add(1);
        let id = updated.next_id;
        updated.rules.push(AutomationRule {
            id,
            device_id: automation.device_id,
            trigger: automation.trigger,
            turn_on: automation.turn_on,
            last_solar_day: None,
        });
        self.save(&updated)?;
        *store = updated;
        Ok(())
    }

    pub fn delete(&self, device_id: &str, id: u64) -> Result<bool> {
        let mut store = self.lock_store()?;
        let mut updated = store.clone();
        let original_len = updated.rules.len();
        updated
            .rules
            .retain(|rule| rule.id != id || rule.device_id != device_id);
        if updated.rules.len() == original_len {
            return Ok(false);
        }
        self.save(&updated)?;
        *store = updated;
        Ok(true)
    }

    pub async fn weather_status(&self, plug: &SmartPlug) -> Result<WeatherStatus> {
        let coordinate = Coordinate::from_plug(plug).ok_or_else(|| {
            Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                "plug does not have location coordinates",
            )) as Box<dyn Error + Send + Sync>
        })?;
        Ok(self.fetch_weather(coordinate).await?.status())
    }

    pub async fn run(
        self: std::sync::Arc<Self>,
        client: SmartHomeClient,
        device_addresses: Vec<IpAddr>,
    ) {
        let mut interval = tokio::time::interval(EVALUATION_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = self.evaluate(&client, &device_addresses).await {
                eprintln!("automation evaluation failed: {error}");
            }
        }
    }

    async fn evaluate(&self, client: &SmartHomeClient, device_addresses: &[IpAddr]) -> Result<()> {
        let rules = {
            let store = self.lock_store()?;
            store.rules.clone()
        };
        if rules.is_empty() {
            return Ok(());
        }

        let discovery_client = client.clone();
        let device_addresses = device_addresses.to_vec();
        let plugs = tokio::task::spawn_blocking(move || {
            discovery_client.get_inventory_from(&device_addresses, Duration::from_secs(3))
        })
        .await??;
        let mut plugs: HashMap<_, _> = plugs
            .into_iter()
            .map(|plug| (plug.device_id.clone(), plug))
            .collect();
        let mut forecasts = HashMap::new();
        let mut triggered_solar_rules = Vec::new();

        for rule in rules {
            let Some(plug) = plugs.get(&rule.device_id) else {
                continue;
            };
            let Some(key) = Coordinate::from_plug(plug) else {
                continue;
            };
            if let std::collections::hash_map::Entry::Vacant(entry) = forecasts.entry(key) {
                entry.insert(self.fetch_weather(key).await?);
            }
            let forecast = &forecasts[&key];
            let Some(evaluation) = evaluate_rule(&rule, forecast) else {
                continue;
            };

            if plug.relay_on != evaluation.turn_on {
                let control_client = client.clone();
                let address = plug.address;
                tokio::task::spawn_blocking(move || {
                    control_client.set_relay(address, evaluation.turn_on)
                })
                .await??;
                if let Some(plug) = plugs.get_mut(&rule.device_id) {
                    plug.relay_on = evaluation.turn_on;
                }
            }
            if let Some(day) = evaluation.solar_day {
                triggered_solar_rules.push((rule.id, day));
            }
        }

        if !triggered_solar_rules.is_empty() {
            let mut store = self.lock_store()?;
            let mut updated = store.clone();
            for (id, day) in triggered_solar_rules {
                if let Some(rule) = updated.rules.iter_mut().find(|rule| rule.id == id) {
                    rule.last_solar_day = Some(day);
                }
            }
            self.save(&updated)?;
            *store = updated;
        }
        Ok(())
    }

    async fn fetch_weather(&self, coordinate: Coordinate) -> Result<WeatherSnapshot> {
        let response = self
            .weather
            .get("https://api.open-meteo.com/v1/forecast")
            .query(&[
                ("latitude", coordinate.latitude()),
                ("longitude", coordinate.longitude()),
            ])
            .query(&[
                (
                    "current",
                    "temperature_2m,apparent_temperature,precipitation,weather_code,cloud_cover,is_day,shortwave_radiation",
                ),
                ("daily", "sunrise,sunset"),
                ("timezone", "auto"),
                ("timeformat", "unixtime"),
                ("forecast_days", "1"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json::<WeatherResponse>()
            .await?;
        response.snapshot()
    }

    fn lock_store(&self) -> Result<std::sync::MutexGuard<'_, AutomationStore>> {
        self.store.lock().map_err(|_| {
            Box::new(io::Error::other("automation store lock is poisoned"))
                as Box<dyn Error + Send + Sync>
        })
    }

    fn save(&self, store: &AutomationStore) -> Result<()> {
        let contents = serde_json::to_vec_pretty(store)?;
        let temporary = temporary_path(&self.path);
        fs::write(&temporary, contents)?;
        fs::rename(temporary, &self.path)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Coordinate {
    latitude: i32,
    longitude: i32,
}

impl Coordinate {
    fn from_plug(plug: &SmartPlug) -> Option<Self> {
        Some(Self {
            latitude: (plug.latitude? * 10_000.0).round() as i32,
            longitude: (plug.longitude? * 10_000.0).round() as i32,
        })
    }

    fn latitude(self) -> f64 {
        f64::from(self.latitude) / 10_000.0
    }

    fn longitude(self) -> f64 {
        f64::from(self.longitude) / 10_000.0
    }
}

#[derive(Debug, Deserialize)]
struct WeatherResponse {
    utc_offset_seconds: i32,
    timezone_abbreviation: String,
    current: CurrentWeather,
    daily: DailyWeather,
}

#[derive(Debug, Deserialize)]
struct CurrentWeather {
    time: i64,
    temperature_2m: f64,
    apparent_temperature: f64,
    precipitation: f64,
    weather_code: u8,
    cloud_cover: u8,
    is_day: u8,
    shortwave_radiation: f64,
}

#[derive(Debug, Deserialize)]
struct DailyWeather {
    time: Vec<i64>,
    sunrise: Vec<i64>,
    sunset: Vec<i64>,
}

#[derive(Debug, Clone)]
struct WeatherSnapshot {
    time: i64,
    day: i64,
    sunrise: i64,
    sunset: i64,
    shortwave_radiation: f64,
    utc_offset_seconds: i32,
    timezone_abbreviation: String,
    temperature: f64,
    apparent_temperature: f64,
    precipitation: f64,
    weather_code: u8,
    cloud_cover: u8,
    is_day: bool,
}

impl WeatherResponse {
    fn snapshot(self) -> Result<WeatherSnapshot> {
        Ok(WeatherSnapshot {
            time: self.current.time,
            day: first_value(&self.daily.time, "daily time")?,
            sunrise: first_value(&self.daily.sunrise, "sunrise")?,
            sunset: first_value(&self.daily.sunset, "sunset")?,
            shortwave_radiation: self.current.shortwave_radiation,
            utc_offset_seconds: self.utc_offset_seconds,
            timezone_abbreviation: self.timezone_abbreviation,
            temperature: self.current.temperature_2m,
            apparent_temperature: self.current.apparent_temperature,
            precipitation: self.current.precipitation,
            weather_code: self.current.weather_code,
            cloud_cover: self.current.cloud_cover,
            is_day: self.current.is_day != 0,
        })
    }
}

impl WeatherSnapshot {
    fn status(&self) -> WeatherStatus {
        WeatherStatus {
            local_time: local_time(self.time, self.utc_offset_seconds),
            timezone: self.timezone_abbreviation.clone(),
            condition: weather_condition(self.weather_code),
            is_day: self.is_day,
            shortwave_radiation: self.shortwave_radiation,
            cloud_cover: self.cloud_cover,
            temperature: self.temperature,
            apparent_temperature: self.apparent_temperature,
            precipitation: self.precipitation,
            sunrise: local_time(self.sunrise, self.utc_offset_seconds),
            sunset: local_time(self.sunset, self.utc_offset_seconds),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuleEvaluation {
    turn_on: bool,
    solar_day: Option<i64>,
}

fn evaluate_rule(rule: &AutomationRule, weather: &WeatherSnapshot) -> Option<RuleEvaluation> {
    match rule.trigger {
        AutomationTrigger::Solar {
            event,
            offset_minutes,
        } => {
            if rule.last_solar_day == Some(weather.day) {
                return None;
            }
            let event_time = match event {
                SolarEvent::Sunrise => weather.sunrise,
                SolarEvent::Sunset => weather.sunset,
            } + i64::from(offset_minutes) * 60;
            (weather.time >= event_time && weather.time < event_time + SOLAR_TRIGGER_WINDOW_SECONDS)
                .then_some(RuleEvaluation {
                    turn_on: rule.turn_on,
                    solar_day: Some(weather.day),
                })
        }
        AutomationTrigger::LightLevel {
            on_below,
            off_above,
        } => {
            let turn_on = if weather.shortwave_radiation <= on_below {
                true
            } else if weather.shortwave_radiation >= off_above {
                false
            } else {
                return None;
            };
            Some(RuleEvaluation {
                turn_on,
                solar_day: None,
            })
        }
    }
}

fn first_value(values: &[i64], name: &str) -> Result<i64> {
    values.first().copied().ok_or_else(|| {
        Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Open-Meteo response omitted {name}"),
        )) as Box<dyn Error + Send + Sync>
    })
}

fn local_time(timestamp: i64, utc_offset_seconds: i32) -> String {
    let seconds = (timestamp + i64::from(utc_offset_seconds)).rem_euclid(24 * 60 * 60);
    format!("{:02}:{:02}", seconds / 3_600, (seconds % 3_600) / 60)
}

fn weather_condition(code: u8) -> &'static str {
    match code {
        0 => "Clear sky",
        1 => "Mainly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Fog",
        51 | 53 | 55 => "Drizzle",
        56 | 57 => "Freezing drizzle",
        61 | 63 | 65 => "Rain",
        66 | 67 => "Freezing rain",
        71 | 73 | 75 | 77 => "Snow",
        80..=82 => "Rain showers",
        85 | 86 => "Snow showers",
        95 => "Thunderstorm",
        96 | 99 => "Thunderstorm with hail",
        _ => "Unknown conditions",
    }
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

    fn weather(time: i64, radiation: f64) -> WeatherSnapshot {
        WeatherSnapshot {
            time,
            day: 1_000,
            sunrise: 2_000,
            sunset: 3_000,
            shortwave_radiation: radiation,
            utc_offset_seconds: 0,
            timezone_abbreviation: "UTC".to_owned(),
            temperature: 10.0,
            apparent_temperature: 9.0,
            precipitation: 0.0,
            weather_code: 0,
            cloud_cover: 0,
            is_day: true,
        }
    }

    #[test]
    fn solar_rule_fires_once_inside_offset_window() {
        let mut rule = AutomationRule {
            id: 1,
            device_id: "plug".to_owned(),
            trigger: AutomationTrigger::Solar {
                event: SolarEvent::Sunset,
                offset_minutes: -5,
            },
            turn_on: true,
            last_solar_day: None,
        };

        assert_eq!(
            evaluate_rule(&rule, &weather(2_700, 0.0)),
            Some(RuleEvaluation {
                turn_on: true,
                solar_day: Some(1_000),
            })
        );
        rule.last_solar_day = Some(1_000);
        assert_eq!(evaluate_rule(&rule, &weather(2_700, 0.0)), None);
    }

    #[test]
    fn light_rule_uses_hysteresis_between_thresholds() {
        let rule = AutomationRule {
            id: 1,
            device_id: "plug".to_owned(),
            trigger: AutomationTrigger::LightLevel {
                on_below: 75.0,
                off_above: 125.0,
            },
            turn_on: false,
            last_solar_day: None,
        };

        assert!(evaluate_rule(&rule, &weather(0, 50.0)).unwrap().turn_on);
        assert_eq!(evaluate_rule(&rule, &weather(0, 100.0)), None);
        assert!(!evaluate_rule(&rule, &weather(0, 150.0)).unwrap().turn_on);
    }

    #[test]
    fn open_meteo_response_maps_current_light_and_solar_times() {
        let response: WeatherResponse = serde_json::from_value(serde_json::json!({
            "utc_offset_seconds": -14_400,
            "timezone_abbreviation": "GMT-4",
            "current": {
                "time": 10_000,
                "temperature_2m": 13.4,
                "apparent_temperature": 11.9,
                "precipitation": 0.0,
                "weather_code": 3,
                "cloud_cover": 100,
                "is_day": 0,
                "shortwave_radiation": 42.5
            },
            "daily": {
                "time": [8_000],
                "sunrise": [9_000],
                "sunset": [12_000]
            }
        }))
        .unwrap();

        let snapshot = response.snapshot().unwrap();
        assert_eq!(snapshot.time, 10_000);
        assert_eq!(snapshot.day, 8_000);
        assert_eq!(snapshot.sunrise, 9_000);
        assert_eq!(snapshot.sunset, 12_000);
        assert_eq!(snapshot.shortwave_radiation, 42.5);
        let status = snapshot.status();
        assert_eq!(status.local_time, "22:46");
        assert_eq!(status.condition, "Overcast");
        assert_eq!(status.cloud_cover, 100);
        assert!(!status.is_day);
    }

    #[test]
    fn automation_rules_survive_engine_restart() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tddp-client-automations-{}-{unique}.json",
            std::process::id()
        ));
        let engine = AutomationEngine::load(&path).unwrap();
        engine
            .add(NewAutomation {
                device_id: "plug".to_owned(),
                trigger: AutomationTrigger::Solar {
                    event: SolarEvent::Sunset,
                    offset_minutes: -30,
                },
                turn_on: true,
            })
            .unwrap();
        drop(engine);

        let reloaded = AutomationEngine::load(&path).unwrap();
        let rules = reloaded.rules_for("plug").unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, 1);
        assert_eq!(
            rules[0].trigger,
            AutomationTrigger::Solar {
                event: SolarEvent::Sunset,
                offset_minutes: -30,
            }
        );

        fs::remove_file(path).unwrap();
    }
}
