//! The server behind a tab: S3, SFTP or FTP. SFTP and FTP have no buckets, so
//! their `bucket` arguments are ignored and their keys are paths from the
//! root.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::config::{Protocol, ServerProfile};
use crate::ftp;
use crate::s3::{self, Entry, Page, Progress};
use crate::sftp;

#[derive(Clone)]
pub enum Remote {
    S3(aws_sdk_s3::Client),
    Sftp(Arc<sftp::Conn>),
    Ftp(Arc<ftp::Conn>),
}

impl Remote {
    /// Does not connect yet. Needs a tokio runtime context for S3.
    pub fn new(p: &ServerProfile) -> Remote {
        match p.protocol {
            Protocol::S3 => Remote::S3(s3::build_client(p)),
            Protocol::Sftp => Remote::Sftp(Arc::new(sftp::Conn::new(p))),
            Protocol::Ftp => Remote::Ftp(Arc::new(ftp::Conn::new(p))),
        }
    }

    /// Address shown to the user: `s3://bucket/key`, `sftp://host/path` or
    /// `ftp://host/path`.
    pub fn uri(&self, bucket: &str, key: &str) -> String {
        match self {
            Remote::S3(_) => format!("s3://{bucket}/{key}"),
            Remote::Sftp(c) => c.uri(key),
            Remote::Ftp(c) => c.uri(key),
        }
    }

    /// The key as the server names it: SFTP and FTP paths start at the root.
    pub fn path(&self, key: &str) -> String {
        match self {
            Remote::S3(_) => key.to_string(),
            Remote::Sftp(_) | Remote::Ftp(_) => format!("/{key}"),
        }
    }

    /// Folder prefix a new tab starts in. Connects; S3 has nothing to ask.
    pub async fn start_prefix(&self) -> Result<String> {
        match self {
            Remote::S3(_) => Ok(String::new()),
            Remote::Sftp(c) => c.start_prefix().await,
            Remote::Ftp(c) => c.start_prefix().await,
        }
    }

    pub async fn test_connection(&self, bucket: &str) -> Result<String> {
        match self {
            Remote::S3(c) => s3::test_connection(c, bucket).await,
            Remote::Sftp(c) => c.test_connection().await,
            Remote::Ftp(c) => c.test_connection().await,
        }
    }

    pub async fn list_buckets(&self) -> Result<Vec<Entry>> {
        match self {
            Remote::S3(c) => s3::list_buckets(c).await,
            Remote::Sftp(_) | Remote::Ftp(_) => Ok(Vec::new()),
        }
    }

    pub async fn list_page(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<String>,
    ) -> Result<Page> {
        match self {
            Remote::S3(c) => s3::list_page(c, bucket, prefix, token).await,
            Remote::Sftp(c) => c.list_page(prefix).await,
            Remote::Ftp(c) => c.list_page(prefix).await,
        }
    }

    pub async fn list_all(&self, bucket: &str, prefix: &str) -> Result<Vec<(String, u64)>> {
        match self {
            Remote::S3(c) => s3::list_all(c, bucket, prefix).await,
            Remote::Sftp(c) => c.list_all(prefix).await,
            Remote::Ftp(c) => c.list_all(prefix).await,
        }
    }

    pub async fn object_exists(&self, bucket: &str, key: &str) -> Result<bool> {
        match self {
            Remote::S3(c) => s3::object_exists(c, bucket, key).await,
            Remote::Sftp(c) => c.object_exists(key).await,
            Remote::Ftp(c) => c.object_exists(key).await,
        }
    }

    pub async fn get_bytes(&self, bucket: &str, key: &str) -> Result<Vec<u8>> {
        match self {
            Remote::S3(c) => s3::get_bytes(c, bucket, key).await,
            Remote::Sftp(c) => c.get_bytes(key).await,
            Remote::Ftp(c) => c.get_bytes(key).await,
        }
    }

