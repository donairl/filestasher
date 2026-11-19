//! Saved server profiles.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerProfile {
    pub name: String,
    /// Blank means the default AWS endpoint for the region.
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
    #[serde(default)]
    pub path_style: bool,
    /// Optional bucket to open directly on connect.
    #[serde(default)]
    pub bucket: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub servers: Vec<ServerProfile>,
}

pub fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("cannot find the user config directory")?;
    Ok(dir.join("s3-fstasher").join("servers.toml"))
}

impl Config {
    /// A missing file is an empty config.
    pub fn load_from(path: &Path) -> Result<Config> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
    }

    /// Writes through a temp file so a failed write keeps the old config.
    /// The file holds secret keys, so it is created owner-only.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("cannot serialize config")?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let tmp = path.with_extension("toml.tmp");
        let _ = fs::remove_file(&tmp);
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        file.write_all(text.as_bytes())
            .and_then(|_| file.sync_all())
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        drop(file);
        fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            servers: vec![
                ServerProfile {
                    name: "minio".into(),
                    endpoint: "http://localhost:9000".into(),
                    region: "us-east-1".into(),
                    access_key: "ak".into(),
                    secret_key: "s3cr3t/+=".into(),
                    path_style: true,
                    bucket: "data".into(),
                },
                ServerProfile {
                    name: "aws".into(),
                    region: "ap-southeast-1".into(),
                    ..Default::default()
                },
            ],
        }
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("servers.toml");
        let cfg = sample();
        cfg.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), cfg);
        // Saving again replaces the file.
        let mut cfg2 = cfg.clone();
        cfg2.servers.pop();
        cfg2.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), cfg2);
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&dir.path().join("none.toml")).unwrap();
        assert!(cfg.servers.is_empty());
    }

    #[test]
    fn missing_fields_default() {
        let cfg: Config = toml::from_str("[[servers]]\nname = \"x\"\n").unwrap();
        assert_eq!(cfg.servers[0].name, "x");
        assert!(!cfg.servers[0].path_style);
    }

    #[cfg(unix)]
    #[test]
    fn file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("servers.toml");
        sample().save_to(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
