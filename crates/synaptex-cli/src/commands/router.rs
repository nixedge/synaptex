use anyhow::{bail, Context, Result};
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum RouterCmd {
    /// List all Tuya devices currently in the router's discovery cache.
    Devices,
}

pub async fn run(cmd: RouterCmd, url: &str, key: Option<&str>) -> Result<()> {
    match cmd {
        RouterCmd::Devices => devices(url, key).await,
    }
}

async fn devices(url: &str, key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.get(format!("{url}/api/v1/router/devices"));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await.context("GET /api/v1/router/devices")?;
    if !resp.status().is_success() {
        bail!("server error: {}", resp.text().await?);
    }

    let devices: Vec<serde_json::Value> = resp.json().await?;
    if devices.is_empty() {
        println!("no devices in router cache (router not connected or no devices seen yet)");
        return Ok(());
    }

    println!("{:<19} {:<16} {:<16} {:<32} VERSION",
        "MAC", "CURRENT IP", "MANAGED IP", "TUYA_ID");
    println!("{}", "-".repeat(90));
    for d in &devices {
        println!("{:<19} {:<16} {:<16} {:<32} {}",
            d["mac"].as_str().unwrap_or("-"),
            d["ip"].as_str().unwrap_or("-"),
            d["managed_ip"].as_str().unwrap_or("-"),
            d["tuya_id"].as_str().unwrap_or("-"),
            d["version"].as_str().unwrap_or("-"),
        );
    }
    println!("\n{} device(s)", devices.len());
    Ok(())
}
