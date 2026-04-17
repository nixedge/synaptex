use anyhow::Result;
use clap::Subcommand;

// ─── Subcommands ─────────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub enum HubCmd {
    /// List registered hubs and their sub-device counts.
    List,

    /// Register a hub or cloud account.
    Register {
        #[command(subcommand)]
        kind: RegisterKind,
    },
}

#[derive(Debug, Subcommand)]
pub enum RegisterKind {
    /// Register a Bond bridge hub.
    Bond {
        /// MAC address of the hub (AA:BB:CC:DD:EE:FF).
        #[arg(long, value_name = "MAC")]
        mac: String,

        /// Currently observed IP address (optional; leave blank if unknown).
        #[arg(long, value_name = "IP", default_value = "")]
        ip: String,

        /// Bond hub serial number (bondid from GET /v2/sys/version).
        #[arg(long, value_name = "BOND_ID", default_value = "")]
        bond_id: String,

        /// Bond local API token (BOND-Token header value).
        #[arg(long, value_name = "TOKEN", default_value = "")]
        bond_token: String,
    },

    /// Register a Mysa cloud account.
    Mysa {
        /// Mysa cloud account e-mail address.
        #[arg(long, value_name = "EMAIL")]
        username: String,

        /// Mysa cloud account password.
        #[arg(long, value_name = "PASS")]
        password: String,
    },
}

// ─── Dispatch ─────────────────────────────────────────────────────────────────

pub async fn run(cmd: HubCmd, url: &str, key: Option<&str>) -> Result<()> {
    match cmd {
        HubCmd::List => {
            let client = reqwest::Client::new();
            let mut req = client.get(format!("{url}/api/v1/hubs"));
            if let Some(k) = key {
                req = req.bearer_auth(k);
            }
            let resp = req.send().await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("list failed ({status}): {text}");
            }
            let hubs: serde_json::Value = resp.json().await?;
            let hubs = hubs.as_array().map(Vec::as_slice).unwrap_or_default();
            if hubs.is_empty() {
                println!("no hubs registered");
            } else {
                println!("{:<19} {:<8} {:<16} devices", "MAC", "KIND", "HUB IP");
                for h in hubs {
                    println!("{:<19} {:<8} {:<16} {}",
                        h["mac"].as_str().unwrap_or(""),
                        h["kind"].as_str().unwrap_or(""),
                        h["hub_ip"].as_str().unwrap_or(""),
                        h["device_count"].as_u64().unwrap_or(0),
                    );
                }
            }
            Ok(())
        }

        HubCmd::Register { kind } => {
            let body = match kind {
                RegisterKind::Bond { mac, ip, bond_id, bond_token } => serde_json::json!({
                    "kind":       "bond",
                    "mac":        mac,
                    "ip":         ip,
                    "bond_id":    bond_id,
                    "bond_token": bond_token,
                }),
                RegisterKind::Mysa { username, password } => serde_json::json!({
                    "kind":     "mysa",
                    "username": username,
                    "password": password,
                }),
            };

            let client = reqwest::Client::new();
            let mut req = client
                .post(format!("{url}/api/v1/hubs"))
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
