use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::convert::TryFrom;
use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

const SMART_HOME_PORT: u16 = 9999;
const MAX_RESPONSE_LENGTH: usize = 16 * 1024;
const GET_SYSINFO: &[u8] = br#"{"system":{"get_sysinfo":{}}}"#;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Json(serde_json::Error),
    Protocol {
        module: String,
        command: String,
        code: i64,
        message: Option<String>,
    },
    InvalidInput(String),
    InvalidResponse(String),
    ResponseTooLarge(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
            Self::Protocol {
                module,
                command,
                code,
                message,
            } => write!(
                formatter,
                "{module}.{command} failed with error {code}{}",
                message
                    .as_deref()
                    .map(|message| format!(": {message}"))
                    .unwrap_or_default()
            ),
            Self::InvalidInput(message) => write!(formatter, "invalid input: {message}"),
            Self::InvalidResponse(message) => write!(formatter, "invalid response: {message}"),
            Self::ResponseTooLarge(length) => {
                write!(formatter, "response length {length} exceeds protocol limit")
            }
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmartPlug {
    pub address: IpAddr,
    pub model: String,
    pub alias: String,
    pub device_id: String,
    pub software_version: String,
    pub relay_on: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnergyReading {
    pub current_amps: f64,
    pub voltage_volts: f64,
    pub power_watts: f64,
    pub total_kwh: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DailyEnergy {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub energy_kwh: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MonthlyEnergy {
    pub year: u16,
    pub month: u8,
    pub energy_kwh: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuleSet<T> {
    pub enabled: bool,
    pub version: Option<u8>,
    pub rules: Vec<T>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CountdownRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(rename = "enable", with = "bool_u8")]
    pub enabled: bool,
    pub delay: u64,
    #[serde(rename = "act", with = "bool_u8")]
    pub turn_on: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(rename = "enable", with = "bool_u8")]
    pub enabled: bool,
    #[serde(with = "bool_u8")]
    pub repeat: bool,
    #[serde(rename = "wday", with = "weekday_array")]
    pub weekdays: [bool; 7],
    pub stime_opt: i8,
    pub smin: u16,
    pub sact: i8,
    #[serde(default = "disabled_rule_value")]
    pub etime_opt: i8,
    #[serde(default)]
    pub emin: u16,
    #[serde(default = "disabled_rule_value")]
    pub eact: i8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soffset: Option<i16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eoffset: Option<i16>,
    #[serde(default)]
    pub year: u16,
    #[serde(default)]
    pub month: u8,
    #[serde(default)]
    pub day: u8,
    #[serde(default)]
    pub latitude: f64,
    #[serde(default)]
    pub longitude: f64,
    #[serde(default)]
    pub force: u8,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AntiTheftRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(rename = "enable", with = "bool_u8")]
    pub enabled: bool,
    #[serde(with = "bool_u8")]
    pub repeat: bool,
    #[serde(rename = "wday", with = "weekday_array")]
    pub weekdays: [bool; 7],
    pub stime_opt: i8,
    pub smin: u16,
    pub etime_opt: i8,
    pub emin: u16,
    pub frequency: u16,
    pub duration: u16,
    pub lastfor: u16,
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub latitude: f64,
    pub longitude: f64,
    pub force: u8,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn disabled_rule_value() -> i8 {
    -1
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NextAction {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub action: Option<i8>,
    #[serde(default, rename = "schd_time")]
    pub scheduled_seconds: Option<u64>,
    #[serde(rename = "type")]
    pub action_type: i8,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AccessPoint {
    pub ssid: String,
    #[serde(default)]
    pub key_type: Option<u8>,
    #[serde(default)]
    pub cipher_type: Option<u8>,
    #[serde(default)]
    pub channel: Option<u8>,
    #[serde(default)]
    pub signal_level: Option<i16>,
    #[serde(default)]
    pub bssid: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CloudInfo {
    #[serde(default, rename = "binded", with = "optional_bool_u8")]
    pub bound: Option<bool>,
    #[serde(default, rename = "cld_connection", with = "optional_bool_u8")]
    pub connected: Option<bool>,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactoryResetConfirmation {
    EraseAllSettings,
}

#[derive(Debug, Clone)]
pub struct SmartHomeClient {
    timeout: Duration,
}

#[derive(Deserialize)]
struct DiscoveryResponse {
    system: SystemResponse,
}

#[derive(Deserialize)]
struct SystemResponse {
    get_sysinfo: SysInfo,
}

#[derive(Deserialize)]
struct SysInfo {
    model: String,
    alias: String,
    #[serde(rename = "deviceId")]
    device_id: String,
    sw_ver: String,
    relay_state: u8,
    err_code: i32,
}

#[derive(Deserialize)]
struct RealtimeEnergyResponse {
    current: Option<f64>,
    current_ma: Option<f64>,
    voltage: Option<f64>,
    voltage_mv: Option<f64>,
    power: Option<f64>,
    power_mw: Option<f64>,
    total: Option<f64>,
    total_wh: Option<f64>,
    energy: Option<f64>,
    energy_wh: Option<f64>,
}

#[derive(Deserialize)]
struct DayStatisticsResponse {
    day_list: Vec<DayStatistic>,
}

#[derive(Deserialize)]
struct DayStatistic {
    year: u16,
    month: u8,
    day: u8,
    energy: Option<f64>,
    energy_wh: Option<f64>,
}

#[derive(Deserialize)]
struct MonthStatisticsResponse {
    month_list: Vec<MonthStatistic>,
}

#[derive(Deserialize)]
struct MonthStatistic {
    year: u16,
    month: u8,
    energy: Option<f64>,
    energy_wh: Option<f64>,
}

#[derive(Deserialize)]
struct RulesResponse<T> {
    #[serde(default)]
    enable: u8,
    version: Option<u8>,
    rule_list: Vec<T>,
}

#[derive(Deserialize)]
struct WifiScanResponse {
    ap_list: Vec<AccessPoint>,
}

impl SmartHomeClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Broadcasts `get_sysinfo` and collects responses until the timeout expires.
    pub fn get_inventory(&self, timeout: Duration) -> Result<Vec<SmartPlug>> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_broadcast(true)?;
        socket.send_to(
            &encrypt(GET_SYSINFO),
            SocketAddr::from((Ipv4Addr::BROADCAST, SMART_HOME_PORT)),
        )?;

        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "timeout is too large"))?;
        let mut devices = Vec::new();
        let mut buffer = [0_u8; 2048];

        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            socket.set_read_timeout(Some(remaining))?;

            match socket.recv_from(&mut buffer) {
                Ok((length, peer)) => {
                    if let Some(device) = parse_device(&buffer[..length], peer.ip()) {
                        if !devices
                            .iter()
                            .any(|known: &SmartPlug| known.address == device.address)
                        {
                            devices.push(device);
                        }
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }

        devices.sort_by_key(|device| device.address);
        Ok(devices)
    }

    pub fn get_sysinfo(&self, address: IpAddr) -> Result<SmartPlug> {
        let response = self.query_command(address, "system", "get_sysinfo", json!({}))?;
        parse_sysinfo(response, address)
    }

    pub fn set_relay(&self, address: IpAddr, on: bool) -> Result<()> {
        self.query_command(
            address,
            "system",
            "set_relay_state",
            json!({ "state": u8::from(on) }),
        )?;
        Ok(())
    }

    pub fn set_led(&self, address: IpAddr, on: bool) -> Result<()> {
        self.query_command(
            address,
            "system",
            "set_led_off",
            json!({ "off": u8::from(!on) }),
        )?;
        Ok(())
    }

    pub fn reboot(&self, address: IpAddr, delay: Duration) -> Result<()> {
        self.query_command(
            address,
            "system",
            "reboot",
            json!({ "delay": delay.as_secs() }),
        )?;
        Ok(())
    }

    pub fn get_realtime_energy(&self, address: IpAddr) -> Result<EnergyReading> {
        let response = self.query_command(address, "emeter", "get_realtime", json!({}))?;
        parse_realtime_energy(response)
    }

    pub fn get_daily_energy(
        &self,
        address: IpAddr,
        year: u16,
        month: u8,
    ) -> Result<Vec<DailyEnergy>> {
        if !(1..=12).contains(&month) {
            return Err(Error::InvalidInput(format!(
                "month must be between 1 and 12, got {month}"
            )));
        }
        let response = self.query_command(
            address,
            "emeter",
            "get_daystat",
            json!({ "year": year, "month": month }),
        )?;
        parse_daily_energy(response)
    }

    pub fn get_monthly_energy(&self, address: IpAddr, year: u16) -> Result<Vec<MonthlyEnergy>> {
        let response =
            self.query_command(address, "emeter", "get_monthstat", json!({ "year": year }))?;
        parse_monthly_energy(response)
    }

    /// Permanently erases the device's stored energy history.
    pub fn erase_energy_statistics(&self, address: IpAddr) -> Result<()> {
        self.query_command(address, "emeter", "erase_emeter_stat", json!({}))?;
        Ok(())
    }

    pub fn get_countdown_rules(&self, address: IpAddr) -> Result<RuleSet<CountdownRule>> {
        self.get_rules(address, "count_down")
    }

    pub fn add_countdown_rule(
        &self,
        address: IpAddr,
        rule: &CountdownRule,
    ) -> Result<Option<String>> {
        self.add_rule(address, "count_down", rule)
    }

    pub fn edit_countdown_rule(&self, address: IpAddr, rule: &CountdownRule) -> Result<()> {
        require_rule_id(&rule.id)?;
        self.edit_rule(address, "count_down", rule)
    }

    pub fn delete_countdown_rule(&self, address: IpAddr, id: &str) -> Result<()> {
        self.delete_rule(address, "count_down", id)
    }

    pub fn delete_all_countdown_rules(&self, address: IpAddr) -> Result<()> {
        self.delete_all_rules(address, "count_down")
    }

    pub fn get_schedule_rules(&self, address: IpAddr) -> Result<RuleSet<ScheduleRule>> {
        self.get_rules(address, "schedule")
    }

    pub fn add_schedule_rule(
        &self,
        address: IpAddr,
        rule: &ScheduleRule,
    ) -> Result<Option<String>> {
        self.add_rule(address, "schedule", rule)
    }

    pub fn edit_schedule_rule(&self, address: IpAddr, rule: &ScheduleRule) -> Result<()> {
        require_rule_id(&rule.id)?;
        self.edit_rule(address, "schedule", rule)
    }

    pub fn delete_schedule_rule(&self, address: IpAddr, id: &str) -> Result<()> {
        self.delete_rule(address, "schedule", id)
    }

    pub fn delete_all_schedule_rules(&self, address: IpAddr) -> Result<()> {
        self.delete_all_rules(address, "schedule")
    }

    pub fn set_schedules_enabled(&self, address: IpAddr, enabled: bool) -> Result<()> {
        self.set_rules_enabled(address, "schedule", enabled)
    }

    pub fn get_next_schedule_action(&self, address: IpAddr) -> Result<NextAction> {
        let response = self.query_command(address, "schedule", "get_next_action", json!({}))?;
        Ok(serde_json::from_value(response)?)
    }

    /// Permanently erases schedule runtime statistics.
    pub fn erase_schedule_runtime_statistics(&self, address: IpAddr) -> Result<()> {
        self.query_command(address, "schedule", "erase_runtime_stat", json!({}))?;
        Ok(())
    }

    pub fn get_anti_theft_rules(&self, address: IpAddr) -> Result<RuleSet<AntiTheftRule>> {
        self.get_rules(address, "anti_theft")
    }

    pub fn add_anti_theft_rule(
        &self,
        address: IpAddr,
        rule: &AntiTheftRule,
    ) -> Result<Option<String>> {
        self.add_rule(address, "anti_theft", rule)
    }

    pub fn edit_anti_theft_rule(&self, address: IpAddr, rule: &AntiTheftRule) -> Result<()> {
        require_rule_id(&rule.id)?;
        self.edit_rule(address, "anti_theft", rule)
    }

    pub fn delete_anti_theft_rule(&self, address: IpAddr, id: &str) -> Result<()> {
        self.delete_rule(address, "anti_theft", id)
    }

    pub fn delete_all_anti_theft_rules(&self, address: IpAddr) -> Result<()> {
        self.delete_all_rules(address, "anti_theft")
    }

    pub fn set_anti_theft_enabled(&self, address: IpAddr, enabled: bool) -> Result<()> {
        self.set_rules_enabled(address, "anti_theft", enabled)
    }

    pub fn scan_wifi(&self, address: IpAddr, refresh: bool) -> Result<Vec<AccessPoint>> {
        let arguments = json!({ "refresh": u8::from(refresh) });
        let response = match self.query_command(address, "netif", "get_scaninfo", arguments.clone())
        {
            Ok(response) => response,
            Err(Error::Protocol { code: -1, .. }) => self.query_command(
                address,
                "smartlife.iot.common.softaponboarding",
                "get_scaninfo",
                arguments,
            )?,
            Err(error) => return Err(error),
        };
        let response: WifiScanResponse = serde_json::from_value(response)?;
        Ok(response.ap_list)
    }

    /// Changes the device's Wi-Fi network and may make it unreachable.
    pub fn configure_wifi(
        &self,
        address: IpAddr,
        ssid: &str,
        password: &str,
        key_type: u8,
    ) -> Result<()> {
        if ssid.is_empty() {
            return Err(Error::InvalidInput("Wi-Fi SSID cannot be empty".to_owned()));
        }
        let arguments = json!({
            "ssid": ssid,
            "password": password,
            "key_type": key_type,
        });
        match self.query_command(address, "netif", "set_stainfo", arguments.clone()) {
            Ok(_) => Ok(()),
            Err(Error::Protocol { code: -1, .. }) => {
                self.query_command(
                    address,
                    "smartlife.iot.common.softaponboarding",
                    "set_stainfo",
                    arguments,
                )?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub fn get_cloud_info(&self, address: IpAddr) -> Result<CloudInfo> {
        let response = self.query_command(address, "cnCloud", "get_info", json!({}))?;
        Ok(serde_json::from_value(response)?)
    }

    pub fn get_cloud_firmware_list(&self, address: IpAddr) -> Result<Value> {
        self.query_command(address, "cnCloud", "get_intl_fw_list", json!({}))
    }

    /// Changes the cloud endpoint and can break cloud connectivity.
    pub fn set_cloud_server(&self, address: IpAddr, server: &str) -> Result<()> {
        if server.is_empty() {
            return Err(Error::InvalidInput(
                "cloud server cannot be empty".to_owned(),
            ));
        }
        self.query_command(
            address,
            "cnCloud",
            "set_server_url",
            json!({ "server": server }),
        )?;
        Ok(())
    }

    /// Sends cloud credentials using the protocol's weak XOR obfuscation.
    pub fn bind_cloud(&self, address: IpAddr, username: &str, password: &str) -> Result<()> {
        self.query_command(
            address,
            "cnCloud",
            "bind",
            json!({ "username": username, "password": password }),
        )?;
        Ok(())
    }

    /// Permanently removes the device's cloud-account association.
    pub fn unbind_cloud(&self, address: IpAddr) -> Result<()> {
        self.query_command(address, "cnCloud", "unbind", json!({}))?;
        Ok(())
    }

    /// Erases all settings and returns the device to factory defaults.
    pub fn factory_reset(
        &self,
        address: IpAddr,
        _confirmation: FactoryResetConfirmation,
        delay: Duration,
    ) -> Result<()> {
        self.query_command(
            address,
            "system",
            "reset",
            json!({ "delay": delay.as_secs() }),
        )?;
        Ok(())
    }

    /// Sends an arbitrary protocol request and returns the complete response.
    pub fn query_raw(&self, address: IpAddr, request: &Value) -> Result<Value> {
        let address = SocketAddr::new(address, SMART_HOME_PORT);
        let mut stream = TcpStream::connect_timeout(&address, self.timeout)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        stream.set_nodelay(true)?;
        stream.write_all(&encode_frame(request)?)?;
        read_frame(&mut stream)
    }

    /// Sends an arbitrary JSON protocol request and returns the complete response.
    pub fn query_raw_json(&self, address: IpAddr, request: &str) -> Result<Value> {
        let request = serde_json::from_str(request)?;
        self.query_raw(address, &request)
    }

    fn get_rules<T: DeserializeOwned>(&self, address: IpAddr, module: &str) -> Result<RuleSet<T>> {
        let response = self.query_command(address, module, "get_rules", json!({}))?;
        let response: RulesResponse<T> = serde_json::from_value(response)?;
        Ok(RuleSet {
            enabled: response.enable != 0,
            version: response.version,
            rules: response.rule_list,
        })
    }

    fn add_rule<T: Serialize>(
        &self,
        address: IpAddr,
        module: &str,
        rule: &T,
    ) -> Result<Option<String>> {
        let response =
            self.query_command(address, module, "add_rule", serde_json::to_value(rule)?)?;
        Ok(response
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    fn edit_rule<T: Serialize>(&self, address: IpAddr, module: &str, rule: &T) -> Result<()> {
        self.query_command(address, module, "edit_rule", serde_json::to_value(rule)?)?;
        Ok(())
    }

    fn delete_rule(&self, address: IpAddr, module: &str, id: &str) -> Result<()> {
        if id.is_empty() {
            return Err(Error::InvalidInput("rule ID cannot be empty".to_owned()));
        }
        self.query_command(address, module, "delete_rule", json!({ "id": id }))?;
        Ok(())
    }

    fn delete_all_rules(&self, address: IpAddr, module: &str) -> Result<()> {
        self.query_command(address, module, "delete_all_rules", json!({}))?;
        Ok(())
    }

    fn set_rules_enabled(&self, address: IpAddr, module: &str, enabled: bool) -> Result<()> {
        self.query_command(
            address,
            module,
            "set_overall_enable",
            json!({ "enable": u8::from(enabled) }),
        )?;
        Ok(())
    }

    fn query_command(
        &self,
        address: IpAddr,
        module: &str,
        command: &str,
        arguments: Value,
    ) -> Result<Value> {
        let response = self.query_raw(address, &command_request(module, command, arguments))?;
        command_response(response, module, command)
    }
}

impl Default for SmartHomeClient {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
        }
    }
}

fn encrypt(plaintext: &[u8]) -> Vec<u8> {
    let mut key = 171_u8;
    plaintext
        .iter()
        .map(|byte| {
            key ^= byte;
            key
        })
        .collect()
}

fn decrypt(ciphertext: &[u8]) -> Vec<u8> {
    let mut key = 171_u8;
    ciphertext
        .iter()
        .map(|byte| {
            let plaintext = key ^ byte;
            key = *byte;
            plaintext
        })
        .collect()
}

fn parse_device(datagram: &[u8], address: IpAddr) -> Option<SmartPlug> {
    let response: DiscoveryResponse = serde_json::from_slice(&decrypt(datagram)).ok()?;
    smart_plug(response.system.get_sysinfo, address)
}

fn parse_sysinfo(response: Value, address: IpAddr) -> Result<SmartPlug> {
    let info = serde_json::from_value(response)?;
    smart_plug(info, address).ok_or_else(|| Error::Protocol {
        module: "system".to_owned(),
        command: "get_sysinfo".to_owned(),
        code: -1,
        message: Some("device returned an unsuccessful system response".to_owned()),
    })
}

fn smart_plug(info: SysInfo, address: IpAddr) -> Option<SmartPlug> {
    if info.err_code != 0 {
        return None;
    }

    Some(SmartPlug {
        address,
        model: info.model,
        alias: info.alias,
        device_id: info.device_id,
        software_version: info.sw_ver,
        relay_on: info.relay_state != 0,
    })
}

fn parse_realtime_energy(response: Value) -> Result<EnergyReading> {
    let reading: RealtimeEnergyResponse = serde_json::from_value(response)?;
    Ok(EnergyReading {
        current_amps: normalized_value(reading.current, reading.current_ma, 1_000.0, "current")?,
        voltage_volts: normalized_value(reading.voltage, reading.voltage_mv, 1_000.0, "voltage")?,
        power_watts: normalized_value(reading.power, reading.power_mw, 1_000.0, "power")?,
        total_kwh: normalized_value(
            reading.total.or(reading.energy),
            reading.total_wh.or(reading.energy_wh),
            1_000.0,
            "total energy",
        )?,
    })
}

fn parse_daily_energy(response: Value) -> Result<Vec<DailyEnergy>> {
    let response: DayStatisticsResponse = serde_json::from_value(response)?;
    response
        .day_list
        .into_iter()
        .map(|statistic| {
            Ok(DailyEnergy {
                year: statistic.year,
                month: statistic.month,
                day: statistic.day,
                energy_kwh: normalized_value(
                    statistic.energy,
                    statistic.energy_wh,
                    1_000.0,
                    "daily energy",
                )?,
            })
        })
        .collect()
}

fn parse_monthly_energy(response: Value) -> Result<Vec<MonthlyEnergy>> {
    let response: MonthStatisticsResponse = serde_json::from_value(response)?;
    response
        .month_list
        .into_iter()
        .map(|statistic| {
            Ok(MonthlyEnergy {
                year: statistic.year,
                month: statistic.month,
                energy_kwh: normalized_value(
                    statistic.energy,
                    statistic.energy_wh,
                    1_000.0,
                    "monthly energy",
                )?,
            })
        })
        .collect()
}

fn normalized_value(
    value: Option<f64>,
    scaled_value: Option<f64>,
    scale: f64,
    field: &str,
) -> Result<f64> {
    value
        .or_else(|| scaled_value.map(|value| value / scale))
        .ok_or_else(|| Error::InvalidResponse(format!("response omitted {field}")))
}

fn require_rule_id(id: &Option<String>) -> Result<&str> {
    id.as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::InvalidInput("editing a rule requires its device ID".to_owned()))
}

mod bool_u8 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &bool, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(u8::from(*value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
        Ok(u8::deserialize(deserializer)? != 0)
    }
}

mod optional_bool_u8 {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<bool>, D::Error> {
        Ok(Option::<u8>::deserialize(deserializer)?.map(|value| value != 0))
    }
}

mod weekday_array {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::convert::TryInto;

    pub fn serialize<S: Serializer>(
        weekdays: &[bool; 7],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        weekdays.map(u8::from).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[bool; 7], D::Error> {
        let weekdays = Vec::<u8>::deserialize(deserializer)?;
        let weekdays: [u8; 7] = weekdays
            .try_into()
            .map_err(|_| D::Error::custom("wday must contain seven entries"))?;
        Ok(weekdays.map(|day| day != 0))
    }
}

fn command_request(module: &str, command: &str, arguments: Value) -> Value {
    json!({ module: { command: arguments } })
}

fn command_response(response: Value, module: &str, command: &str) -> Result<Value> {
    let module_response = response
        .get(module)
        .ok_or_else(|| Error::InvalidResponse(format!("response omitted module {module}")))?;
    check_protocol_error(module_response, module, command)?;
    let command_response = module_response.get(command).ok_or_else(|| {
        Error::InvalidResponse(format!("response omitted command {module}.{command}"))
    })?;
    check_protocol_error(command_response, module, command)?;
    Ok(command_response.clone())
}

fn check_protocol_error(response: &Value, module: &str, command: &str) -> Result<()> {
    let Some(code) = response.get("err_code").and_then(Value::as_i64) else {
        return Ok(());
    };
    if code == 0 {
        return Ok(());
    }

    Err(Error::Protocol {
        module: module.to_owned(),
        command: command.to_owned(),
        code,
        message: response
            .get("err_msg")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn encode_frame(request: &Value) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(request)?;
    let length =
        u32::try_from(payload.len()).map_err(|_| Error::ResponseTooLarge(payload.len()))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&encrypt(&payload));
    Ok(frame)
}

fn read_frame(reader: &mut impl Read) -> Result<Value> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_RESPONSE_LENGTH {
        return Err(Error::ResponseTooLarge(length));
    }

    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    Ok(serde_json::from_slice(&decrypt(&payload))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_sysinfo_request_matches_smart_home_wire_format() {
        assert_eq!(
            encrypt(GET_SYSINFO),
            [
                0xd0, 0xf2, 0x81, 0xf8, 0x8b, 0xff, 0x9a, 0xf7, 0xd5, 0xef, 0x94, 0xb6, 0xd1, 0xb4,
                0xc0, 0x9f, 0xec, 0x95, 0xe6, 0x8f, 0xe1, 0x87, 0xe8, 0xca, 0xf0, 0x8b, 0xf6, 0x8b,
                0xf6,
            ]
        );
    }

    #[test]
    fn discovery_response_produces_inventory_entry() {
        let plaintext = br#"{"system":{"get_sysinfo":{"model":"HS105(US)","alias":"Desk lamp","deviceId":"device-1","sw_ver":"1.5.6","relay_state":1,"err_code":0,"new_field":"ignored"}}}"#;

        assert_eq!(
            parse_device(&encrypt(plaintext), IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))),
            Some(SmartPlug {
                address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
                model: "HS105(US)".to_owned(),
                alias: "Desk lamp".to_owned(),
                device_id: "device-1".to_owned(),
                software_version: "1.5.6".to_owned(),
                relay_on: true,
            })
        );
    }

    #[test]
    fn control_commands_match_protocol_envelopes() {
        assert_eq!(
            command_request("system", "set_relay_state", json!({ "state": 1 })),
            json!({ "system": { "set_relay_state": { "state": 1 } } })
        );
        assert_eq!(
            command_request("system", "set_led_off", json!({ "off": 0 })),
            json!({ "system": { "set_led_off": { "off": 0 } } })
        );
        assert_eq!(
            command_request("system", "reboot", json!({ "delay": 3 })),
            json!({ "system": { "reboot": { "delay": 3 } } })
        );
    }

    #[test]
    fn framed_response_is_read_exactly_and_decrypted() {
        let response = json!({ "system": { "set_relay_state": { "err_code": 0 } } });
        let mut frame = io::Cursor::new(encode_frame(&response).unwrap());

        assert_eq!(read_frame(&mut frame).unwrap(), response);
    }

    #[test]
    fn command_error_retains_device_error_details() {
        let response = json!({
            "emeter": { "get_realtime": { "err_code": -1, "err_msg": "module not support" } }
        });

        let error = command_response(response, "emeter", "get_realtime").unwrap_err();
        assert!(matches!(
            error,
            Error::Protocol {
                code: -1,
                message: Some(message),
                ..
            } if message == "module not support"
        ));
    }

    #[test]
    fn scaled_energy_response_is_normalized_to_si_units() {
        let reading = parse_realtime_energy(json!({
            "err_code": 0,
            "current_ma": 296,
            "voltage_mv": 230_123,
            "power_mw": 63_499,
            "total_wh": 12_068
        }))
        .unwrap();

        assert_eq!(
            reading,
            EnergyReading {
                current_amps: 0.296,
                voltage_volts: 230.123,
                power_watts: 63.499,
                total_kwh: 12.068,
            }
        );
    }

    #[test]
    fn energy_history_accepts_kwh_and_wh_firmware_formats() {
        assert_eq!(
            parse_daily_energy(json!({
                "day_list": [
                    { "year": 2026, "month": 8, "day": 1, "energy": 0.026 },
                    { "year": 2026, "month": 8, "day": 2, "energy_wh": 109 }
                ]
            }))
            .unwrap(),
            vec![
                DailyEnergy {
                    year: 2026,
                    month: 8,
                    day: 1,
                    energy_kwh: 0.026,
                },
                DailyEnergy {
                    year: 2026,
                    month: 8,
                    day: 2,
                    energy_kwh: 0.109,
                },
            ]
        );
        assert_eq!(
            parse_monthly_energy(json!({
                "month_list": [{ "year": 2026, "month": 8, "energy_wh": 1_582 }]
            }))
            .unwrap(),
            vec![MonthlyEnergy {
                year: 2026,
                month: 8,
                energy_kwh: 1.582,
            }]
        );
    }

    #[test]
    fn schedule_rule_serializes_protocol_booleans_and_preserves_extra_fields() {
        let mut extra = Map::new();
        extra.insert("custom".to_owned(), json!(7));
        let rule = ScheduleRule {
            id: None,
            name: "lights on".to_owned(),
            enabled: true,
            repeat: true,
            weekdays: [true, false, false, true, true, false, false],
            stime_opt: 0,
            smin: 1014,
            sact: 1,
            etime_opt: -1,
            emin: 0,
            eact: -1,
            soffset: None,
            eoffset: None,
            year: 0,
            month: 0,
            day: 0,
            latitude: 0.0,
            longitude: 0.0,
            force: 0,
            extra,
        };

        assert_eq!(
            serde_json::to_value(rule).unwrap(),
            json!({
                "name": "lights on", "enable": 1, "repeat": 1,
                "wday": [1, 0, 0, 1, 1, 0, 0],
                "stime_opt": 0, "smin": 1014, "sact": 1,
                "etime_opt": -1, "emin": 0, "eact": -1,
                "year": 0, "month": 0, "day": 0,
                "latitude": 0.0, "longitude": 0.0, "force": 0,
                "custom": 7
            })
        );
    }

    #[test]
    fn rule_responses_deserialize_ids_and_unknown_fields() {
        let response: RulesResponse<CountdownRule> = serde_json::from_value(json!({
            "enable": 1,
            "version": 2,
            "rule_list": [{
                "id": "opaque-id", "name": "turn on", "enable": 1,
                "delay": 1800, "act": 1, "firmware_field": "retained"
            }]
        }))
        .unwrap();

        assert_eq!(response.enable, 1);
        assert_eq!(response.version, Some(2));
        assert_eq!(response.rule_list[0].id.as_deref(), Some("opaque-id"));
        assert_eq!(
            response.rule_list[0].extra.get("firmware_field"),
            Some(&json!("retained"))
        );
    }

    #[test]
    fn schedule_response_allows_firmware_to_omit_optional_fields() {
        let rule: ScheduleRule = serde_json::from_value(json!({
            "id": "opaque-id",
            "name": "Schedule Rule",
            "enable": 1,
            "repeat": 1,
            "wday": [0, 1, 1, 1, 1, 1, 0],
            "stime_opt": 0,
            "smin": 435,
            "sact": 1,
            "eact": -1
        }))
        .unwrap();

        assert_eq!(rule.etime_opt, -1);
        assert_eq!(rule.emin, 0);
        assert_eq!(rule.year, 0);
        assert_eq!(rule.latitude, 0.0);
    }

    #[test]
    fn editing_rule_without_id_is_rejected() {
        assert!(matches!(
            require_rule_id(&None),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn wifi_scan_and_cloud_responses_tolerate_firmware_fields() {
        let scan: WifiScanResponse = serde_json::from_value(json!({
            "ap_list": [{
                "ssid": "network", "key_type": 3, "channel": 6,
                "future_field": true
            }]
        }))
        .unwrap();
        assert_eq!(scan.ap_list[0].ssid, "network");
        assert_eq!(
            scan.ap_list[0].extra.get("future_field"),
            Some(&json!(true))
        );

        let cloud: CloudInfo = serde_json::from_value(json!({
            "binded": 1,
            "cld_connection": 0,
            "server": "devs.tplinkcloud.com",
            "tcspStatus": 1
        }))
        .unwrap();
        assert_eq!(cloud.bound, Some(true));
        assert_eq!(cloud.connected, Some(false));
        assert_eq!(cloud.extra.get("tcspStatus"), Some(&json!(1)));
    }

    #[test]
    fn provisioning_cloud_and_reset_commands_match_protocol_envelopes() {
        assert_eq!(
            command_request(
                "netif",
                "set_stainfo",
                json!({ "ssid": "wifi", "password": "secret", "key_type": 3 })
            ),
            json!({
                "netif": { "set_stainfo": {
                    "ssid": "wifi", "password": "secret", "key_type": 3
                }}
            })
        );
        assert_eq!(
            command_request("cnCloud", "unbind", json!({})),
            json!({ "cnCloud": { "unbind": {} } })
        );
        assert_eq!(
            command_request("system", "reset", json!({ "delay": 1 })),
            json!({ "system": { "reset": { "delay": 1 } } })
        );
    }
}
