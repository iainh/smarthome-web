use crate::database::{Database, WeatherObservation};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::error::Error;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tddp_client::{SmartHomeClient, SmartPlug};

const EVALUATION_INTERVAL: Duration = Duration::from_secs(5 * 60);
const SOLAR_TRIGGER_WINDOW_SECONDS: i64 = 20 * 60;
const LIGHT_AVERAGE_DAYS: u8 = 30;

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
    pub previous_day_light: Option<LightHistory>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LightHistory {
    pub points: Vec<LightPoint>,
    pub average_points: Vec<LightPoint>,
    pub average_days: usize,
    pub max_radiation: u32,
    pub mid_radiation: u32,
    pub sunrise_x: f64,
    pub sunset_x: f64,
    pub sunrise: String,
    pub sunset: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LightPoint {
    pub x: f64,
    pub y: f64,
    pub time: String,
    pub radiation: f64,
}

pub struct AutomationEngine {
    database: Arc<Database>,
    weather: reqwest::Client,
}

impl AutomationEngine {
    pub fn new(database: Arc<Database>) -> Result<Self> {
        Ok(Self {
            database,
            weather: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
        })
    }

    pub fn rules_for(&self, device_id: &str) -> Result<Vec<AutomationRule>> {
        self.database.with_connection(|connection| {
            load_rules(
                connection,
                "SELECT id, device_id, trigger_json, turn_on, last_solar_day
                 FROM automations WHERE device_id = ?1 ORDER BY id",
                [device_id],
            )
        })
    }

    pub fn add(&self, automation: NewAutomation) -> Result<()> {
        let trigger = serde_json::to_string(&automation.trigger)?;
        self.database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO automations (device_id, trigger_json, turn_on)
                 VALUES (?1, ?2, ?3)",
                params![automation.device_id, trigger, automation.turn_on],
            )?;
            Ok(())
        })
    }

    pub fn delete(&self, device_id: &str, id: u64) -> Result<bool> {
        let Ok(id) = i64::try_from(id) else {
            return Ok(false);
        };
        self.database.with_connection(|connection| {
            Ok(connection.execute(
                "DELETE FROM automations WHERE id = ?1 AND device_id = ?2",
                params![id, device_id],
            )? != 0)
        })
    }

    pub async fn weather_status(&self, plug: &SmartPlug) -> Result<WeatherStatus> {
        let coordinate = Coordinate::from_plug(plug).ok_or_else(|| {
            Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                "plug does not have location coordinates",
            )) as Box<dyn Error + Send + Sync>
        })?;
        Ok(self
            .fetch_weather(coordinate, LIGHT_AVERAGE_DAYS)
            .await?
            .status())
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
        let rules = self.all_rules()?;
        if rules.is_empty() {
            return Ok(());
        }

        let discovery_client = client.clone();
        let mut device_addresses = device_addresses.to_vec();
        device_addresses.extend(
            self.database
                .devices()?
                .into_iter()
                .map(|device| device.address),
        );
        device_addresses.sort_unstable();
        device_addresses.dedup();
        let plugs = tokio::task::spawn_blocking(move || {
            discovery_client.get_inventory_from(&device_addresses, Duration::from_secs(3))
        })
        .await??;
        self.database.remember_devices(&plugs)?;
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
                entry.insert(self.fetch_weather(key, 1).await?);
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
            self.database.with_connection(|connection| {
                let transaction = connection.transaction()?;
                {
                    let mut statement = transaction
                        .prepare("UPDATE automations SET last_solar_day = ?1 WHERE id = ?2")?;
                    for (id, day) in triggered_solar_rules {
                        statement.execute(params![day, id as i64])?;
                    }
                }
                transaction.commit()?;
                Ok(())
            })?;
        }
        Ok(())
    }

    async fn fetch_weather(
        &self,
        coordinate: Coordinate,
        past_days: u8,
    ) -> Result<WeatherSnapshot> {
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
                ("hourly", "shortwave_radiation"),
                ("timezone", "auto"),
                ("timeformat", "unixtime"),
                ("forecast_days", "1"),
            ])
            .query(&[("past_days", past_days)])
            .send()
            .await?
            .error_for_status()?
            .json::<WeatherResponse>()
            .await?;
        let snapshot = response.snapshot()?;
        self.database.record_weather(&WeatherObservation {
            latitude: coordinate.latitude,
            longitude: coordinate.longitude,
            observed_at: snapshot.time,
            temperature: snapshot.temperature,
            apparent_temperature: snapshot.apparent_temperature,
            precipitation: snapshot.precipitation,
            weather_code: snapshot.weather_code,
            cloud_cover: snapshot.cloud_cover,
            is_day: snapshot.is_day,
            shortwave_radiation: snapshot.shortwave_radiation,
        })?;
        Ok(snapshot)
    }

    fn all_rules(&self) -> Result<Vec<AutomationRule>> {
        self.database.with_connection(|connection| {
            load_rules(
                connection,
                "SELECT id, device_id, trigger_json, turn_on, last_solar_day
                 FROM automations ORDER BY id",
                [],
            )
        })
    }
}

