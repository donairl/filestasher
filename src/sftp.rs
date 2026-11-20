//! SFTP over SSH, optionally tunnelled through jump hosts.
//!
//! Keys are absolute remote paths without the leading slash; folders end in
//! `/`. That is the shape of S3 keys, so the rest of the app treats both alike.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use russh::client::{self, Handle};
use russh::keys::{
    HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, known_hosts, load_secret_key,
};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::StatusCode;
use tokio::io::AsyncWriteExt;

use crate::config::ServerProfile;
use crate::remote::{abs, ancestors, pump, save_to};
use crate::s3::{Entry, Page, Progress};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Splits `[user@]host[:port]`. The user defaults to the local login name.
// ponytail: a bare IPv6 address is taken as a host on the default port; `[v6]:port`
// is not parsed. Add when someone needs it.
pub fn parse_host(spec: &str, default_port: u16) -> Result<(String, String, u16)> {
    let spec = spec.trim();
    let (user, rest) = match spec.rsplit_once('@') {
        Some((u, r)) => (u.to_string(), r),
        None => (
            std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_default(),
            spec,
        ),
    };
    let (host, port) = match rest.split_once(':') {
        Some((h, p)) if !p.contains(':') => (
            h,
            p.parse()
                .with_context(|| format!("bad port in \"{spec}\""))?,
        ),
        _ => (rest, default_port),
    };
    if user.is_empty() || host.is_empty() {
        bail!("\"{spec}\" is not user@host[:port]");
    }
    Ok((user, host.to_string(), port))
}

/// Checks the server key against `~/.ssh/known_hosts`. An unknown host is
/// recorded once the user trusts its fingerprint, a changed key is refused:
/// the same as OpenSSH.
struct HostCheck {
    host: String,
    port: u16,
    /// True while the user is being asked about this host.
    asking: Arc<AtomicBool>,
}

/// Asks the user whether to trust a host seen for the first time, given the
/// question to show. The app installs it at startup; while it is unset, as
/// in the tests, such hosts are trusted without asking.
pub static CONFIRM_HOST: OnceLock<fn(String) -> tokio::sync::oneshot::Receiver<bool>> =
    OnceLock::new();

impl client::Handler for HostCheck {
    type Error = anyhow::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            bail!(
                "{} presented a host certificate, which is not supported",
                self.host
            );
        };
        match known_hosts::check_known_hosts(&self.host, self.port, key) {
            Ok(true) => Ok(true),
            Ok(false) => {
                if let Some(ask) = CONFIRM_HOST.get() {
                    let question = format!(
                        "First connection to {}:{}.\n\nIts {} key fingerprint is\n{}\n\n\
                         Trust this host and remember it?",
                        self.host,
                        self.port,
                        key.algorithm(),
                        key.fingerprint(HashAlg::Sha256)
                    );
                    self.asking.store(true, Ordering::Relaxed);
                    // A closed dialog answers no.
                    let trusted = ask(question).await.unwrap_or(false);
                    self.asking.store(false, Ordering::Relaxed);
                    if !trusted {
                        bail!("the host key of {} was not trusted", self.host);
                    }
                }
                known_hosts::learn_known_hosts(&self.host, self.port, key)
                    .context("cannot record the host key in ~/.ssh/known_hosts")?;
                Ok(true)
            }
            Err(russh::keys::Error::KeyChanged { line }) => bail!(
                "the host key of {} CHANGED (~/.ssh/known_hosts line {line}). \
                 Refusing to connect: this may be an attack. If the server was \
                 reinstalled, remove that line.",
                self.host
            ),
            Err(e) => Err(e).context("cannot read ~/.ssh/known_hosts"),
        }
    }
}

/// Key file first (the given one, else the usual `~/.ssh` defaults), then the
/// password. `password` also unlocks the key file.
// ponytail: no ssh-agent and no keyboard-interactive (OTP) login. Add when a
// server needs them.
async fn authenticate(
    handle: &mut Handle<HostCheck>,
    user: &str,
    password: &str,
    key_file: &str,
) -> Result<()> {
    let home = dirs::home_dir().unwrap_or_default();
    let explicit = !key_file.is_empty();
    let keys: Vec<PathBuf> = match key_file.strip_prefix("~/") {
        Some(rest) => vec![home.join(rest)],
        None if explicit => vec![PathBuf::from(key_file)],
        None => ["id_ed25519", "id_ecdsa", "id_rsa"]
            .iter()
            .map(|n| home.join(".ssh").join(n))
            .filter(|p| p.exists())
            .collect(),
    };
    for path in keys {
        let key = match load_secret_key(&path, (!password.is_empty()).then_some(password)) {
            Ok(k) => k,
            Err(e) if explicit => {
                return Err(e).with_context(|| format!("cannot load key {}", path.display()));
            }
            // A default key that is locked or unreadable is just skipped.
            Err(_) => continue,
        };
        let hash = handle.best_supported_rsa_hash().await?.flatten();
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        if handle.authenticate_publickey(user, key).await?.success() {
            return Ok(());
        }
    }
    if !password.is_empty()
        && handle
            .authenticate_password(user, password)
            .await?
            .success()
    {
        return Ok(());
    }
    bail!("authentication failed (key file and password both rejected)")
}

