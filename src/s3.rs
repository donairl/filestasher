//! Thin wrapper over aws-sdk-s3.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use aws_sdk_s3::Client;
use aws_sdk_s3::config::timeout::TimeoutConfig;
use aws_sdk_s3::config::{
    BehaviorVersion, Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use aws_sdk_s3::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::{ByteStream, Length};
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier};
use tokio::io::AsyncWriteExt;

use crate::config::ServerProfile;

const MULTIPART_THRESHOLD: u64 = 32 * 1024 * 1024;
const MIN_PART_SIZE: u64 = 16 * 1024 * 1024;
const MAX_PARTS: u64 = 10_000;
/// Entries fetched per listing request.
const PAGE_SIZE: i32 = 1000;

/// Called with the number of bytes just transferred.
pub type Progress<'a> = &'a (dyn Fn(u64) + Send + Sync);

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    /// Full key for objects, full prefix (ending in `/`) for folders, bucket
    /// name for buckets.
    pub key: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<i64>,
}

fn sdk_err<E, R>(what: &str, e: SdkError<E, R>) -> anyhow::Error
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug,
{
    match (e.code(), e.message()) {
        (Some(code), Some(msg)) => anyhow!("{what}: {code}: {msg}"),
        (Some(code), None) => anyhow!("{what}: {code}"),
        _ => anyhow!("{what}: {}", DisplayErrorContext(&e)),
    }
}

pub fn build_client(p: &ServerProfile) -> Client {
    let region = match p.region.trim() {
        "" => "us-east-1",
        r => r,
    };
    let mut builder = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(region.to_string()))
        .credentials_provider(Credentials::new(
            p.access_key.trim(),
            p.secret_key.trim(),
            None,
            None,
            "s3-fstasher",
        ))
        .force_path_style(p.path_style)
        // Many S3-compatible servers reject the newer default checksums.
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(Duration::from_secs(10))
                .build(),
        );
    let endpoint = p.endpoint.trim();
    if !endpoint.is_empty() {
        builder = builder.endpoint_url(endpoint);
    }
    Client::from_conf(builder.build())
}

/// Checks that the profile can reach its default bucket, or list buckets when
/// none is set.
pub async fn test_connection(client: &Client, bucket: &str) -> Result<String> {
    if bucket.is_empty() {
        let n = list_buckets(client).await?.len();
        Ok(format!("Connection OK, {n} bucket(s) visible."))
    } else {
        client
            .head_bucket()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| sdk_err("head bucket", e))?;
        Ok(format!("Connection OK, bucket \"{bucket}\" reachable."))
    }
}

