use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Client,
    Host,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub nick: String,
    pub machine_id: String,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default)]
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_device: Option<String>,
}

fn default_theme() -> String {
    "cyberpunk".into()
}

impl Config {
    pub fn path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".config").join("dusk").join("config.toml")
    }

    pub fn load() -> Result<Option<Self>> {
        let path = Self::path();
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        Ok(Some(toml::from_str(&content)?))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn machine_id() -> String {
        // /etc/machine-id is stable and unique per Linux installation
        if let Ok(id) = std::fs::read_to_string("/etc/machine-id") {
            let id = id.trim().to_string();
            if !id.is_empty() {
                return id[..8.min(id.len())].to_string();
            }
        }
        primary_mac().unwrap_or_else(|| "unknown".to_string())
    }

    pub fn update_theme(theme: &str) -> Result<()> {
        if let Some(mut config) = Self::load()? {
            config.theme = theme.to_string();
            config.save()?;
        }
        Ok(())
    }

    pub fn update_devices(
        audio_input: Option<&str>,
        audio_output: Option<&str>,
        video_device: Option<&str>,
        theme: &str,
    ) -> Result<()> {
        if let Some(mut config) = Self::load()? {
            config.audio_input = audio_input.map(str::to_string);
            config.audio_output = audio_output.map(str::to_string);
            config.video_device = video_device.map(str::to_string);
            config.theme = theme.to_string();
            config.save()?;
        }
        Ok(())
    }
}

pub fn prompt_nick() -> Result<String> {
    use std::io::Write;
    print!("Welcome to Dusk! Enter your nickname: ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let nick: String = input
        .trim()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .take(20)
        .collect();
    if nick.is_empty() {
        return Err(anyhow::anyhow!("nickname cannot be empty"));
    }
    Ok(nick)
}

fn primary_mac() -> Option<String> {
    let preferred = [
        "eth0", "enp0s3", "enp1s0", "enp2s0", "wlan0", "wlp2s0", "wlp3s0", "wlp0s20f3",
    ];
    for iface in preferred {
        if let Some(mac) = read_mac(iface) {
            return Some(mac);
        }
    }
    // fallback: any non-loopback, non-virtual interface
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return None;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_str().unwrap_or("");
        if matches!(name, "lo")
            || name.starts_with("tailscale")
            || name.starts_with("docker")
            || name.starts_with("veth")
            || name.starts_with("br-")
        {
            continue;
        }
        if let Some(mac) = read_mac(name) {
            return Some(mac);
        }
    }
    None
}

fn read_mac(iface: &str) -> Option<String> {
    let path = format!("/sys/class/net/{iface}/address");
    let mac = std::fs::read_to_string(path).ok()?;
    let mac = mac.trim().to_string();
    if mac.is_empty() || mac == "00:00:00:00:00:00" {
        None
    } else {
        Some(mac)
    }
}
