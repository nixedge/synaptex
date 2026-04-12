use anyhow::Result;
use clap::Subcommand;

// ─── Subcommands ─────────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub enum RouterCmd {
    /// Manage devices known to the router.
    #[command(subcommand)]
    Device(RouterDeviceCmd),
}

#[derive(Debug, Subcommand)]
pub enum RouterDeviceCmd {
    /// Register a device with the router and allocate a managed IP.
    Register {
        /// MAC address of the device (AA:BB:CC:DD:EE:FF).
        #[arg(long, value_name = "MAC")]
        mac: String,

        /// Currently observed IP address (optional; leave blank if unknown).
        #[arg(long, value_name = "IP", default_value = "")]
        ip: String,

        /// Device kind: "bond", "matter", or "other".
        #[arg(long, value_name = "KIND")]
        kind: String,

        /// Bond hub serial number (bondid from GET /v2/sys/version).
        #[arg(long, value_name = "BOND_ID", default_value = "")]
        bond_id: String,

        /// Bond local API token (BOND-Token header value).
        #[arg(long, value_name = "TOKEN", default_value = "")]
        bond_token: String,
    },
}

// ─── Dispatch ─────────────────────────────────────────────────────────────────

pub async fn run(cmd: RouterCmd, url: &str, key: Option<&str>) -> Result<()> {
    match cmd {
        RouterCmd::Device(device_cmd) => run_device(device_cmd, url, key).await,
    }
}

async fn run_device(cmd: RouterDeviceCmd, url: &str, key: Option<&str>) -> Result<()> {
    match cmd {
        RouterDeviceCmd::Register { mac, ip, kind, bond_id, bond_token } => {
            let body = serde_json::json!({
                "mac":        mac,
                "ip":         ip,
                "kind":       kind,
                "bond_id":    bond_id,
                "bond_token": bond_token,
            });

            let client = reqwest::Client::new();
            let mut req = client
                .post(format!("{url}/api/v1/router/devices"))
                .json(&body);

            if let Some(k) = key {
                req = req.bearer_auth(k);
            }

            let resp = req.send().await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("register failed ({status}): {text}");
            }

            let r: serde_json::Value = resp.json().await?;
            println!("device_id:  {}", r["device_id"].as_str().unwrap_or(""));
            println!("managed_ip: {}", r["managed_ip"].as_str().unwrap_or(""));
            Ok(())
        }
    }
}
