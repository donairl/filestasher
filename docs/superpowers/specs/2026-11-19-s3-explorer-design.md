# Filestasher — design

Desktop S3 file explorer written in Rust with a Slint UI.

## Scope

- Saved server profiles: add, edit, delete, test connection.
- Several servers connected at once, one tab per connection.
- Per tab: list buckets, browse folders, upload files, download files and
  folders, create folder, delete files and folders.

Out of scope: rename/copy/move, presigned URLs, preview, drag and drop, sync,
bucket creation and deletion.

## Architecture

- `src/main.rs` — UI thread. Owns all state (`App`), wires Slint callbacks,
  spawns S3 work on a tokio runtime and applies results back on the UI thread
  through `slint::invoke_from_event_loop`.
- `src/s3.rs` — thin wrapper over `aws-sdk-s3`: build client, list buckets,
  list a folder, list recursively, download, upload (multipart above 32 MiB),
  delete, create folder, test connection.
- `src/config.rs` — server profiles, load and save.
- `src/util.rs` — pure helpers: size formatting, prefix navigation, safe local
  paths for downloaded keys.
- `ui/app.slint` — window, sidebar, tab bar, file list, dialogs.

Each tab owns its own S3 client, bucket, prefix and listing. Async results are
addressed by tab id and a generation counter, so a closed tab or a newer
navigation discards stale results.

## Server profiles

Stored in `~/.config/filestasher/servers.toml`, written with mode `0600`.
Fields: name, endpoint URL (blank means AWS), region, access key, secret key,
path-style flag, optional default bucket.

The secret key is stored in plaintext by the user's choice. Anyone who can read
the file gets full access to those servers.

## Behaviour notes

- Folders are S3 common prefixes with delimiter `/`. "New folder" writes an
  empty object whose key ends in `/`.
- Deleting a folder deletes every object under its prefix, after confirmation.
- Downloads go to a folder the user picks. Keys are mapped to relative paths
  with `..` and absolute components rejected.
- Errors are shown in the tab's status bar.

## Testing

Unit tests cover config round trip and file permissions, and the pure helpers.
S3 calls are verified manually against a real endpoint.