/// An open SFTP session. The handles keep every hop of the tunnel alive.
struct Live {
    sftp: Arc<SftpSession>,
    hops: Vec<Handle<HostCheck>>,
}

async fn connect(p: &ServerProfile) -> Result<Live> {
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });
    let jump_password = match p.jump_password.as_str() {
        "" => p.password.as_str(),
        pw => pw,
    };
    let mut targets: Vec<(&str, &str)> = p
        .jump
        .split(',')
        .map(str::trim)
        .filter(|j| !j.is_empty())
        .map(|j| (j, jump_password))
        .collect();
    targets.push((&p.host, &p.password));

    let mut hops: Vec<Handle<HostCheck>> = Vec::new();
    for (spec, password) in targets {
        let (user, host, port) = parse_host(spec, 22)?;
        let asking = Arc::new(AtomicBool::new(false));
        let check = HostCheck {
            host: host.clone(),
            port,
            asking: asking.clone(),
        };
        let opening = async {
            match hops.last() {
                None => client::connect(config.clone(), (host.as_str(), port), check).await,
                // Through the previous hop, like `ssh -J`.
                Some(prev) => {
                    let tunnel = prev
                        .channel_open_direct_tcpip(host.clone(), port.into(), "127.0.0.1", 0)
                        .await?;
                    client::connect_stream(config.clone(), tunnel.into_stream(), check).await
                }
            }
        };
        let opened = {
            tokio::pin!(opening);
            loop {
                tokio::select! {
                    r = &mut opening => break r,
                    // The clock does not run while the user reads the host
                    // key question.
                    _ = tokio::time::sleep(CONNECT_TIMEOUT) => {
                        if !asking.load(Ordering::Relaxed) {
                            break Err(anyhow::anyhow!("timed out"));
                        }
                    }
                }
            }
        };
        let mut handle = opened.with_context(|| format!("cannot connect to {host}:{port}"))?;
        authenticate(&mut handle, &user, password, &p.key_file)
            .await
            .with_context(|| format!("{user}@{host}"))?;
        hops.push(handle);
    }
    let channel = hops[hops.len() - 1].channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .context("cannot start SFTP")?;
    Ok(Live {
        sftp: Arc::new(sftp),
        hops,
    })
}

/// A lazily opened SFTP connection: connects on first use, and again after
/// the connection dropped.
pub struct Conn {
    profile: ServerProfile,
    live: tokio::sync::Mutex<Option<Live>>,
}

impl Conn {
    pub fn new(profile: &ServerProfile) -> Conn {
        Conn {
            profile: profile.clone(),
            live: tokio::sync::Mutex::new(None),
        }
    }

    /// `sftp://user@host/key`.
    pub fn uri(&self, key: &str) -> String {
        format!("sftp://{}/{key}", self.profile.host.trim())
    }

    async fn sftp(&self) -> Result<Arc<SftpSession>> {
        // Held while connecting, so concurrent callers share one connection.
        let mut live = self.live.lock().await;
        if let Some(l) = live.as_ref()
            && !l.hops.iter().any(Handle::is_closed)
        {
            return Ok(l.sftp.clone());
        }
        let l = connect(&self.profile).await?;
        let sftp = l.sftp.clone();
        *live = Some(l);
        Ok(sftp)
    }

    /// Folder prefix a new tab starts in: the profile's start folder, or the
    /// home folder.
    pub async fn start_prefix(&self) -> Result<String> {
        let start = match self.profile.bucket.trim() {
            "" => ".",
            s => s,
        };
        let path = self
            .sftp()
            .await?
            .canonicalize(start)
            .await
            .with_context(|| format!("cannot open {start}"))?;
        Ok(match path.trim_matches('/') {
            "" => String::new(),
            p => format!("{p}/"),
        })
    }

    pub async fn test_connection(&self) -> Result<String> {
        let prefix = self.start_prefix().await?;
        Ok(format!("Connection OK, folder /{prefix} reachable."))
    }

