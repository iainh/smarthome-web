use serde_json::{json, Map, Value};
use smarthome::SmartPlug;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

const MOCK_PORT: u16 = 9999;
const MAX_FRAME_LENGTH: usize = 16 * 1024;

pub fn groups() -> Vec<(&'static str, Vec<String>)> {
    vec![(
        "Living room",
        vec!["mock-outlet-2".to_owned(), "mock-outlet-4".to_owned()],
    )]
}

struct Outlet {
    plug: SmartPlug,
    countdown_rules: Vec<Value>,
    schedule_rules: Vec<Value>,
    next_rule_id: u64,
}

pub fn start() -> io::Result<Vec<SmartPlug>> {
    let outlets = mock_outlets();
    let inventory = outlets.values().map(|outlet| outlet.plug.clone()).collect();
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, MOCK_PORT))?;
    let outlets = Arc::new(Mutex::new(outlets));
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            match connection {
                Ok(stream) => {
                    let outlets = outlets.clone();
                    std::thread::spawn(move || {
                        if let Err(error) = serve_connection(stream, &outlets) {
                            eprintln!("mock outlet request failed: {error}");
                        }
                    });
                }
                Err(error) => eprintln!("mock outlet connection failed: {error}"),
            }
        }
    });
    println!("Mock outlets listening on 127.0.0.2–127.0.0.4:{MOCK_PORT}");
    Ok(inventory)
}

fn mock_outlets() -> HashMap<IpAddr, Outlet> {
    vec![
        mock_outlet(2, "Living room lamp", "HS220(US)", true, Some(65)),
        mock_outlet(3, "Coffee maker", "HS103(US)", false, None),
        mock_outlet(4, "Patio lights", "KP115(US)", true, None),
    ]
    .into_iter()
    .map(|outlet| (outlet.plug.address, outlet))
    .collect()
}

fn mock_outlet(
    octet: u8,
    alias: &str,
    model: &str,
    relay_on: bool,
    brightness: Option<u8>,
) -> Outlet {
    let schedule_rules = (octet == 4)
        .then(|| {
            vec![json!({
                "id": "mock-schedule-1",
                "name": "Patio at sunset",
                "enable": 1,
                "repeat": 1,
                "wday": [1, 1, 1, 1, 1, 1, 1],
                "stime_opt": 2,
                "smin": 0,
                "sact": 1,
                "etime_opt": -1,
                "soffset": -15
            })]
        })
        .unwrap_or_default();
    Outlet {
        plug: SmartPlug {
            address: IpAddr::V4(Ipv4Addr::new(127, 0, 0, octet)),
            model: model.to_owned(),
            alias: alias.to_owned(),
            device_id: format!("mock-outlet-{octet}"),
            software_version: "1.0.0-mock".to_owned(),
            relay_on,
            brightness,
            latitude: Some(46.4917),
            longitude: Some(-80.9930),
        },
        countdown_rules: Vec::new(),
        schedule_rules,
        next_rule_id: 1,
    }
}

fn serve_connection(
    mut stream: TcpStream,
    outlets: &Arc<Mutex<HashMap<IpAddr, Outlet>>>,
) -> io::Result<()> {
    let address = stream.local_addr()?.ip();
    let request = read_frame(&mut stream)?;
    let response = {
        let mut outlets = outlets
            .lock()
            .map_err(|_| io::Error::other("mock outlet state lock is poisoned"))?;
        match outlets.get_mut(&address) {
            Some(outlet) => respond(outlet, &request),
            None => error_response(&request, "unknown mock outlet address"),
        }
    };
    write_frame(&mut stream, &response)
}