fn load_rules(
    connection: &rusqlite::Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
) -> Result<Vec<AutomationRule>> {
    let mut statement = connection.prepare(sql)?;
    let stored_rules = statement
        .query_map(parameters, |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    stored_rules
        .into_iter()
        .map(|(id, device_id, trigger, turn_on, last_solar_day)| {
            Ok(AutomationRule {
                id,
                device_id,
                trigger: serde_json::from_str(&trigger)?,
                turn_on,
                last_solar_day,
            })
        })
        .collect()
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
    hourly: HourlyWeather,
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

#[derive(Debug, Deserialize)]
struct HourlyWeather {
    time: Vec<i64>,
    shortwave_radiation: Vec<Option<f64>>,
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
    previous_day_light: Option<LightHistory>,
}

impl WeatherResponse {
    fn snapshot(self) -> Result<WeatherSnapshot> {
        let day_index = self
            .daily
            .time
            .iter()
            .rposition(|day| *day <= self.current.time)
            .ok_or_else(|| invalid_weather_data("daily time"))?;
        let day = value_at(&self.daily.time, day_index, "daily time")?;
        let sunrise = value_at(&self.daily.sunrise, day_index, "sunrise")?;
        let sunset = value_at(&self.daily.sunset, day_index, "sunset")?;
        let previous_day_light = day_index.checked_sub(1).and_then(|previous_index| {
            light_history(
                &self.hourly,
                *self.daily.time.first()?,
                value_at(&self.daily.time, previous_index, "previous daily time").ok()?..day,
                value_at(&self.daily.sunrise, previous_index, "previous sunrise").ok()?
                    ..value_at(&self.daily.sunset, previous_index, "previous sunset").ok()?,
                self.utc_offset_seconds,
                day_index,
            )
        });

        Ok(WeatherSnapshot {
            time: self.current.time,
            day,
            sunrise,
            sunset,
            shortwave_radiation: self.current.shortwave_radiation,
            utc_offset_seconds: self.utc_offset_seconds,
            timezone_abbreviation: self.timezone_abbreviation,
            temperature: self.current.temperature_2m,
            apparent_temperature: self.current.apparent_temperature,
            precipitation: self.current.precipitation,
            weather_code: self.current.weather_code,
            cloud_cover: self.current.cloud_cover,
            is_day: self.current.is_day != 0,
            previous_day_light,
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
            previous_day_light: self.previous_day_light.clone(),
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

fn light_history(
    hourly: &HourlyWeather,
    history_start: i64,
    day: std::ops::Range<i64>,
    solar: std::ops::Range<i64>,
    utc_offset_seconds: i32,
    average_days: usize,
) -> Option<LightHistory> {
    let day_start = day.start;
    let day_end = day.end;
    let readings: Vec<_> = hourly
        .time
        .iter()
        .copied()
        .zip(hourly.shortwave_radiation.iter().copied())
        .filter_map(|(time, radiation)| {
            (time >= day_start && time < day_end).then_some((time, radiation?))
        })
        .collect();
    if readings.is_empty() || day_end <= day_start {
        return None;
    }

    let mut hourly_totals = [0.0; 24];
    let mut hourly_counts = [0_u32; 24];
    for (time, radiation) in hourly
        .time
        .iter()
        .copied()
        .zip(hourly.shortwave_radiation.iter().copied())
        .filter_map(|(time, radiation)| {
            (time >= history_start && time < day_end).then_some((time, radiation?))
        })
    {
        let hour =
            ((time + i64::from(utc_offset_seconds)).rem_euclid(24 * 60 * 60) / (60 * 60)) as usize;
        hourly_totals[hour] += radiation;
        hourly_counts[hour] += 1;
    }
    let averages: Vec<_> = hourly_totals
        .iter()
        .copied()
        .zip(hourly_counts.iter().copied())
        .enumerate()
        .filter_map(|(hour, (total, count))| {
            (count > 0).then_some((hour, total / f64::from(count)))
        })
        .collect();
    let observed_max = readings
        .iter()
        .map(|(_, radiation)| *radiation)
        .chain(averages.iter().map(|(_, radiation)| *radiation))
        .fold(0.0_f64, f64::max);
    let max_radiation = ((observed_max / 100.0).ceil().max(1.0) * 100.0) as u32;
    let x = |time| 40.0 + (time - day_start) as f64 / (day_end - day_start) as f64 * 272.0;
    let y = |radiation: f64| rounded(120.0 - radiation.max(0.0) / f64::from(max_radiation) * 108.0);
    let points = readings
        .into_iter()
        .map(|(time, radiation)| LightPoint {
            x: rounded(x(time)),
            y: y(radiation),
            time: local_time(time, utc_offset_seconds),
            radiation,
        })
        .collect();
    let average_points = averages
        .into_iter()
        .map(|(hour, radiation)| LightPoint {
            x: rounded(40.0 + hour as f64 / 24.0 * 272.0),
            y: y(radiation),
            time: format!("{hour:02}:00"),
            radiation: rounded(radiation),
        })
        .collect();

    Some(LightHistory {
        points,
        average_points,
        average_days,
        max_radiation,
        mid_radiation: max_radiation / 2,
        sunrise_x: rounded(x(solar.start)),
        sunset_x: rounded(x(solar.end)),
        sunrise: local_time(solar.start, utc_offset_seconds),
        sunset: local_time(solar.end, utc_offset_seconds),
    })
}

fn rounded(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn value_at(values: &[i64], index: usize, name: &str) -> Result<i64> {
    values
        .get(index)
        .copied()
        .ok_or_else(|| invalid_weather_data(name))
}

fn invalid_weather_data(name: &str) -> Box<dyn Error + Send + Sync> {
    Box::new(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Open-Meteo response omitted {name}"),
    ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use std::fs;
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
            previous_day_light: None,
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
                "time": 100_000,
                "temperature_2m": 13.4,
                "apparent_temperature": 11.9,
                "precipitation": 0.0,
                "weather_code": 3,
                "cloud_cover": 100,
                "is_day": 0,
                "shortwave_radiation": 42.5
            },
            "daily": {
                "time": [0, 86_400],
                "sunrise": [36_000, 122_400],
                "sunset": [79_200, 165_600]
            },
            "hourly": {
                "time": [0, 21_600, 36_000, 43_200, 64_800, 82_800, 86_400],
                "shortwave_radiation": [0.0, 0.0, 100.0, 500.0, 50.0, 0.0, 0.0]
            }
        }))
        .unwrap();

        let snapshot = response.snapshot().unwrap();
        assert_eq!(snapshot.time, 100_000);
        assert_eq!(snapshot.day, 86_400);
        assert_eq!(snapshot.sunrise, 122_400);
        assert_eq!(snapshot.sunset, 165_600);
        assert_eq!(snapshot.shortwave_radiation, 42.5);
        let history = snapshot.previous_day_light.as_ref().unwrap();
        assert_eq!(history.points.len(), 6);
        assert_eq!(history.max_radiation, 500);
        assert_eq!(history.sunrise, "06:00");
        assert_eq!(history.sunset, "18:00");
        assert_eq!(history.sunrise_x, 153.3);
        assert_eq!(history.sunset_x, 289.3);
        assert_eq!(history.points[3].radiation, 500.0);
        assert_eq!(history.points[3].y, 12.0);
        assert_eq!(history.average_days, 1);
        let average_at_eight = history
            .average_points
            .iter()
            .find(|point| point.time == "08:00")
            .unwrap();
        assert_eq!(average_at_eight.radiation, 500.0);
        let status = snapshot.status();
        assert_eq!(status.local_time, "23:46");
        assert_eq!(status.condition, "Overcast");
        assert_eq!(status.cloud_cover, 100);
        assert!(!status.is_day);
    }

    #[test]
    fn light_history_averages_each_local_hour_and_uses_a_shared_scale() {
        const DAY: i64 = 24 * 60 * 60;
        let hourly = HourlyWeather {
            time: vec![12 * 3_600, DAY + 12 * 3_600, 2 * DAY + 12 * 3_600],
            shortwave_radiation: vec![Some(300.0), Some(600.0), Some(0.0)],
        };

        let history = light_history(
            &hourly,
            0,
            2 * DAY..3 * DAY,
            2 * DAY + 6 * 3_600..2 * DAY + 18 * 3_600,
            0,
            3,
        )
        .unwrap();

        assert_eq!(history.average_days, 3);
        assert_eq!(history.average_points.len(), 1);
        assert_eq!(history.average_points[0].time, "12:00");
        assert_eq!(history.average_points[0].radiation, 300.0);
        assert_eq!(history.average_points[0].x, 176.0);
        assert_eq!(history.average_points[0].y, 12.0);
        assert_eq!(history.max_radiation, 300);
        assert_eq!(history.points[0].radiation, 0.0);
        assert_eq!(history.points[0].y, 120.0);
    }

    #[test]
    fn automation_rules_survive_engine_restart() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tddp-client-automations-{}-{unique}.sqlite3",
            std::process::id()
        ));
        let engine = AutomationEngine::new(Arc::new(Database::open(&path).unwrap())).unwrap();
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

        let reloaded = AutomationEngine::new(Arc::new(Database::open(&path).unwrap())).unwrap();
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

        drop(reloaded);
        fs::remove_file(path).unwrap();
    }
}
