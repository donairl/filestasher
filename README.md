# S3 FStasher

A desktop file explorer for S3 and S3-compatible storage (AWS S3, MinIO, Ceph,
Cloudflare R2, Wasabi, and others), written in Rust with a
[Slint](https://slint.dev) UI.

## Features

- Saved server profiles: add, edit, delete, and test the connection before
  saving.
- Several servers connected at once, each in its own tab.
- Browse buckets and folders. Large folders load 1000 entries at a time as you
  scroll.
- Sort by name, size or modified time by clicking a column title. The default
  is newest first.
- Search the current folder by name.
- Upload files, with multipart upload for files larger than 32 MiB.
- Download files and whole folders.
- Image preview: double-click a PNG, JPEG, GIF, BMP, WebP or ICO file.
- Right-click menu on the file list: open, preview, download, copy the path or
  the `s3://` URI to the clipboard.
- Create folders.
- Delete files and folders, always after a confirmation.
- Transfer progress in the status bar.

Not supported yet: uploading folders, rename/copy/move, presigned URLs, preview
of non-image files, drag and drop, creating or deleting buckets.

## Requirements

- Rust 1.85 or newer (the crate uses the 2024 edition).
- A C compiler, needed to build the TLS backend.
- On Linux, a running XDG desktop portal for the file dialogs. Most desktop
  environments provide one.

## Build and run

```sh
cargo run --release
```

The binary is written to `target/release/s3-fstasher`.

## Usage

1. Click **Add** and fill in the server details, then **Test Connection** and
   **Save**.
2. Double-click a server, or select it and click **Connect**. Each connection
   opens in a new tab.
3. Double-click a bucket or folder to open it. **Up** goes one level back.
4. Click a row to select it. Ctrl+click selects several rows.
5. Use **Upload**, **Download**, **New Folder** and **Delete** on the toolbar.

### Server settings

| Field          | Notes                                                           |
| -------------- | --------------------------------------------------------------- |
| Name           | Label shown in the sidebar and on the tab.                      |
| Endpoint URL   | Must start with `http://` or `https://`. Leave blank for AWS.   |
| Region         | Defaults to `us-east-1` when blank.                             |
| Access key     | Required.                                                       |
| Secret key     | Required.                                                       |
| Default bucket | Optional. The tab opens this bucket directly on connect.        |
| Path-style     | Enable for MinIO, Ceph and most self-hosted servers.            |

Set a default bucket when the credentials are not allowed to list buckets.

## Configuration file

Profiles are stored in `servers.toml` inside the user configuration directory:

| Platform | Path                                                      |
| -------- | --------------------------------------------------------- |
| Linux    | `~/.config/s3-fstasher/servers.toml`                      |
| macOS    | `~/Library/Application Support/s3-fstasher/servers.toml`  |
| Windows  | `%APPDATA%\s3-fstasher\servers.toml`                      |

```toml
[[servers]]
name = "minio-dev"
endpoint = "http://localhost:9000"
region = "us-east-1"
access_key = "..."
secret_key = "..."
path_style = true
bucket = ""
```

> **Security:** secret keys are stored in plaintext. Anyone who can read this
> file gets full access to those servers. On Unix the file is created with mode
> `0600`, which only protects it from other users on the same machine. Do not
> commit or share this file.

## Behaviour notes

- Folders are S3 key prefixes separated by `/`. **New Folder** writes an empty
  object whose key ends in `/`.
- Deleting a folder deletes every object under its prefix. This cannot be
  undone.
- Downloads go to a folder you pick. Object keys that would escape that folder,
  such as keys containing `..`, are skipped and counted in the status message.
- Errors are shown in the status bar of the tab.

## Project layout

| Path            | Purpose                                                    |
| --------------- | ---------------------------------------------------------- |
| `src/main.rs`   | Application state, UI callbacks, background jobs.          |
| `src/s3.rs`     | S3 operations on top of `aws-sdk-s3`.                      |
| `src/config.rs` | Server profiles, load and save.                            |
| `src/util.rs`   | Size and time formatting, prefix navigation, safe paths.   |
| `ui/app.slint`  | Window, sidebar, tabs, file list and dialogs.              |

S3 calls run on a tokio runtime, and results are applied on the UI thread, so
the window stays responsive during transfers.

## Releasing

Pushing a tag that starts with `v` runs `.github/workflows/release.yml`. It
builds the Linux and Windows binaries and attaches them to a GitHub release
for that tag:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The release gets `s3-fstasher-<tag>-linux-x86_64.tar.gz` and
`s3-fstasher-<tag>-windows-x86_64.zip`. Keep the tag in step with `version` in
`Cargo.toml`.

## Tests

```sh
cargo test
```

The unit tests cover the configuration file and the pure helpers. S3 operations
are verified manually against a real endpoint.
