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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimeBoundary {
    Fixed {
        minute_of_day: u16,
    },
    Solar {
        event: SolarEvent,
        offset_minutes: i16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutsideWindowBehavior {
    TurnOff,
    StopControlling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveWindow {
    pub start: TimeBoundary,
    pub end: TimeBoundary,
    pub outside: OutsideWindowBehavior,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutomationTrigger {
    FixedTime {
        minute_of_day: u16,
        weekdays: [bool; 7],
    },
    Solar {
        event: SolarEvent,
        offset_minutes: i16,
        #[serde(default = "every_day")]
        weekdays: [bool; 7],
    },
    LightLevel {
        on_below: f64,
        off_above: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_window: Option<ActiveWindow>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationRule {
    pub id: u64,
    pub device_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub trigger: AutomationTrigger,
    pub turn_on: bool,
    #[serde(default)]
    pub last_solar_day: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewAutomation {
    pub device_id: String,
    pub name: String,
    pub enabled: bool,
    pub trigger: AutomationTrigger,
    pub turn_on: bool,
}

fn every_day() -> [bool; 7] {
    [true; 7]
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WeatherStatus {
    pub local_time: String,
    pub current_minute: u16,
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
    pub current_day: i64,
    pub solar_days: Vec<SolarForecastDay>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SolarForecastDay {
    pub day: i64,
    pub sunrise_minute: u16,
    pub sunset_minute: u16,
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
                "SELECT id, device_id, name, enabled, trigger_json, turn_on, last_solar_day
                 FROM automations WHERE device_id = ?1 ORDER BY id",
                [device_id],
            )
        })
    }

    pub fn add(&self, automation: NewAutomation) -> Result<u64> {
        let trigger = serde_json::to_string(&automation.trigger)?;
        self.database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO automations (device_id, name, enabled, trigger_json, turn_on)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    automation.device_id,
                    automation.name,
                    automation.enabled,
                    trigger,
                    automation.turn_on
                ],
            )?;
            Ok(u64::try_from(connection.last_insert_rowid())?)
        })
    }

    pub fn update(&self, device_id: &str, id: u64, automation: NewAutomation) -> Result<bool> {
        let Ok(id) = i64::try_from(id) else {
            return Ok(false);
        };
        let trigger = serde_json::to_string(&automation.trigger)?;
        self.database.with_connection(|connection| {
            Ok(connection.execute(
                "UPDATE automations
                 SET name = ?1,
                     last_solar_day = CASE
                         WHEN trigger_json != ?2 OR turn_on != ?3 THEN NULL
                         ELSE last_solar_day
                     END,
                     trigger_json = ?2,
                     turn_on = ?3
                 WHERE id = ?4 AND device_id = ?5",
                params![automation.name, trigger, automation.turn_on, id, device_id],
            )? != 0)
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

    pub fn set_enabled(&self, device_id: &str, id: u64, enabled: bool) -> Result<bool> {
        let Ok(id) = i64::try_from(id) else {
            return Ok(false);
        };
        self.database.with_connection(|connection| {
            Ok(connection.execute(
                "UPDATE automations SET enabled = ?1 WHERE id = ?2 AND device_id = ?3",
                params![enabled, id, device_id],
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
            .fetch_weather(coordinate, LIGHT_AVERAGE_DAYS, 7)
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
        let plugs: HashMap<_, _> = plugs
            .into_iter()
            .map(|plug| (plug.device_id.clone(), plug))
            .collect();
        let mut forecasts = HashMap::new();
        let mut triggered_timed_rules = Vec::new();
        let mut device_evaluations = HashMap::new();

        for rule in rules {
            if !rule.enabled {
                continue;
            }
            let Some(plug) = plugs.get(&rule.device_id) else {
                continue;
            };
            let Some(key) = Coordinate::from_plug(plug) else {
                continue;
            };
            if let std::collections::hash_map::Entry::Vacant(entry) = forecasts.entry(key) {
                entry.insert(self.fetch_weather(key, 1, 1).await?);
            }
            let forecast = &forecasts[&key];
            let Some(evaluation) = evaluate_rule(&rule, forecast) else {
                continue;
            };
            if let Some(day) = evaluation.trigger_day {
                triggered_timed_rules.push((rule.id, day));
            }
            device_evaluations.insert(rule.device_id, evaluation.turn_on);
        }

        for (device_id, turn_on) in device_evaluations {
            let plug = &plugs[&device_id];
            if plug.relay_on == turn_on {
                continue;
            }
            let control_client = client.clone();
            let address = plug.address;
            tokio::task::spawn_blocking(move || control_client.set_relay(address, turn_on))
                .await??;
        }

        if !triggered_timed_rules.is_empty() {
            self.database.with_connection(|connection| {
                let transaction = connection.transaction()?;
                {
                    let mut statement = transaction
                        .prepare("UPDATE automations SET last_solar_day = ?1 WHERE id = ?2")?;
                    for (id, day) in triggered_timed_rules {
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
        forecast_days: u8,
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
            ])
            .query(&[("past_days", past_days), ("forecast_days", forecast_days)])
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
                "SELECT id, device_id, name, enabled, trigger_json, turn_on, last_solar_day
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
                row.get::<_, String>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    stored_rules
        .into_iter()
        .map(
            |(id, device_id, name, enabled, trigger, turn_on, last_solar_day)| {
                Ok(AutomationRule {
                    id,
                    device_id,
                    name,
                    enabled,
                    trigger: serde_json::from_str(&trigger)?,
                    turn_on,
                    last_solar_day,
                })
            },
        )
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
    solar_days: Vec<SolarForecastDay>,
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
        let solar_days = self
            .daily
            .time
            .iter()
            .zip(&self.daily.sunrise)
            .zip(&self.daily.sunset)
            .map(|((day, sunrise), sunset)| SolarForecastDay {
                day: (*day + i64::from(self.utc_offset_seconds)).div_euclid(24 * 60 * 60),
                sunrise_minute: local_minute(*sunrise, self.utc_offset_seconds),
                sunset_minute: local_minute(*sunset, self.utc_offset_seconds),
            })
            .collect();

        Ok(WeatherSnapshot {
            time: self.current.time,
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
            solar_days,
        })
    }
}

impl WeatherSnapshot {
    fn status(&self) -> WeatherStatus {
        WeatherStatus {
            local_time: local_time(self.time, self.utc_offset_seconds),
            current_minute: local_minute(self.time, self.utc_offset_seconds),
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
            current_day: local_day(self),
            solar_days: self.solar_days.clone(),
        }
    }
}

fn local_minute(timestamp: i64, utc_offset_seconds: i32) -> u16 {
    ((timestamp + i64::from(utc_offset_seconds)).rem_euclid(24 * 60 * 60) / 60) as u16
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuleEvaluation {
    turn_on: bool,
    trigger_day: Option<i64>,
}

fn evaluate_rule(rule: &AutomationRule, weather: &WeatherSnapshot) -> Option<RuleEvaluation> {
    match rule.trigger {
        AutomationTrigger::FixedTime {
            minute_of_day,
            weekdays,
        } => {
            let day = local_day(weather);
            if rule.last_solar_day == Some(day) || !weekdays[local_weekday(day)] {
                return None;
            }
            let local_seconds = local_seconds(weather);
            let event_time = weather.time - local_seconds + i64::from(minute_of_day) * 60;
            timed_rule_evaluation(rule, weather.time, event_time, day)
        }
        AutomationTrigger::Solar {
            event,
            offset_minutes,
            weekdays,
        } => {
            let day = local_day(weather);
            if rule.last_solar_day == Some(day) || !weekdays[local_weekday(day)] {
                return None;
            }
            let event_time = match event {
                SolarEvent::Sunrise => weather.sunrise,
                SolarEvent::Sunset => weather.sunset,
            } + i64::from(offset_minutes) * 60;
            timed_rule_evaluation(rule, weather.time, event_time, day)
        }
        AutomationTrigger::LightLevel {
            on_below,
            off_above,
            active_window,
        } => {
            if let Some(window) = active_window {
                if !window_contains(window, weather) {
                    return match window.outside {
                        OutsideWindowBehavior::TurnOff => Some(RuleEvaluation {
                            turn_on: false,
                            trigger_day: None,
                        }),
                        OutsideWindowBehavior::StopControlling => None,
                    };
                }
            }
            let turn_on = if weather.shortwave_radiation <= on_below {
                true
            } else if weather.shortwave_radiation >= off_above {
                false
            } else {
                return None;
            };
            Some(RuleEvaluation {
                turn_on,
                trigger_day: None,
            })
        }
    }
}

fn timed_rule_evaluation(
    rule: &AutomationRule,
    current_time: i64,
    event_time: i64,
    day: i64,
) -> Option<RuleEvaluation> {
    (current_time >= event_time && current_time < event_time + SOLAR_TRIGGER_WINDOW_SECONDS)
        .then_some(RuleEvaluation {
            turn_on: rule.turn_on,
            trigger_day: Some(day),
        })
}

fn local_seconds(weather: &WeatherSnapshot) -> i64 {
    (weather.time + i64::from(weather.utc_offset_seconds)).rem_euclid(24 * 60 * 60)
}

fn local_day(weather: &WeatherSnapshot) -> i64 {
    (weather.time + i64::from(weather.utc_offset_seconds)).div_euclid(24 * 60 * 60)
}

fn local_weekday(day: i64) -> usize {
    (day + 4).rem_euclid(7) as usize
}

fn window_contains(window: ActiveWindow, weather: &WeatherSnapshot) -> bool {
    const DAY_SECONDS: i64 = 24 * 60 * 60;

    let current = local_seconds(weather);
    let boundary_seconds = |boundary| match boundary {
        TimeBoundary::Fixed { minute_of_day } => i64::from(minute_of_day) * 60,
        TimeBoundary::Solar {
            event,
            offset_minutes,
        } => {
            let event_time = match event {
                SolarEvent::Sunrise => weather.sunrise,
                SolarEvent::Sunset => weather.sunset,
            };
            (event_time + i64::from(weather.utc_offset_seconds) + i64::from(offset_minutes) * 60)
                .rem_euclid(DAY_SECONDS)
        }
    };
    let start = boundary_seconds(window.start);
    let end = boundary_seconds(window.end);

    match start.cmp(&end) {
        std::cmp::Ordering::Less => current >= start && current < end,
        std::cmp::Ordering::Equal => true,
        std::cmp::Ordering::Greater => current >= start || current < end,
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
            time: format_clock_time(hour as u16, 0),
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
    format_clock_time((seconds / 3_600) as u16, ((seconds % 3_600) / 60) as u16)
}

pub(crate) fn format_clock_time(hour: u16, minute: u16) -> String {
    let suffix = if hour < 12 { "AM" } else { "PM" };
    let hour = match hour % 12 {
        0 => 12,
        hour => hour,
    };
    format!("{hour}:{minute:02} {suffix}")
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
            solar_days: Vec::new(),
        }
    }

    #[test]
    fn clock_times_use_twelve_hour_format() {
        assert_eq!(format_clock_time(0, 0), "12:00 AM");
        assert_eq!(format_clock_time(11, 5), "11:05 AM");
        assert_eq!(format_clock_time(12, 0), "12:00 PM");
        assert_eq!(format_clock_time(23, 59), "11:59 PM");
    }

    #[test]
    fn solar_rule_fires_once_inside_offset_window() {
        let mut rule = AutomationRule {
            id: 1,
            device_id: "plug".to_owned(),
            name: "Sunset".to_owned(),
            enabled: true,
            trigger: AutomationTrigger::Solar {
                event: SolarEvent::Sunset,
                offset_minutes: -5,
                weekdays: every_day(),
            },
            turn_on: true,
            last_solar_day: None,
        };

        assert_eq!(
            evaluate_rule(&rule, &weather(2_700, 0.0)),
            Some(RuleEvaluation {
                turn_on: true,
                trigger_day: Some(0),
            })
        );
        rule.last_solar_day = Some(0);
        assert_eq!(evaluate_rule(&rule, &weather(2_700, 0.0)), None);
    }

    #[test]
    fn fixed_time_rule_uses_local_weekday_and_fires_once() {
        let mut rule = AutomationRule {
            id: 1,
            device_id: "plug".to_owned(),
            name: "Morning".to_owned(),
            enabled: true,
            trigger: AutomationTrigger::FixedTime {
                minute_of_day: 9 * 60,
                weekdays: [false, false, false, false, true, false, false],
            },
            turn_on: true,
            last_solar_day: None,
        };
        let mut conditions = weather(13 * 3_600, 0.0);
        conditions.utc_offset_seconds = -4 * 3_600;

        assert_eq!(
            evaluate_rule(&rule, &conditions),
            Some(RuleEvaluation {
                turn_on: true,
                trigger_day: Some(0),
            })
        );
        rule.last_solar_day = Some(0);
        assert_eq!(evaluate_rule(&rule, &conditions), None);
    }

    #[test]
    fn light_rule_uses_hysteresis_between_thresholds() {
        let rule = AutomationRule {
            id: 1,
            device_id: "plug".to_owned(),
            name: "Light".to_owned(),
            enabled: true,
            trigger: AutomationTrigger::LightLevel {
                on_below: 75.0,
                off_above: 125.0,
                active_window: None,
            },
            turn_on: false,
            last_solar_day: None,
        };

        assert!(evaluate_rule(&rule, &weather(0, 50.0)).unwrap().turn_on);
        assert_eq!(evaluate_rule(&rule, &weather(0, 100.0)), None);
        assert!(!evaluate_rule(&rule, &weather(0, 150.0)).unwrap().turn_on);
    }

    #[test]
    fn light_rule_only_controls_within_its_fixed_to_solar_window() {
        let rule = AutomationRule {
            id: 1,
            device_id: "plug".to_owned(),
            name: "Light".to_owned(),
            enabled: true,
            trigger: AutomationTrigger::LightLevel {
                on_below: 100.0,
                off_above: 125.0,
                active_window: Some(ActiveWindow {
                    start: TimeBoundary::Fixed {
                        minute_of_day: 9 * 60,
                    },
                    end: TimeBoundary::Solar {
                        event: SolarEvent::Sunset,
                        offset_minutes: 0,
                    },
                    outside: OutsideWindowBehavior::TurnOff,
                }),
            },
            turn_on: true,
            last_solar_day: None,
        };

        let mut conditions = weather(13 * 3_600, 50.0);
        conditions.utc_offset_seconds = -4 * 3_600;
        conditions.sunrise = 10 * 3_600;
        conditions.sunset = 23 * 3_600;
        assert!(evaluate_rule(&rule, &conditions).unwrap().turn_on);

        conditions.time = 23 * 3_600;
        assert!(!evaluate_rule(&rule, &conditions).unwrap().turn_on);

        conditions.time = 12 * 3_600 + 59 * 60;
        assert!(!evaluate_rule(&rule, &conditions).unwrap().turn_on);
    }

    #[test]
    fn outside_window_can_leave_the_plug_unchanged() {
        let rule = AutomationRule {
            id: 1,
            device_id: "plug".to_owned(),
            name: "Light".to_owned(),
            enabled: true,
            trigger: AutomationTrigger::LightLevel {
                on_below: 100.0,
                off_above: 125.0,
                active_window: Some(ActiveWindow {
                    start: TimeBoundary::Fixed {
                        minute_of_day: 9 * 60,
                    },
                    end: TimeBoundary::Fixed {
                        minute_of_day: 17 * 60,
                    },
                    outside: OutsideWindowBehavior::StopControlling,
                }),
            },
            turn_on: true,
            last_solar_day: None,
        };

        assert_eq!(evaluate_rule(&rule, &weather(8 * 3_600, 50.0)), None);
    }

    #[test]
    fn active_window_can_cross_midnight() {
        let window = ActiveWindow {
            start: TimeBoundary::Fixed {
                minute_of_day: 22 * 60,
            },
            end: TimeBoundary::Fixed {
                minute_of_day: 2 * 60,
            },
            outside: OutsideWindowBehavior::TurnOff,
        };

        assert!(window_contains(window, &weather(23 * 3_600, 0.0)));
        assert!(window_contains(window, &weather(3_600, 0.0)));
        assert!(!window_contains(window, &weather(12 * 3_600, 0.0)));
    }

    #[test]
    fn stored_light_rules_without_a_window_remain_all_day_rules() {
        let trigger: AutomationTrigger = serde_json::from_value(serde_json::json!({
            "type": "light_level",
            "on_below": 75.0,
            "off_above": 125.0
        }))
        .unwrap();

        assert_eq!(
            trigger,
            AutomationTrigger::LightLevel {
                on_below: 75.0,
                off_above: 125.0,
                active_window: None,
            }
        );
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
        assert_eq!(snapshot.sunrise, 122_400);
        assert_eq!(snapshot.sunset, 165_600);
        assert_eq!(snapshot.shortwave_radiation, 42.5);
        assert_eq!(
            snapshot.solar_days,
            vec![
                SolarForecastDay {
                    day: -1,
                    sunrise_minute: 6 * 60,
                    sunset_minute: 18 * 60,
                },
                SolarForecastDay {
                    day: 0,
                    sunrise_minute: 6 * 60,
                    sunset_minute: 18 * 60,
                },
            ]
        );
        let history = snapshot.previous_day_light.as_ref().unwrap();
        assert_eq!(history.points.len(), 6);
        assert_eq!(history.max_radiation, 500);
        assert_eq!(history.sunrise, "6:00 AM");
        assert_eq!(history.sunset, "6:00 PM");
        assert_eq!(history.sunrise_x, 153.3);
        assert_eq!(history.sunset_x, 289.3);
        assert_eq!(history.points[3].radiation, 500.0);
        assert_eq!(history.points[3].y, 12.0);
        assert_eq!(history.average_days, 1);
        let average_at_eight = history
            .average_points
            .iter()
            .find(|point| point.time == "8:00 AM")
            .unwrap();
        assert_eq!(average_at_eight.radiation, 500.0);
        let status = snapshot.status();
        assert_eq!(status.local_time, "11:46 PM");
        assert_eq!(status.current_minute, 23 * 60 + 46);
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
        assert_eq!(history.average_points[0].time, "12:00 PM");
        assert_eq!(history.average_points[0].radiation, 300.0);
        assert_eq!(history.average_points[0].x, 176.0);
        assert_eq!(history.average_points[0].y, 12.0);
        assert_eq!(history.max_radiation, 300);
        assert_eq!(history.points[0].radiation, 0.0);
        assert_eq!(history.points[0].y, 120.0);
    }

    #[test]
    fn automation_edits_persist_and_preserve_rule_state() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tddp-client-automations-{}-{unique}.sqlite3",
            std::process::id()
        ));
        let engine = AutomationEngine::new(Arc::new(Database::open(&path).unwrap())).unwrap();
        let id = engine
            .add(NewAutomation {
                device_id: "plug".to_owned(),
                name: "Evening".to_owned(),
                enabled: false,
                trigger: AutomationTrigger::Solar {
                    event: SolarEvent::Sunset,
                    offset_minutes: -30,
                    weekdays: every_day(),
                },
                turn_on: true,
            })
            .unwrap();
        engine
            .database
            .with_connection(|connection| {
                connection.execute(
                    "UPDATE automations SET last_solar_day = 123 WHERE id = ?1",
                    [id as i64],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(engine
            .update(
                "plug",
                id,
                NewAutomation {
                    device_id: "plug".to_owned(),
                    name: "Renamed evening".to_owned(),
                    enabled: true,
                    trigger: AutomationTrigger::Solar {
                        event: SolarEvent::Sunset,
                        offset_minutes: -30,
                        weekdays: every_day(),
                    },
                    turn_on: true,
                },
            )
            .unwrap());
        let renamed = engine.rules_for("plug").unwrap();
        assert_eq!(renamed[0].name, "Renamed evening");
        assert_eq!(renamed[0].last_solar_day, Some(123));
        assert!(!renamed[0].enabled);
        assert!(!engine
            .update(
                "another-plug",
                id,
                NewAutomation {
                    device_id: "another-plug".to_owned(),
                    name: "Wrong device".to_owned(),
                    enabled: true,
                    trigger: AutomationTrigger::FixedTime {
                        minute_of_day: 8 * 60,
                        weekdays: every_day(),
                    },
                    turn_on: false,
                },
            )
            .unwrap());
        assert!(engine
            .update(
                "plug",
                id,
                NewAutomation {
                    device_id: "plug".to_owned(),
                    name: "Weekday morning".to_owned(),
                    enabled: true,
                    trigger: AutomationTrigger::FixedTime {
                        minute_of_day: 7 * 60 + 30,
                        weekdays: [false, true, true, true, true, true, false],
                    },
                    turn_on: false,
                },
            )
            .unwrap());
        drop(engine);

        let reloaded = AutomationEngine::new(Arc::new(Database::open(&path).unwrap())).unwrap();
        let rules = reloaded.rules_for("plug").unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, 1);
        assert_eq!(rules[0].name, "Weekday morning");
        assert!(!rules[0].enabled);
        assert!(!rules[0].turn_on);
        assert_eq!(rules[0].last_solar_day, None);
        assert_eq!(
            rules[0].trigger,
            AutomationTrigger::FixedTime {
                minute_of_day: 7 * 60 + 30,
                weekdays: [false, true, true, true, true, true, false],
            }
        );

        drop(reloaded);
        fs::remove_file(path).unwrap();
    }
}
