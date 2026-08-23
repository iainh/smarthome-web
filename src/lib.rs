use serde::Deserialize;
use serde_json::{json, Value};
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

    fn query_command(
        &self,
        address: IpAddr,
        module: &str,
        command: &str,
        arguments: Value,
    ) -> Result<Value> {
        let response = self.query(address, &command_request(module, command, arguments))?;
        command_response(response, module, command)
    }

    fn query(&self, address: IpAddr, request: &Value) -> Result<Value> {
        let address = SocketAddr::new(address, SMART_HOME_PORT);
        let mut stream = TcpStream::connect_timeout(&address, self.timeout)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        stream.set_nodelay(true)?;
        stream.write_all(&encode_frame(request)?)?;
        read_frame(&mut stream)
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
}
