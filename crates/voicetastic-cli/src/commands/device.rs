//! `voicetastic device {info,reboot,factory-reset}` — device admin commands.

use anyhow::{Result, bail};

use voicetastic_core::MeshtasticService;
use voicetastic_core::ids::node_num_to_id;
use voicetastic_core::meshtastic::service::node_long_name;

use crate::connect::connect;
use crate::util::disconnect_with_timeout;

pub async fn info(device: &str) -> Result<()> {
    let svc = MeshtasticService::new().await?;
    connect(&svc, device).await?;

    if let Some(info) = svc.watch_my_info().borrow().as_ref() {
        let id = node_num_to_id(info.my_node_num);
        println!("Local node:   {id} ({})", info.my_node_num);
        println!("Reboot count: {}", info.reboot_count);
        println!("Min app vers: {}", info.min_app_version);
    } else {
        println!("Local node:   <unknown>");
    }
    if let Some(meta) = svc.watch_metadata().borrow().as_ref() {
        println!("Firmware:     {}", meta.firmware_version);
        println!("Hw model:     {}", meta.hw_model);
        println!("Role:         {}", meta.role);
        println!("Has Wifi:     {}", meta.has_wifi);
        println!("Has Bluetooth:{}", meta.has_bluetooth);
    }
    if let Some(owner) = svc.watch_owner().borrow().as_ref() {
        println!("Owner:        {} ({})", owner.long_name, owner.short_name);
    }

    let nodes = svc.watch_nodes().borrow().clone();
    if !nodes.is_empty() {
        println!("\nKnown nodes ({}):", nodes.len());
        let mut sorted: Vec<_> = nodes.values().collect();
        sorted.sort_by_key(|n| n.num);
        for node in sorted {
            let id = node_num_to_id(node.num);
            let name = node_long_name(node).unwrap_or("?");
            println!("  {id}  {name}");
        }
    }

    disconnect_with_timeout(&svc).await;
    Ok(())
}

pub async fn reboot(device: &str, secs: u32) -> Result<()> {
    let svc = MeshtasticService::new().await?;
    connect(&svc, device).await?;
    let id = svc.reboot(secs as i32).await?;
    println!("reboot scheduled in {secs}s (admin id={id})");
    disconnect_with_timeout(&svc).await;
    Ok(())
}

pub async fn factory_reset(device: &str, confirmed: bool) -> Result<()> {
    if !confirmed {
        bail!("refusing to factory-reset without --yes");
    }
    let svc = MeshtasticService::new().await?;
    connect(&svc, device).await?;
    let id = svc.factory_reset().await?;
    println!("factory reset sent (admin id={id})");
    disconnect_with_timeout(&svc).await;
    Ok(())
}