fn respond(outlet: &mut Outlet, request: &Value) -> Value {
    let Some((module, command, arguments)) = request_command(request) else {
        return json!({ "system": { "err_code": -1, "err_msg": "invalid request" } });
    };
    let result = match (module, command) {
        ("system", "get_sysinfo") => json!({
            "model": outlet.plug.model,
            "alias": outlet.plug.alias,
            "deviceId": outlet.plug.device_id,
            "sw_ver": outlet.plug.software_version,
            "relay_state": u8::from(outlet.plug.relay_on),
            "brightness": outlet.plug.brightness,
            "latitude": outlet.plug.latitude,
            "longitude": outlet.plug.longitude,
            "err_code": 0
        }),
        ("system", "set_relay_state") => {
            outlet.plug.relay_on = arguments
                .get("state")
                .and_then(Value::as_u64)
                .unwrap_or_default()
                != 0;
            json!({ "err_code": 0 })
        }
        ("smartlife.iot.dimmer", "set_brightness") if outlet.plug.brightness.is_some() => {
            outlet.plug.brightness = arguments
                .get("brightness")
                .and_then(Value::as_u64)
                .and_then(|brightness| u8::try_from(brightness).ok());
            json!({ "err_code": 0 })
        }
        ("count_down", "get_rules") => rules_response(&outlet.countdown_rules),
        ("count_down", "add_rule") => {
            let id = format!("mock-countdown-{}", outlet.next_rule_id);
            outlet.next_rule_id += 1;
            let mut rule = arguments.as_object().cloned().unwrap_or_default();
            rule.insert("id".to_owned(), Value::String(id.clone()));
            outlet.countdown_rules.push(Value::Object(rule));
            json!({ "id": id, "err_code": 0 })
        }
        ("count_down", "delete_rule") => {
            delete_rule(&mut outlet.countdown_rules, arguments);
            json!({ "err_code": 0 })
        }
        ("count_down", "delete_all_rules") => {
            outlet.countdown_rules.clear();
            json!({ "err_code": 0 })
        }
        ("schedule", "get_rules") => rules_response(&outlet.schedule_rules),
        ("schedule", "delete_rule") => {
            delete_rule(&mut outlet.schedule_rules, arguments);
            json!({ "err_code": 0 })
        }
        _ => json!({ "err_code": -1, "err_msg": "command not implemented by mock outlet" }),
    };
    command_response(module, command, result)
}

fn request_command(request: &Value) -> Option<(&str, &str, &Value)> {
    let (module, commands) = request.as_object()?.iter().next()?;
    let (command, arguments) = commands.as_object()?.iter().next()?;
    Some((module, command, arguments))
}

fn rules_response(rules: &[Value]) -> Value {
    json!({ "enable": 1, "version": 2, "rule_list": rules, "err_code": 0 })
}

fn command_response(module: &str, command: &str, result: Value) -> Value {
    let mut commands = Map::new();
    commands.insert(command.to_owned(), result);
    let mut response = Map::new();
    response.insert(module.to_owned(), Value::Object(commands));
    Value::Object(response)
}

fn delete_rule(rules: &mut Vec<Value>, arguments: &Value) {
    let id = arguments.get("id").and_then(Value::as_str);
    rules.retain(|rule| rule.get("id").and_then(Value::as_str) != id);
}

fn error_response(request: &Value, message: &str) -> Value {
    let Some((module, command, _)) = request_command(request) else {
        return json!({ "system": { "err_code": -1, "err_msg": message } });
    };
    command_response(
        module,
        command,
        json!({ "err_code": -1, "err_msg": message }),
    )
}

fn read_frame(reader: &mut impl Read) -> io::Result<Value> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_LENGTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mock request is too large",
        ));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&decrypt(&payload))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_frame(writer: &mut impl Write, response: &Value) -> io::Result<()> {
    let payload = serde_json::to_vec(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "mock response is too large"))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&encrypt(&payload))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_and_countdown_commands_update_mock_state() {
        let mut outlet = mock_outlet(2, "Test", "HS105(US)", false, None);
        let relay = respond(
            &mut outlet,
            &json!({ "system": { "set_relay_state": { "state": 1 } } }),
        );
        assert_eq!(relay["system"]["set_relay_state"]["err_code"], 0);
        assert!(outlet.plug.relay_on);

        let added = respond(
            &mut outlet,
            &json!({ "count_down": { "add_rule": {
                "name": "Timer", "enable": 1, "delay": 300, "act": 0
            } } }),
        );
        assert_eq!(added["count_down"]["add_rule"]["id"], "mock-countdown-1");
        let rules = respond(&mut outlet, &json!({ "count_down": { "get_rules": {} } }));
        assert_eq!(
            rules["count_down"]["get_rules"]["rule_list"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn brightness_command_updates_mock_dimmer() {
        let mut outlet = mock_outlet(2, "Test dimmer", "HS220(US)", true, Some(40));
        let response = respond(
            &mut outlet,
            &json!({ "smartlife.iot.dimmer": { "set_brightness": { "brightness": 75 } } }),
        );

        assert_eq!(
            response["smartlife.iot.dimmer"]["set_brightness"]["err_code"],
            0
        );
        assert_eq!(outlet.plug.brightness, Some(75));
    }

    #[test]
    fn mock_inventory_includes_a_group() {
        assert_eq!(
            groups(),
            vec![(
                "Living room",
                vec!["mock-outlet-2".to_owned(), "mock-outlet-4".to_owned()]
            )]
        );
    }
}