pub async fn list_buckets(client: &Client) -> Result<Vec<Entry>> {
    let resp = client
        .list_buckets()
        .send()
        .await
        .map_err(|e| sdk_err("list buckets", e))?;
    let mut out: Vec<Entry> = resp
        .buckets()
        .iter()
        .filter_map(|b| {
            let name = b.name()?.to_string();
            Some(Entry {
                key: name.clone(),
                name,
                is_dir: true,
                size: 0,
                modified: b.creation_date().map(|d| d.secs()),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// One page of a folder listing.
pub struct Page {
    pub entries: Vec<Entry>,
    /// Token for the next page, `None` on the last one.
    pub next: Option<String>,
}

/// Lists one page of a folder level, at most [`PAGE_SIZE`] entries. Within the
/// page sub-folders come first, then objects. Pass the previous page's `next`
/// as `token` to continue.
pub async fn list_page(
    client: &Client,
    bucket: &str,
    prefix: &str,
    token: Option<String>,
) -> Result<Page> {
    let resp = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .delimiter("/")
        .max_keys(PAGE_SIZE)
        .set_continuation_token(token)
        .send()
        .await
        .map_err(|e| sdk_err("list objects", e))?;
    let mut out = Vec::new();
    for cp in resp.common_prefixes() {
        let Some(p) = cp.prefix() else { continue };
        let name = p.strip_prefix(prefix).unwrap_or(p).trim_end_matches('/');
        if name.is_empty() {
            continue;
        }
        out.push(Entry {
            name: name.to_string(),
            key: p.to_string(),
            is_dir: true,
            size: 0,
            modified: None,
        });
    }
    for obj in resp.contents() {
        let Some(key) = obj.key() else { continue };
        // The folder's own marker object.
        if key == prefix {
            continue;
        }
        out.push(Entry {
            name: key.strip_prefix(prefix).unwrap_or(key).to_string(),
            key: key.to_string(),
            is_dir: false,
            size: obj.size().unwrap_or(0).max(0) as u64,
            modified: obj.last_modified().map(|d| d.secs()),
        });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    let next = match resp.is_truncated() {
        Some(true) => resp.next_continuation_token().map(str::to_string),
        _ => None,
    };
    Ok(Page { entries: out, next })
}

/// Every object under `prefix`, as (key, size).
pub async fn list_all(client: &Client, bucket: &str, prefix: &str) -> Result<Vec<(String, u64)>> {
    let mut out = Vec::new();
    let mut pages = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .into_paginator()
        .send();
    while let Some(page) = pages.next().await {
        let page = page.map_err(|e| sdk_err("list objects", e))?;
        for obj in page.contents() {
            if let Some(key) = obj.key() {
                out.push((key.to_string(), obj.size().unwrap_or(0).max(0) as u64));
            }
        }
    }
    Ok(out)
}

/// Reads a whole object into memory.
pub async fn get_bytes(client: &Client, bucket: &str, key: &str) -> Result<Vec<u8>> {
    let resp = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| sdk_err(&format!("download {key}"), e))?;
    let data = resp
        .body
        .collect()
        .await
        .with_context(|| format!("cannot read {key}"))?;
    Ok(data.to_vec())
}

pub async fn download_file(
    client: &Client,
    bucket: &str,
    key: &str,
    dest: &Path,
    progress: Progress<'_>,
) -> Result<()> {
    let resp = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| sdk_err(&format!("download {key}"), e))?;
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let result = async {
        let mut file = tokio::fs::File::create(dest).await?;
        let mut body = resp.body;
        while let Some(chunk) = body.try_next().await.map_err(std::io::Error::other)? {
            file.write_all(&chunk).await?;
            progress(chunk.len() as u64);
        }
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

pub async fn upload_file(
    client: &Client,
    bucket: &str,
    key: &str,
    src: &Path,
    progress: Progress<'_>,
) -> Result<()> {
    let size = tokio::fs::metadata(src)
        .await
        .with_context(|| format!("cannot read {}", src.display()))?
        .len();
    if size <= MULTIPART_THRESHOLD {
        let body = ByteStream::from_path(src)
            .await
            .with_context(|| format!("cannot read {}", src.display()))?;
        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .map_err(|e| sdk_err(&format!("upload {key}"), e))?;
        progress(size);
        return Ok(());
    }

    let upload_id = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| sdk_err(&format!("upload {key}"), e))?
        .upload_id()
        .context("server returned no upload id")?
        .to_string();

    match upload_parts(client, bucket, key, src, size, &upload_id, progress).await {
        Ok(parts) => {
            client
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                )
                .send()
                .await
                .map_err(|e| sdk_err(&format!("upload {key}"), e))?;
            Ok(())
        }
        Err(e) => {
            // Best effort: free the parts already stored on the server.
            let _ = client
                .abort_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .send()
                .await;
            Err(e)
        }
    }
}

async fn upload_parts(
    client: &Client,
    bucket: &str,
    key: &str,
    src: &Path,
    size: u64,
    upload_id: &str,
    progress: Progress<'_>,
) -> Result<Vec<CompletedPart>> {
    let part_size = MIN_PART_SIZE.max(size.div_ceil(MAX_PARTS));
    let mut parts = Vec::new();
    let mut offset = 0u64;
    let mut number = 1i32;
    while offset < size {
        let len = part_size.min(size - offset);
        let body = ByteStream::read_from()
            .path(src)
            .offset(offset)
            .length(Length::Exact(len))
            .build()
            .await
            .with_context(|| format!("cannot read {}", src.display()))?;
        let resp = client
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(number)
            .body(body)
            .send()
            .await
            .map_err(|e| sdk_err(&format!("upload {key} part {number}"), e))?;
        parts.push(
            CompletedPart::builder()
                .part_number(number)
                .set_e_tag(resp.e_tag().map(str::to_string))
                .build(),
        );
        progress(len);
        offset += len;
        number += 1;
    }
    Ok(parts)
}

pub async fn delete_keys(client: &Client, bucket: &str, keys: &[String]) -> Result<()> {
    for chunk in keys.chunks(1000) {
        let ids = chunk
            .iter()
            .map(|k| ObjectIdentifier::builder().key(k).build())
            .collect::<Result<Vec<_>, _>>()
            .context("invalid object key")?;
        let delete = Delete::builder()
            .set_objects(Some(ids))
            .quiet(true)
            .build()
            .context("invalid delete request")?;
        let resp = client
            .delete_objects()
            .bucket(bucket)
            .delete(delete)
            .send()
            .await
            .map_err(|e| sdk_err("delete", e))?;
        if let Some(first) = resp.errors().first() {
            bail!(
                "delete failed for {} object(s), first: {}: {}",
                resp.errors().len(),
                first.key().unwrap_or("?"),
                first.message().unwrap_or("unknown error"),
            );
        }
    }
    Ok(())
}

/// `key` must end in `/`.
pub async fn create_folder(client: &Client, bucket: &str, key: &str) -> Result<()> {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from_static(b""))
        .send()
        .await
        .map_err(|e| sdk_err("create folder", e))?;
    Ok(())
}
