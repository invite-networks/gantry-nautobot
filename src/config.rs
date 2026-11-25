//! `/etc/gantry/plugins/nautobot.toml`.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub url: String,
    pub token: String,
    pub ca_file: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default = "default_ip_status")]
    pub ip_status: String,
    pub node: Option<String>,
}

fn default_timeout() -> u64 {
    10
}

fn default_ip_status() -> String {
    "Active".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let c: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if !(c.url.starts_with("https://") || c.url.starts_with("http://")) {
            return Err(format!("{}: url must start with https://", path.display()));
        }
        if c.token.trim().is_empty() {
            return Err(format!("{}: token is empty", path.display()));
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_errors() {
        let dir = std::env::temp_dir().join(format!("gnb-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.toml");
        std::fs::write(&p, "url = \"https://nb\"\ntoken = \"t\"\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!((c.timeout, c.ip_status.as_str()), (10, "Active"));
        std::fs::write(&p, "url = \"nb\"\ntoken = \"t\"\n").unwrap();
        assert!(Config::load(&p).is_err());
        std::fs::write(&p, "url = \"https://nb\"\ntoken = \"t\"\nbogus = 1\n").unwrap();
        assert!(Config::load(&p).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