    pub async fn download_file(
        &self,
        bucket: &str,
        key: &str,
        dest: &Path,
        progress: Progress<'_>,
    ) -> Result<()> {
        match self {
            Remote::S3(c) => s3::download_file(c, bucket, key, dest, progress).await,
            Remote::Sftp(c) => c.download_file(key, dest, progress).await,
            Remote::Ftp(c) => c.download_file(key, dest, progress).await,
        }
    }

    pub async fn upload_file(
        &self,
        bucket: &str,
        key: &str,
        src: &Path,
        progress: Progress<'_>,
    ) -> Result<()> {
        match self {
            Remote::S3(c) => s3::upload_file(c, bucket, key, src, progress).await,
            Remote::Sftp(c) => c.upload_file(key, src, progress).await,
            Remote::Ftp(c) => c.upload_file(key, src, progress).await,
        }
    }

    pub async fn delete_keys(&self, bucket: &str, keys: &[String]) -> Result<()> {
        match self {
            Remote::S3(c) => s3::delete_keys(c, bucket, keys).await,
            Remote::Sftp(c) => c.delete_keys(keys).await,
            Remote::Ftp(c) => c.delete_keys(keys).await,
        }
    }

    /// Renames a file, or a folder when both keys end in `/`. Refuses to
    /// replace something that already exists.
    pub async fn rename(&self, bucket: &str, from: &str, to: &str) -> Result<()> {
        let taken = match self {
            // An S3 folder exists when anything lives under its prefix.
            Remote::S3(c) if to.ends_with('/') => {
                !s3::list_page(c, bucket, to, None).await?.entries.is_empty()
            }
            _ => self.object_exists(bucket, to).await?,
        };
        if taken {
            anyhow::bail!("{} already exists", self.path(to));
        }
        match self {
            Remote::S3(c) => s3::rename(c, bucket, from, to).await,
            Remote::Sftp(c) => c.rename(from, to).await,
            Remote::Ftp(c) => c.rename(from, to).await,
        }
    }

    /// `key` must end in `/`.
    pub async fn create_folder(&self, bucket: &str, key: &str) -> Result<()> {
        match self {
            Remote::S3(c) => s3::create_folder(c, bucket, key).await,
            Remote::Sftp(c) => c.create_folder(key).await,
            Remote::Ftp(c) => c.create_folder(key).await,
        }
    }
}

/// Absolute remote path of an SFTP or FTP key.
pub fn abs(key: &str) -> String {
    format!("/{}", key.trim_matches('/'))
}

/// The folders above an absolute path, outermost first: `/a/b/c.txt` gives
/// `/a` and `/a/b`.
pub fn ancestors(path: &str) -> Vec<String> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    (1..parts.len())
        .map(|n| format!("/{}", parts[..n].join("/")))
        .collect()
}

/// Copies everything from `src` to `dst`, reporting each chunk.
pub async fn pump<R, W>(src: &mut R, dst: &mut W, progress: Progress<'_>) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = src.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        dst.write_all(&buf[..n]).await?;
        progress(n as u64);
    }
}

/// Writes a download to `dest`, creating the folders above it.
pub async fn save_to<R>(src: &mut R, dest: &Path, progress: Progress<'_>) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let result = async {
        let mut file = tokio::fs::File::create(dest).await?;
        pump(src, &mut file, progress).await?;
        file.flush().await
    }
    .await;
    if let Err(e) = result {
        // Do not leave a truncated file behind.
        let _ = tokio::fs::remove_file(dest).await;
        return Err(e).with_context(|| format!("cannot write {}", dest.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(abs(""), "/");
        assert_eq!(abs("home/me/"), "/home/me");
        assert_eq!(abs("home/me/a.txt"), "/home/me/a.txt");
        assert_eq!(ancestors("/a/b/c.txt"), ["/a", "/a/b"]);
        assert!(ancestors("/c.txt").is_empty());
    }
}
