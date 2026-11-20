//! Saved server profiles.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, Generate, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

/// Marks an encrypted value in the config file. What follows is base64 of a
/// 12 byte nonce and the AES-256-GCM ciphertext.
const ENCRYPTED: &str = "enc:v1:";

/// The key that encrypts the secrets in the config file: 32 random bytes in
/// `servers.key` next to it. This keeps passwords out of sight and out of a
/// config file that gets shared or backed up on its own; whoever can read
/// both files can still decrypt them.
fn key_path(config: &Path) -> PathBuf {
    config.with_extension("key")
}

fn read_key(config: &Path) -> Option<Aes256Gcm> {
    Aes256Gcm::new_from_slice(&fs::read(key_path(config)).ok()?).ok()
}

/// The key, made now if there is none yet. Never replaces an existing file.
fn read_or_create_key(config: &Path) -> Result<Aes256Gcm> {
    if let Some(key) = read_key(config) {
        return Ok(key);
    }
    let path = key_path(config);
    let key = aes_gcm::Key::<Aes256Gcm>::generate();
    owner_only()
        .open(&path)
        .and_then(|mut file| file.write_all(&key).and_then(|_| file.sync_all()))
        .with_context(|| format!("cannot write the key file {}", path.display()))?;
    Ok(Aes256Gcm::new(&key))
}

/// Options for a new file that only its owner may read.
fn owner_only() -> fs::OpenOptions {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts
}

fn encrypt(key: &Aes256Gcm, value: &str) -> String {
    // A value that could not be decrypted on load is written back as it is.
    if value.is_empty() || value.starts_with(ENCRYPTED) {
        return value.to_string();
    }
    let nonce = Nonce::generate();
    match key.encrypt(&nonce, value.as_bytes()) {
        Ok(sealed) => {
            let bytes = [nonce.as_slice(), &sealed].concat();
            format!("{ENCRYPTED}{}", BASE64.encode(bytes))
        }
        Err(_) => value.to_string(),
    }
}

/// `None` when the value is encrypted with another key or damaged.
fn decrypt(key: &Aes256Gcm, value: &str) -> Option<String> {
    let bytes = BASE64.decode(value.strip_prefix(ENCRYPTED)?).ok()?;
    let (nonce, sealed) = bytes.split_at_checked(12)?;
    let nonce = Nonce::try_from(nonce).ok()?;
    String::from_utf8(key.decrypt(&nonce, sealed).ok()?).ok()
}

/// How a server is reached. The numbers are the protocol list's order in the
/// server dialog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    S3 = 0,
    Sftp = 1,
    Ftp = 2,
}

impl Protocol {
    pub fn from_index(index: i32) -> Protocol {
        match index {
            1 => Protocol::Sftp,
            2 => Protocol::Ftp,
            _ => Protocol::S3,
        }
    }
}

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
    /// Optional bucket to open directly on connect. For SFTP and FTP: the
    /// start folder, blank for the home folder.
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub protocol: Protocol,
    /// SFTP and FTP: `[user@]host[:port]`.
    #[serde(default)]
    pub host: String,
    /// SFTP and FTP: login password. For SFTP also the passphrase of the key
    /// file.
    #[serde(default)]
    pub password: String,
    /// SFTP: private key; blank tries the default keys in `~/.ssh`.
    #[serde(default)]
    pub key_file: String,
    /// SFTP: jump hosts to tunnel through, comma separated, each
    /// `[user@]host[:port]`. Blank connects directly.
    #[serde(default)]
    pub jump: String,
    /// Blank uses `password` on the jump hosts too.
    #[serde(default)]
    pub jump_password: String,
}

impl ServerProfile {
    /// The fields that are encrypted in the config file.
    fn secrets(&mut self) -> [&mut String; 3] {
        [
            &mut self.secret_key,
            &mut self.password,
            &mut self.jump_password,
        ]
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub servers: Vec<ServerProfile>,
}

pub fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("cannot find the user config directory")?;
    let new = dir.join("filestasher");
    // The app used to be called s3-fstasher: take its settings along.
    let old = dir.join("s3-fstasher");
    if !new.exists() && old.exists() {
        let _ = std::fs::rename(&old, &new);
    }
    Ok(new.join("servers.toml"))
}

impl Config {
    /// A missing file is an empty config.
    pub fn load_from(path: &Path) -> Result<Config> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))?;
        let key = read_key(path);
        let mut plain = false;
        for secret in cfg.servers.iter_mut().flat_map(ServerProfile::secrets) {
            if !secret.starts_with(ENCRYPTED) {
                plain |= !secret.is_empty();
            } else if let Some(key) = &key
                && let Some(value) = decrypt(key, secret)
            {
                *secret = value;
            }
        }
        // A file from an older version, or edited by hand: encrypt it now.
        if plain {
            let _ = cfg.save_to(path);
        }
        Ok(cfg)
    }

    /// Whether some secret could not be decrypted: the key file is missing or
    /// not the one the config was written with. Such values are kept as they
    /// are, so saving does not lose them.
    pub fn has_locked_secrets(&self) -> bool {
        let secrets = |p: &ServerProfile| p.clone().secrets().map(|s| s.starts_with(ENCRYPTED));
        self.servers.iter().any(|p| secrets(p).contains(&true))
    }

    /// Writes through a temp file so a failed write keeps the old config.
    /// Secrets are written encrypted, and the file is created owner-only.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let key = read_or_create_key(path)?;
        let mut out = self.clone();
        for secret in out.servers.iter_mut().flat_map(ServerProfile::secrets) {
            *secret = encrypt(&key, secret);
        }
        let text = toml::to_string_pretty(&out).context("cannot serialize config")?;
        let tmp = path.with_extension("toml.tmp");
        let _ = fs::remove_file(&tmp);
        let mut file = owner_only()
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
                    ..Default::default()
                },
                ServerProfile {
                    name: "box".into(),
                    protocol: Protocol::Sftp,
                    host: "me@10.0.0.5:2222".into(),
                    jump: "ops@bastion".into(),
                    ..Default::default()
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
    fn secrets_are_encrypted_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("servers.toml");
        let mut cfg = sample();
        cfg.servers[1].password = "hunter2".into();
        cfg.save_to(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("hunter2") && !text.contains("s3cr3t"),
            "{text}"
        );
        assert_eq!(text.matches(ENCRYPTED).count(), 2);
        assert_eq!(Config::load_from(&path).unwrap(), cfg);

        // A plaintext file is read and encrypted on the spot.
        fs::write(&path, "[[servers]]\nname = \"x\"\npassword = \"hunter2\"\n").unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.servers[0].password, "hunter2");
        assert!(!fs::read_to_string(&path).unwrap().contains("hunter2"));

        // Without its key the value stays sealed, through a load and a save.
        assert!(!cfg.has_locked_secrets());
        let sealed = fs::read_to_string(&path).unwrap();
        fs::remove_file(key_path(&path)).unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.has_locked_secrets());
        cfg.save_to(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), sealed);
    }

    #[test]
    fn missing_fields_default() {
        let cfg: Config = toml::from_str("[[servers]]\nname = \"x\"\n").unwrap();
        assert_eq!(cfg.servers[0].name, "x");
        assert!(!cfg.servers[0].path_style);
        assert_eq!(cfg.servers[0].protocol, Protocol::S3);
        let cfg: Config =
            toml::from_str("[[servers]]\nname = \"x\"\nprotocol = \"ftp\"\n").unwrap();
        assert_eq!(cfg.servers[0].protocol, Protocol::Ftp);
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
