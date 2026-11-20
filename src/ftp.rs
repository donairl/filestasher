//! Plain FTP. Keys have the same shape as in [`crate::sftp`]: absolute remote
//! paths without the leading slash, folders ending in `/`.
// ponytail: no FTPS, so the password and the data travel unencrypted. Add the
// suppaftp rustls feature and a "TLS" option when a server offers it.

use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use suppaftp::list::File;
use suppaftp::tokio::AsyncFtpStream;
use suppaftp::types::FileType;
use tokio::io::AsyncReadExt;
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};

use crate::config::ServerProfile;
use crate::remote::{abs, ancestors, pump, save_to};
use crate::s3::{Entry, Page, Progress};
use crate::sftp::parse_host;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

async fn connect(p: &ServerProfile) -> Result<AsyncFtpStream> {
    let host = p.host.trim();
    let spec = match host.contains('@') {
        true => host.to_string(),
        false => format!("anonymous@{host}"),
    };
    let (user, host, port) = parse_host(&spec, 21)?;
    let mut ftp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        AsyncFtpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timed out"))
    .and_then(|r| Ok(r?))
    .with_context(|| format!("cannot connect to {host}:{port}"))?;
    // Servers behind NAT announce an address that cannot be reached; the
    // data connection goes to the host we already talk to instead.
    ftp.set_passive_nat_workaround(true);
    ftp.login(user.as_str(), p.password.as_str())
        .await
        .with_context(|| format!("login as {user} failed"))?;
    ftp.transfer_type(FileType::Binary).await?;
    Ok(ftp)
}

/// Lists a folder. Lines that are not entries, such as `total 12`, are
/// dropped.
async fn list(ftp: &mut AsyncFtpStream, dir: &str) -> Result<Vec<File>> {
    // `LIST <path>` breaks on names with spaces on some servers, so the
    // folder is entered first.
    ftp.cwd(dir).await.with_context(|| format!("open {dir}"))?;
    let lines = ftp
        .list(None)
        .await
        .with_context(|| format!("list {dir}"))?;
    Ok(lines
        .iter()
        .filter_map(|l| File::from_str(l).ok())
        .filter(|f| !matches!(f.name(), "." | ".."))
        .collect())
}

/// The entry at `key`, looked up in the listing of its parent. FTP has no
/// portable way to ask about a single path.
async fn find(ftp: &mut AsyncFtpStream, key: &str) -> Option<File> {
    let path = abs(key);
    let (parent, name) = path.rsplit_once('/')?;
    let parent = if parent.is_empty() { "/" } else { parent };
    let files = list(ftp, parent).await.ok()?;
    files.into_iter().find(|f| f.name() == name)
}

/// Lazily opened FTP connections. A connection runs one command at a time,
/// so file transfers get their own: browsing stays usable during a transfer.
// ponytail: transfers still run one at a time, and a server that stops
// answering blocks its connection for good (no command timeout).
pub struct Conn {
    profile: ServerProfile,
    browse: Mutex<Option<AsyncFtpStream>>,
    transfer: Mutex<Option<AsyncFtpStream>>,
}

impl Conn {
    pub fn new(profile: &ServerProfile) -> Conn {
        Conn {
            profile: profile.clone(),
            browse: Mutex::new(None),
            transfer: Mutex::new(None),
        }
    }

    /// `ftp://user@host/key`.
    pub fn uri(&self, key: &str) -> String {
        format!("ftp://{}/{key}", self.profile.host.trim())
    }

