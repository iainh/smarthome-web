use std::time::Duration;
use tddp_client::SmartHomeClient;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Scanning for Kasa smart plugs...");
    let client = SmartHomeClient::new();
    let inventory = client.get_inventory(Duration::from_secs(5))?;

    if inventory.is_empty() {
        println!("No smart plugs responded.");
    } else {
        println!("ADDRESS\tMODEL\tSTATE\tALIAS\tSOFTWARE");
        for device in inventory {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                device.address,
                device.model,
                if device.relay_on { "on" } else { "off" },
                device.alias,
                device.software_version,
            );
        }
    }

    Ok(())
}
