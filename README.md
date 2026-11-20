# S3 FStasher

A desktop file explorer for S3 and S3-compatible storage (AWS S3, MinIO, Ceph,
Cloudflare R2, Wasabi, and others), for SFTP servers over SSH, directly or
through jump hosts, and for FTP servers, written in Rust with a
[Slint](https://slint.dev) UI.

## Features

- Saved server profiles: add, duplicate, edit, delete, and test the connection
  before saving.
- Several servers connected at once, each in its own tab.
- Browse buckets and folders. Large folders load 1000 entries at a time as you
  scroll.
- Sort by name, size or modified time by clicking a column title. The default
  is newest first.
- Search the current folder by name.
- Type or paste a location into the bar above the list and press Enter to go
  there.
- Upload files, with multipart upload for files larger than 32 MiB.
- Download files and whole folders.
- Image preview: double-click a PNG, JPEG, GIF, BMP, WebP or ICO file.
- Right-click menu on the file list: open, preview, download, copy the path or
  the `s3://` URI to the clipboard.
- Local folder pane on the right, with sortable columns, image preview and a
  right-click menu. Drag files and folders from it onto the S3 list to upload,
  or from the S3 list onto it to download.
- Both side panels can be hidden, and resized by dragging their handles.
- Asks before overwriting when a file already exists at the destination.
- Create folders. Rename files and folders.
- Keyboard: F5 refresh, Backspace up, Enter open, Delete delete, F2 rename.
- Delete files and folders, always after a confirmation.
- Transfer progress in the status bar.

Not supported yet: copy/move, rename in the local pane, presigned URLs, preview
of non-image files, drag and drop from other applications, creating or
deleting buckets.

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
4. Click a row to select it. Ctrl+click selects several rows, Shift+click
   selects a range. The keyboard shortcuts act on the list clicked last.
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

### SFTP (SSH) settings

Choose **SFTP (SSH)** as the protocol to browse a server over SSH.

| Field         | Notes                                                              |
| ------------- | ------------------------------------------------------------------ |
| Host          | `user@host` or `user@host:port`. Port 22 when left out.            |
| Password      | Login password. Also the passphrase of the key file. Optional.     |
| Key file      | Private key. Blank tries `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`. |
| Start folder  | Folder the tab opens in. Blank opens the home folder.              |
| Jump host     | Optional `user@host:port` to tunnel through, like `ssh -J`. Several hosts, comma separated, are chained in order. |
| Jump password | Password for the jump hosts. Blank uses **Password**.              |

The key file is tried first, then the password; the same key file is used on
the jump hosts. Host keys are checked against `~/.ssh/known_hosts`: for a host
seen for the first time the app shows the key fingerprint and asks before
adding it, and a host whose key changed is refused.
ssh-agent and one-time-code (keyboard-interactive) logins are not supported.

### FTP settings

Choose **FTP** as the protocol.

| Field        | Notes                                                              |
| ------------ | ------------------------------------------------------------------ |
| Host         | `user@host` or `user@host:port`. Port 21 when left out; without a user the login is `anonymous`. |
| Password     | Login password.                                                    |
| Start folder | Folder the tab opens in. Blank opens the folder the server logs you into. |

> **Security:** this is plain FTP, in passive mode. The password and the files
> travel unencrypted; FTPS (FTP over TLS) is not supported. Prefer SFTP when
> the server offers it.

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

[[servers]]
name = "app-server"
protocol = "sftp"
host = "deploy@10.0.0.5"
key_file = "~/.ssh/id_ed25519"
jump = "ops@bastion.example.com:2222"
bucket = "/var/www"

[[servers]]
name = "old-nas"
protocol = "ftp"
host = "backup@192.168.1.20"
password = "..."
```

Secret keys and passwords are written encrypted (AES-256-GCM); in the file
they look like `secret_key = "enc:v1:…"`. The key is `servers.key` in the same
folder, made on the first save. A value typed in by hand as plain text is
accepted and encrypted the next time the application starts.

To move profiles to another computer, copy both `servers.toml` and
`servers.key`. Without the key file the passwords cannot be read and have to
be entered again.

> **Security:** the encryption keeps passwords out of sight and out of a
> `servers.toml` that is shared or backed up on its own. It does not protect
> against someone who can read the whole folder: with `servers.key` they can
> decrypt everything. On Unix both files are created with mode `0600`. Do not
> commit or share `servers.key`.

## Behaviour notes

- Folders are S3 key prefixes separated by `/`. **New Folder** writes an empty
  object whose key ends in `/`.
- Deleting a folder deletes every object under its prefix. This cannot be
  undone.
- Downloads go to a folder you pick. Object keys that would escape that folder,
  such as keys containing `..`, are skipped and counted in the status message.
- On SFTP and FTP, folders are real folders and a whole folder is listed at once.
  Deleting a link to a folder removes the link, not what it points to; a
  linked folder cannot be downloaded as part of a folder download.
- A file that fails during an upload or download is skipped; the rest of the
  batch continues and the status bar reports how many failed, with the first
  error.
- **Cancel** in the status bar stops all running transfers. The file that was
  in progress is removed again; files already transferred stay. On S3, the
  parts of a cancelled large upload stay on the server until the bucket's
  lifecycle rule for incomplete multipart uploads removes them.
- **Rename** is in the right-click menu. It refuses a name that is already
  taken. S3 has no rename: every object is copied to its new key and the old
  one deleted, so renaming a large folder takes one request per object, and
  objects over 5 GB cannot be renamed.
- Errors are shown in the status bar of the tab.

## Project layout

| Path            | Purpose                                                    |
| --------------- | ---------------------------------------------------------- |
| `src/main.rs`   | Application state, UI callbacks, background jobs.          |
| `src/s3.rs`     | S3 operations on top of `aws-sdk-s3`.                      |
| `src/sftp.rs`   | SFTP operations and SSH jump hosts on top of `russh`.      |
| `src/ftp.rs`    | FTP operations on top of `suppaftp`.                       |
| `src/remote.rs` | Picks S3, SFTP or FTP for a tab.                           |
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