    /// The connection in `slot`, opened now if it is missing or went stale.
    async fn lock<'a>(
        &self,
        slot: &'a Mutex<Option<AsyncFtpStream>>,
    ) -> Result<MappedMutexGuard<'a, AsyncFtpStream>> {
        let mut guard = slot.lock().await;
        let alive = match guard.as_mut() {
            Some(ftp) => ftp.noop().await.is_ok(),
            None => false,
        };
        if !alive {
            *guard = Some(connect(&self.profile).await?);
        }
        Ok(MutexGuard::map(guard, |ftp| {
            ftp.as_mut().expect("connected above")
        }))
    }

    /// Folder prefix a new tab starts in: the profile's start folder, or the
    /// folder the server puts the login in.
    pub async fn start_prefix(&self) -> Result<String> {
        let mut ftp = self.lock(&self.browse).await?;
        let start = self.profile.bucket.trim();
        if !start.is_empty() {
            ftp.cwd(start)
                .await
                .with_context(|| format!("cannot open {start}"))?;
        }
        let path = ftp.pwd().await?;
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
        let mut ftp = self.lock(&self.browse).await?;
        let dir = abs(prefix);
        let mut entries = Vec::new();
        for f in list(&mut ftp, &dir).await? {
            let name = f.name().to_string();
            // A link to a folder opens like a folder.
            let is_dir = f.is_directory()
                || (f.is_symlink() && ftp.cwd(abs(&format!("{prefix}{name}"))).await.is_ok());
            entries.push(Entry {
                key: format!("{prefix}{name}{}", if is_dir { "/" } else { "" }),
                name,
                is_dir,
                size: f.size() as u64,
                modified: f
                    .modified()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs() as i64),
            });
        }
        Ok(Page {
            entries,
            next: None,
        })
    }

    /// Same contract as [`crate::sftp::Conn::list_all`]: files first, then
    /// folders deepest first and `prefix` last; links are never followed.
    pub async fn list_all(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let mut ftp = self.lock(&self.browse).await?;
        if !prefix.is_empty() {
            match find(&mut ftp, prefix).await {
                Some(f) if f.is_directory() => {}
                // A link to a folder is one file here: deleting it must
                // remove the link, never what it points to.
                Some(_) => return Ok(vec![(prefix.trim_end_matches('/').to_string(), 0)]),
                None => return Ok(Vec::new()),
            }
        }
        let mut files = Vec::new();
        let mut dirs = vec![prefix.to_string()];
        let mut next = 0;
        while next < dirs.len() {
            let dir = dirs[next].clone();
            next += 1;
            for f in list(&mut ftp, &abs(&dir)).await? {
                if f.is_directory() {
                    dirs.push(format!("{dir}{}/", f.name()));
                } else {
                    files.push((format!("{dir}{}", f.name()), f.size() as u64));
                }
            }
        }
        // Every folder was found after its parent.
        files.extend(dirs.into_iter().rev().map(|d| (d, 0)));
        Ok(files)
    }

    pub async fn object_exists(&self, key: &str) -> Result<bool> {
        let mut ftp = self.lock(&self.browse).await?;
        Ok(find(&mut ftp, key).await.is_some())
    }

    pub async fn get_bytes(&self, key: &str) -> Result<Vec<u8>> {
        let path = abs(key);
        let mut ftp = self.lock(&self.browse).await?;
        let mut src = ftp
            .retr_as_stream(&path)
            .await
            .with_context(|| format!("download {path}"))?;
        let mut data = Vec::new();
        src.read_to_end(&mut data)
            .await
            .with_context(|| format!("download {path}"))?;
        src.finish().await?;
        Ok(data)
    }

    pub async fn download_file(
        &self,
        key: &str,
        dest: &Path,
        progress: Progress<'_>,
    ) -> Result<()> {
        let path = abs(key);
        let mut ftp = self.lock(&self.transfer).await?;
        let mut src = ftp
            .retr_as_stream(&path)
            .await
            .with_context(|| format!("download {path}"))?;
        save_to(&mut src, dest, progress).await?;
        src.finish()
            .await
            .with_context(|| format!("download {path}"))
    }

    pub async fn upload_file(&self, key: &str, src: &Path, progress: Progress<'_>) -> Result<()> {
        let path = abs(key);
        let mut file = tokio::fs::File::open(src)
            .await
            .with_context(|| format!("cannot read {}", src.display()))?;
        let mut ftp = self.lock(&self.transfer).await?;
        let mut dst = match ftp.put_with_stream(&path).await {
            Ok(s) => s,
            // Most likely the folders above it are missing.
            Err(_) => {
                for dir in ancestors(&path) {
                    // Fails for the folders that exist; the upload below
                    // reports anything that really went wrong.
                    let _ = ftp.mkdir(dir).await;
                }
                ftp.put_with_stream(&path)
                    .await
                    .with_context(|| format!("upload {path}"))?
            }
        };
        pump(&mut file, &mut dst, progress)
            .await
            .with_context(|| format!("upload {path}"))?;
        dst.finish().await.with_context(|| format!("upload {path}"))
    }

    /// Keys in the order of [`Conn::list_all`]: a key ending in `/` is a
    /// folder, which must be empty by the time it is reached.
    pub async fn delete_keys(&self, keys: &[String]) -> Result<()> {
        let mut ftp = self.lock(&self.browse).await?;
        for key in keys {
            let path = abs(key);
            if key.ends_with('/') {
                ftp.rmdir(&path).await
            } else {
                ftp.rm(&path).await
            }
            .with_context(|| format!("delete {path}"))?;
        }
        Ok(())
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (from, to) = (abs(from), abs(to));
        let mut ftp = self.lock(&self.browse).await?;
        ftp.rename(&from, &to)
            .await
            .with_context(|| format!("rename {from}"))
    }

    pub async fn create_folder(&self, key: &str) -> Result<()> {
        let path = abs(key);
        let mut ftp = self.lock(&self.browse).await?;
        ftp.mkdir(&path)
            .await
            .with_context(|| format!("create folder {path}"))
    }
}

/// End-to-end check against a real server:
/// `FTP_TEST_HOST=user@host:port FTP_TEST_PASSWORD=… [FTP_TEST_DIR=…]
/// cargo test ftp::ftp_round_trip -- --ignored`
#[cfg(test)]
#[tokio::test]
#[ignore = "needs an FTP server"]
async fn ftp_round_trip() {
    let env = |k| std::env::var(k).unwrap_or_default();
    let conn = Conn::new(&ServerProfile {
        protocol: crate::config::Protocol::Ftp,
        host: env("FTP_TEST_HOST"),
        password: env("FTP_TEST_PASSWORD"),
        bucket: env("FTP_TEST_DIR"),
        ..Default::default()
    });
    let base = format!("{}fstasher test/", conn.start_prefix().await.unwrap());
    let local = tempfile::tempdir().unwrap();
    let src = local.path().join("src.bin");
    let data: Vec<u8> = (0..700_000u32).map(|i| i as u8).collect();
    std::fs::write(&src, &data).unwrap();
    let sent = std::sync::atomic::AtomicU64::new(0);
    let count = |n| {
        sent.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    };

    // The folders above the file do not exist yet.
    let key = format!("{base}a/b/file one.bin");
    assert!(!conn.object_exists(&key).await.unwrap());
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
    let inner = conn.list_page(&format!("{base}a/b/")).await.unwrap();
    assert_eq!(inner.entries[0].size, data.len() as u64);
    assert!(inner.entries[0].modified.is_some());

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
