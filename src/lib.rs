use serde::Deserialize;
use serde::Serialize;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

#[derive(Serialize, Deserialize, Debug)]
pub struct Device {
    pub system: System,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct System {
    #[serde(rename = "get_sysinfo")]
    pub sys_info: SysInfo,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SysInfo {
    pub sw_ver: String,
    pub hw_ver: String,
    #[serde(rename = "type")]
    pub dev_type: String,
    pub model: String,
    pub mac: String,
    pub dev_name: String,
    pub alias: String,
    pub relay_state: u8,
    pub on_time: u16,
    pub active_mode: String,
    pub feature: String,
    pub updating: u8,
    pub icon_hash: String,
    pub rssi: i8,
    pub led_off: u8,
    pub longitude_i: i32,
    pub latitude_i: i32,
    #[serde(rename = "hwId")]
    pub hw_id: String,
    #[serde(rename = "fwId")]
    pub fw_id: String,
    #[serde(rename = "deviceId")]
    pub device_id: String,
    #[serde(rename = "oemId")]
    pub oem_id: String,
    pub next_action: Action,
    pub err_code: u8,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Action {
    #[serde(rename = "type")]
    pub action_type: u8,
    pub id: String,
    pub schd_sec: u32,
    pub action: u8,
}

// Encryption and Decryption of TP-Link Smart Home Protocol
//  XOR Autokey Cipher with starting key = 171
fn encrypt(input: &str) -> Vec<u8> {
    let mut key = 171;
    input.chars().map(|i| {
        let a = key ^ (i as u8);
        key = a;
        a
    }).collect()
}

fn decrypt(input: &[u8]) -> Vec<u8> {
    let mut key = 171 as u8;
    input.iter().map(|i| {
        let a = key ^ i;
        key = *i;
        a
    }).collect()
}

pub fn get_inventory(timeout: Duration) -> Vec<Device> {
    const MAX_DATAGRAM_SIZE: usize = 2048;

    let info_message = "{\"system\":{\"get_sysinfo\":{}}}";
    let info_message = encrypt(info_message);

    let remote_addr: SocketAddr = "255.255.255.255:9999".parse().unwrap();
    // We use port 0 to let the operating system allocate an available port for us.
    let local_addr: SocketAddr = if remote_addr.is_ipv4() {
        "0.0.0.0:9999"
    } else {
        "[::]:9999"
    }
        .parse()
        .unwrap();

    let local_socket = UdpSocket::bind(&local_addr).unwrap();
    local_socket.set_broadcast(true).expect("set_broadcast call failed");
    local_socket.set_read_timeout(Some(timeout)).expect("set_read_timeout call failed");

    let mut devices: Vec<Device> = Vec::new();

    match local_socket.send_to(&info_message, &remote_addr) {
        Err(e) => eprintln!("{}", e),
        Ok(_) => loop {
            let mut buf = [0; MAX_DATAGRAM_SIZE];
            match local_socket.recv_from(&mut buf) {
                Err(e) => {
                    eprintln!("{}", e);
                    break;
                }
                Ok((bytes_received, _peer)) => {
                    let filled_buf = &buf[..bytes_received];
                    let decoded = decrypt(filled_buf);
                    if decoded != info_message {
                        let device: Result<Device, serde_json::Error> =
                            serde_json::from_slice(&decoded);
                        if let Ok(device) = device {
                            devices.push(device);
                        }
                    }
                }
            }
        },
    }

    devices
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