    /// The whole folder in one page.
    pub async fn list_page(&self, prefix: &str) -> Result<Page> {
        let sftp = self.sftp().await?;
        let dir = abs(prefix);
        let mut entries = Vec::new();
        let listing = sftp
            .read_dir(dir.as_str())
            .await
            .with_context(|| format!("list {dir}"))?;
        for e in listing {
            let name = e.file_name();
            let mut meta = e.metadata();
            // A link to a folder opens like a folder.
            if meta.file_type().is_symlink()
                && let Ok(target) = sftp.metadata(e.path()).await
            {
                meta = target;
            }
            let is_dir = meta.file_type().is_dir();
            entries.push(Entry {
                key: format!("{prefix}{name}{}", if is_dir { "/" } else { "" }),
                name,
                is_dir,
                size: meta.size.unwrap_or(0),
                modified: meta.mtime.map(i64::from),
            });
        }
        Ok(Page {
            entries,
            next: None,
        })
    }

    /// Everything under the folder `prefix`, as (key, size): files first,
    /// then the folders (keys ending in `/`) deepest first and `prefix`
    /// itself last, so deleting in this order empties each folder before
    /// removing it. A missing folder is empty.
    pub async fn list_all(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let sftp = self.sftp().await?;
        match sftp.symlink_metadata(abs(prefix)).await {
            Ok(meta) if meta.file_type().is_dir() => {}
            // A link to a folder is one file here: deleting it must remove
            // the link, never what it points to.
            // ponytail: so downloading a linked folder fails; follow links on
            // download when that matters.
            Ok(_) => return Ok(vec![(prefix.trim_end_matches('/').to_string(), 0)]),
            Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", abs(prefix))),
        }
        let mut files = Vec::new();
        let mut dirs = vec![prefix.to_string()];
        let mut next = 0;
        while next < dirs.len() {
            let dir = dirs[next].clone();
            next += 1;
            let listing = sftp
                .read_dir(abs(&dir))
                .await
                .with_context(|| format!("list {}", abs(&dir)))?;
            for e in listing {
                let meta = e.metadata();
                if meta.file_type().is_dir() {
                    dirs.push(format!("{dir}{}/", e.file_name()));
                } else {
                    files.push((format!("{dir}{}", e.file_name()), meta.size.unwrap_or(0)));
                }
            }
        }
        // Every folder was found after its parent.
        files.extend(dirs.into_iter().rev().map(|d| (d, 0)));
        Ok(files)
    }

    pub async fn object_exists(&self, key: &str) -> Result<bool> {
        Ok(self.sftp().await?.try_exists(abs(key)).await?)
    }

    pub async fn get_bytes(&self, key: &str) -> Result<Vec<u8>> {
        let path = abs(key);
        let sftp = self.sftp().await?;
        sftp.read(path.as_str())
            .await
            .with_context(|| format!("download {path}"))
    }

    pub async fn download_file(
        &self,
        key: &str,
        dest: &Path,
        progress: Progress<'_>,
    ) -> Result<()> {
        let path = abs(key);
        let sftp = self.sftp().await?;
        let mut src = sftp
            .open(path.as_str())
            .await
            .with_context(|| format!("download {path}"))?;
        save_to(&mut src, dest, progress).await
    }

    pub async fn upload_file(&self, key: &str, src: &Path, progress: Progress<'_>) -> Result<()> {
        let path = abs(key);
        let sftp = self.sftp().await?;
        let mut file = tokio::fs::File::open(src)
            .await
            .with_context(|| format!("cannot read {}", src.display()))?;
        let mut dst = match sftp.create(path.as_str()).await {
            Ok(f) => f,
            // Most likely the folders above it are missing.
            Err(_) => {
                for dir in ancestors(&path) {
                    // Fails for the folders that exist; the create below
                    // reports anything that really went wrong.
                    let _ = sftp.create_dir(dir).await;
                }
                sftp.create(path.as_str())
                    .await
                    .with_context(|| format!("upload {path}"))?
            }
        };
        let result = async {
            pump(&mut file, &mut dst, progress).await?;
            dst.shutdown().await
        }
        .await;
        result.with_context(|| format!("upload {path}"))
    }

    /// Keys in the order of [`Conn::list_all`]: a key ending in `/` is a
    /// folder, which must be empty by the time it is reached.
    pub async fn delete_keys(&self, keys: &[String]) -> Result<()> {
        let sftp = self.sftp().await?;
        for key in keys {
            let path = abs(key);
            if key.ends_with('/') {
                sftp.remove_dir(path.as_str()).await
            } else {
                sftp.remove_file(path.as_str()).await
            }
            .with_context(|| format!("delete {path}"))?;
        }
        Ok(())
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (from, to) = (abs(from), abs(to));
        let sftp = self.sftp().await?;
        sftp.rename(from.as_str(), to.as_str())
            .await
            .with_context(|| format!("rename {from}"))
    }

    pub async fn create_folder(&self, key: &str) -> Result<()> {
        let path = abs(key);
        let sftp = self.sftp().await?;
        sftp.create_dir(path.as_str())
            .await
            .with_context(|| format!("create folder {path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_specs() {
        let ok = |s| parse_host(s, 22).unwrap();
        assert_eq!(ok("me@box"), ("me".into(), "box".into(), 22));
        assert_eq!(
            ok(" ops@10.0.0.5:2222 "),
            ("ops".into(), "10.0.0.5".into(), 2222)
        );
        assert_eq!(ok("a@b@box:1"), ("a@b".into(), "box".into(), 1));
        assert_eq!(ok("me@fe80::1"), ("me".into(), "fe80::1".into(), 22));
        assert!(parse_host("me@box:ssh", 22).is_err());
        assert!(parse_host("me@", 22).is_err());
        assert!(parse_host("@box", 22).is_err());
    }
}

/// End-to-end check against a real server:
/// `SFTP_TEST_HOST=user@host:port [SFTP_TEST_JUMP=…] [SFTP_TEST_KEY=…]
/// cargo test sftp_round_trip -- --ignored`
#[cfg(test)]
#[tokio::test]
#[ignore = "needs an SSH server"]
async fn sftp_round_trip() {
    let env = |k| std::env::var(k).unwrap_or_default();
    let conn = Conn::new(&ServerProfile {
        protocol: crate::config::Protocol::Sftp,
        host: env("SFTP_TEST_HOST"),
        jump: env("SFTP_TEST_JUMP"),
        key_file: env("SFTP_TEST_KEY"),
        password: env("SFTP_TEST_PASSWORD"),
        bucket: env("SFTP_TEST_DIR"),
        ..Default::default()
    });
    let base = format!("{}fstasher-test/", conn.start_prefix().await.unwrap());
    let local = tempfile::tempdir().unwrap();
    let src = local.path().join("src.bin");
    let data: Vec<u8> = (0..700_000u32).map(|i| i as u8).collect();
    std::fs::write(&src, &data).unwrap();
    let sent = std::sync::atomic::AtomicU64::new(0);
    let count = |n| {
        sent.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    };

    // The folders above the file do not exist yet.
    let key = format!("{base}a/b/file.bin");
    conn.upload_file(&key, &src, &count).await.unwrap();
    assert_eq!(sent.into_inner(), data.len() as u64);
    assert!(conn.object_exists(&key).await.unwrap());
    conn.create_folder(&format!("{base}empty/")).await.unwrap();

    let page = conn.list_page(&base).await.unwrap();
    let mut names: Vec<_> = page
        .entries
        .iter()
        .map(|e| (e.key.as_str(), e.is_dir))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            (&*format!("{base}a/"), true),
            (&*format!("{base}empty/"), true)
        ]
    );

    assert_eq!(conn.get_bytes(&key).await.unwrap(), data);
    let dest = local.path().join("out/dst.bin");
    conn.download_file(&key, &dest, &|_| {}).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);

    let moved = format!("{base}moved/");
    conn.rename(&format!("{base}empty/"), &moved).await.unwrap();
    assert!(conn.object_exists(&moved).await.unwrap());
    assert!(!conn.object_exists(&format!("{base}empty/")).await.unwrap());

    let all = conn.list_all(&base).await.unwrap();
    assert_eq!(all[0], (key.clone(), data.len() as u64));
    assert_eq!(all.last().unwrap().0, base);
    let keys: Vec<String> = all.into_iter().map(|(k, _)| k).collect();
    conn.delete_keys(&keys).await.unwrap();
    assert!(conn.list_all(&base).await.unwrap().is_empty());
}

/// A new host that the user does not trust is refused and not remembered.
/// Run alone, since the answer is installed for the whole process:
/// `SFTP_TEST_HOST=… cargo test sftp_untrusted_host -- --ignored`
#[cfg(test)]
#[tokio::test]
#[ignore = "needs an SSH server that is not in known_hosts"]
async fn sftp_untrusted_host() {
    let _ = CONFIRM_HOST.set(|question| {
        assert!(question.contains("SHA256:"), "{question}");
        let (answer, asked) = tokio::sync::oneshot::channel();
        let _ = answer.send(false);
        asked
    });
    let host = std::env::var("SFTP_TEST_HOST").unwrap();
    let conn = Conn::new(&ServerProfile {
        protocol: crate::config::Protocol::Sftp,
        host: host.clone(),
        ..Default::default()
    });
    let err = format!("{:#}", conn.start_prefix().await.unwrap_err());
    assert!(err.contains("was not trusted"), "{err}");
    let (_, name, port) = parse_host(&host, 22).unwrap();
    assert!(
        known_hosts::known_host_keys(&name, port)
            .unwrap()
            .is_empty()
    );
}
