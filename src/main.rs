//! S3 FStasher: an S3 file explorer.
//!
//! All state lives on the UI thread in [`App`]. S3 work runs on a tokio
//! runtime and reports back through [`on_ui`].

mod config;
mod s3;
mod util;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use aws_sdk_s3::Client;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use config::{Config, ServerProfile};
use s3::Entry;
use util::{format_size, format_time, parent_prefix, safe_join};

slint::include_modules!();

struct Tab {
    id: u64,
    title: String,
    client: Client,
    /// `None` shows the bucket list.
    bucket: Option<String>,
    prefix: String,
    entries: Vec<Entry>,
    selected: Vec<bool>,
    status: String,
    busy: bool,
    /// Bumped on every load; results from older loads are dropped.
    generation: u64,
}

impl Tab {
    fn location(&self) -> String {
        match &self.bucket {
            None => "s3://".to_string(),
            Some(b) => format!("s3://{b}/{}", self.prefix),
        }
    }

    fn selected_entries(&self) -> Vec<Entry> {
        self.entries
            .iter()
            .zip(&self.selected)
            .filter(|(_, sel)| **sel)
            .map(|(e, _)| e.clone())
            .collect()
    }
}

enum Pending {
    DeleteServer(usize),
    DeleteObjects {
        tab_id: u64,
        bucket: String,
        items: Vec<Entry>,
    },
}

#[derive(Default)]
struct State {
    config: Config,
    config_path: PathBuf,
    tabs: Vec<Tab>,
    active: Option<usize>,
    next_tab_id: u64,
    /// Job id -> (bytes done, bytes total).
    transfers: BTreeMap<u64, (u64, u64)>,
    /// Index of the profile being edited, `None` when adding.
    editing: Option<usize>,
    pending: Option<Pending>,
    /// Shown when no tab is open.
    global_status: String,
}

impl State {
    fn tab_mut(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn active_tab(&self) -> Option<&Tab> {
        self.active.and_then(|i| self.tabs.get(i))
    }

    fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.active.and_then(|i| self.tabs.get_mut(i))
    }
}

struct App {
    ui: slint::Weak<MainWindow>,
    rt: tokio::runtime::Handle,
    state: RefCell<State>,
    entries: Rc<VecModel<FileEntry>>,
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// Runs `f` on the UI thread. Must be called from the UI thread.
fn with_app(f: impl FnOnce(&Rc<App>)) {
    let app = APP.with(|a| a.borrow().clone());
    if let Some(app) = app {
        f(&app);
    }
}

/// Queues `f` to run on the UI thread, from any thread.
fn on_ui(f: impl FnOnce(&Rc<App>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || with_app(f));
}

/// A running transfer shown in the status bar. Removed from the UI on drop.
struct Job {
    id: u64,
    done: AtomicU64,
    total: u64,
    last_push: Mutex<Instant>,
}

impl Job {
    fn start(total: u64) -> Arc<Job> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let job = Arc::new(Job {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            done: AtomicU64::new(0),
            total,
            last_push: Mutex::new(Instant::now()),
        });
        job.push();
        job
    }

    fn add(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
        let mut last = self.last_push.lock().unwrap();
        if last.elapsed() >= Duration::from_millis(100) {
            *last = Instant::now();
            drop(last);
            self.push();
        }
    }

    fn push(&self) {
        let (id, done, total) = (self.id, self.done.load(Ordering::Relaxed), self.total);
        on_ui(move |app| {
            app.state.borrow_mut().transfers.insert(id, (done, total));
            app.sync_transfers();
        });
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        let id = self.id;
        on_ui(move |app| {
            app.state.borrow_mut().transfers.remove(&id);
            app.sync_transfers();
        });
    }
}

impl App {
    fn ui(&self) -> MainWindow {
        self.ui.unwrap()
    }

    // ---- UI sync ---------------------------------------------------------

    fn sync_servers(&self) {
        let st = self.state.borrow();
        let items: Vec<ServerItem> = st
            .config
            .servers
            .iter()
            .map(|s| ServerItem {
                name: s.name.as_str().into(),
                endpoint: if s.endpoint.is_empty() {
                    format!("AWS {}", s.region).into()
                } else {
                    s.endpoint.as_str().into()
                },
            })
            .collect();
        let ui = self.ui();
        if ui.get_selected_server() >= items.len() as i32 {
            ui.set_selected_server(-1);
        }
        ui.set_servers(ModelRc::new(VecModel::from(items)));
    }

    fn sync_view(&self) {
        let st = self.state.borrow();
        let ui = self.ui();
        let titles: Vec<SharedString> = st.tabs.iter().map(|t| t.title.as_str().into()).collect();
        ui.set_tabs(ModelRc::new(VecModel::from(titles)));
        ui.set_active_tab(st.active.map_or(-1, |i| i as i32));
        match st.active_tab() {
            Some(t) => {
                ui.set_location(t.location().into());
                ui.set_status(t.status.as_str().into());
                ui.set_busy(t.busy);
                ui.set_in_bucket(t.bucket.is_some());
                let rows: Vec<FileEntry> = t
                    .entries
                    .iter()
                    .zip(&t.selected)
                    .map(|(e, sel)| FileEntry {
                        name: e.name.as_str().into(),
                        is_dir: e.is_dir,
                        size: if e.is_dir {
                            SharedString::new()
                        } else {
                            format_size(e.size).into()
                        },
                        modified: e.modified.map(format_time).unwrap_or_default().into(),
                        selected: *sel,
                    })
                    .collect();
                self.entries.set_vec(rows);
            }
            None => {
                ui.set_location(SharedString::new());
                ui.set_status(st.global_status.as_str().into());
                ui.set_busy(false);
                ui.set_in_bucket(false);
                self.entries.set_vec(Vec::new());
            }
        }
    }

    fn sync_transfers(&self) {
        let st = self.state.borrow();
        let ui = self.ui();
        let count = st.transfers.len();
        let done: u64 = st.transfers.values().map(|t| t.0).sum();
        let total: u64 = st.transfers.values().map(|t| t.1).sum();
        ui.set_transfer_count(count as i32);
        ui.set_transfer_progress(if total == 0 {
            0.0
        } else {
            (done as f32 / total as f32).min(1.0)
        });
        ui.set_transfer_text(
            format!(
                "{count} transfer(s) · {} / {}",
                format_size(done),
                format_size(total)
            )
            .into(),
        );
    }

    /// Sets the status of a tab, if it still exists.
    fn set_status(&self, tab_id: u64, msg: String) {
        if let Some(t) = self.state.borrow_mut().tab_mut(tab_id) {
            t.status = msg;
        }
        self.sync_view();
    }

    /// Status for the active tab, or the window when no tab is open.
    fn notify(&self, msg: String) {
        {
            let mut st = self.state.borrow_mut();
            match st.active_tab_mut() {
                Some(t) => t.status = msg,
                None => st.global_status = msg,
            }
        }
        self.sync_view();
    }

    // ---- Servers ---------------------------------------------------------

    fn open_server_dialog(&self, index: Option<usize>) {
        let profile = {
            let mut st = self.state.borrow_mut();
            let profile = match index {
                Some(i) => match st.config.servers.get(i) {
                    Some(p) => p.clone(),
                    None => return,
                },
                None => ServerProfile {
                    region: "us-east-1".into(),
                    ..Default::default()
                },
            };
            st.editing = index;
            profile
        };
        let ui = self.ui();
        ui.set_server_dialog_title(
            if index.is_some() {
                "Edit server"
            } else {
                "Add server"
            }
            .into(),
        );
        ui.set_form_name(profile.name.into());
        ui.set_form_endpoint(profile.endpoint.into());
        ui.set_form_region(profile.region.into());
        ui.set_form_access_key(profile.access_key.into());
        ui.set_form_secret_key(profile.secret_key.into());
        ui.set_form_bucket(profile.bucket.into());
        ui.set_form_path_style(profile.path_style);
        ui.set_form_message(SharedString::new());
        ui.set_server_dialog_open(true);
    }

    /// Reads and validates the server form.
    fn form_profile(&self) -> Result<ServerProfile, String> {
        let ui = self.ui();
        let p = ServerProfile {
            name: ui.get_form_name().trim().to_string(),
            endpoint: ui.get_form_endpoint().trim().to_string(),
            region: ui.get_form_region().trim().to_string(),
            access_key: ui.get_form_access_key().trim().to_string(),
            secret_key: ui.get_form_secret_key().trim().to_string(),
            path_style: ui.get_form_path_style(),
            bucket: ui.get_form_bucket().trim().to_string(),
        };
        if p.name.is_empty() {
            return Err("Name is required.".into());
        }
        if !p.endpoint.is_empty()
            && !p.endpoint.starts_with("http://")
            && !p.endpoint.starts_with("https://")
        {
            return Err("Endpoint URL must start with http:// or https://".into());
        }
        if p.access_key.is_empty() || p.secret_key.is_empty() {
            return Err("Access key and secret key are required.".into());
        }
        Ok(p)
    }

    fn save_server(&self) {
        let ui = self.ui();
        let profile = match self.form_profile() {
            Ok(p) => p,
            Err(msg) => return ui.set_form_message(msg.into()),
        };
        let saved = {
            let mut st = self.state.borrow_mut();
            let mut config = st.config.clone();
            match st.editing {
                Some(i) if i < config.servers.len() => config.servers[i] = profile,
                _ => config.servers.push(profile),
            }
            // Keep the in-memory list unchanged when the write fails.
            config.save_to(&st.config_path).map(|()| st.config = config)
        };
        match saved {
            Ok(()) => {
                ui.set_server_dialog_open(false);
                self.sync_servers();
            }
            Err(e) => ui.set_form_message(format!("Save failed: {e:#}").into()),
        }
    }

    fn test_server(&self) {
        let ui = self.ui();
        let profile = match self.form_profile() {
            Ok(p) => p,
            Err(msg) => return ui.set_form_message(msg.into()),
        };
        ui.set_form_message("Testing…".into());
        let _guard = self.rt.enter();
        let client = s3::build_client(&profile);
        self.rt.spawn(async move {
            let msg = match s3::test_connection(&client, &profile.bucket).await {
                Ok(m) => m,
                Err(e) => format!("Connection failed: {e:#}"),
            };
            on_ui(move |app| app.ui().set_form_message(msg.into()));
        });
    }

    fn ask_delete_server(&self, index: usize) {
        let name = {
            let mut st = self.state.borrow_mut();
            let Some(p) = st.config.servers.get(index) else {
                return;
            };
            let name = p.name.clone();
            st.pending = Some(Pending::DeleteServer(index));
            name
        };
        let ui = self.ui();
        ui.set_confirm_text(format!("Remove server \"{name}\" from the saved settings?").into());
        ui.set_confirm_open(true);
    }

    fn connect(&self, index: usize) {
        let Some(profile) = self.state.borrow().config.servers.get(index).cloned() else {
            return;
        };
        let client = {
            let _guard = self.rt.enter();
            s3::build_client(&profile)
        };
        let id = {
            let mut st = self.state.borrow_mut();
            st.next_tab_id += 1;
            let id = st.next_tab_id;
            st.tabs.push(Tab {
                id,
                title: profile.name.clone(),
                client,
                bucket: (!profile.bucket.is_empty()).then(|| profile.bucket.clone()),
                prefix: String::new(),
                entries: Vec::new(),
                selected: Vec::new(),
                status: String::new(),
                busy: false,
                generation: 0,
            });
            st.active = Some(st.tabs.len() - 1);
            id
        };
        self.load(id);
    }

    // ---- Tabs and navigation ---------------------------------------------

    fn select_tab(&self, index: usize) {
        {
            let mut st = self.state.borrow_mut();
            if index >= st.tabs.len() {
                return;
            }
            st.active = Some(index);
        }
        self.sync_view();
    }

    fn close_tab(&self, index: usize) {
        {
            let mut st = self.state.borrow_mut();
            if index >= st.tabs.len() {
                return;
            }
            st.tabs.remove(index);
            st.active = match st.active {
                _ if st.tabs.is_empty() => None,
                Some(a) if a > index => Some(a - 1),
                Some(a) => Some(a.min(st.tabs.len() - 1)),
                None => None,
            };
            st.global_status.clear();
        }
        self.sync_view();
    }

    /// Reloads the listing of a tab at its current location.
    fn load(&self, tab_id: u64) {
        let (client, bucket, prefix, generation) = {
            let mut st = self.state.borrow_mut();
            let Some(t) = st.tab_mut(tab_id) else { return };
            t.generation += 1;
            t.busy = true;
            t.status = "Loading…".into();
            (
                t.client.clone(),
                t.bucket.clone(),
                t.prefix.clone(),
                t.generation,
            )
        };
        self.sync_view();
        self.rt.spawn(async move {
            let result = match &bucket {
                None => s3::list_buckets(&client).await,
                Some(b) => s3::list_dir(&client, b, &prefix).await,
            };
            on_ui(move |app| app.loaded(tab_id, generation, result));
        });
    }

    fn loaded(&self, tab_id: u64, generation: u64, result: Result<Vec<Entry>>) {
        {
            let mut st = self.state.borrow_mut();
            let Some(t) = st.tab_mut(tab_id) else { return };
            if t.generation != generation {
                return;
            }
            t.busy = false;
            match result {
                Ok(entries) => {
                    t.status = format!("{} item(s)", entries.len());
                    t.selected = vec![false; entries.len()];
                    t.entries = entries;
                }
                Err(e) => {
                    t.status = format!("Error: {e:#}");
                    t.entries.clear();
                    t.selected.clear();
                }
            }
        }
        self.sync_view();
    }

    /// Reloads a tab only if it is still showing `bucket`/`prefix`.
    fn reload_if_at(&self, tab_id: u64, bucket: &str, prefix: &str) {
        let same = self
            .state
            .borrow_mut()
            .tab_mut(tab_id)
            .is_some_and(|t| t.bucket.as_deref() == Some(bucket) && t.prefix == prefix);
        if same {
            self.load(tab_id);
        }
    }

    fn refresh(&self) {
        let id = self.state.borrow().active_tab().map(|t| t.id);
        if let Some(id) = id {
            self.load(id);
        }
    }

    fn go_up(&self) {
        let id = {
            let mut st = self.state.borrow_mut();
            let Some(t) = st.active_tab_mut() else { return };
            if !t.prefix.is_empty() {
                t.prefix = parent_prefix(&t.prefix);
            } else if t.bucket.is_some() {
                t.bucket = None;
            } else {
                return;
            }
            t.id
        };
        self.load(id);
    }

    fn row_clicked(&self, index: usize, toggle: bool) {
        let selected = {
            let mut st = self.state.borrow_mut();
            let Some(t) = st.active_tab_mut() else { return };
            if index >= t.selected.len() {
                return;
            }
            if toggle {
                t.selected[index] = !t.selected[index];
            } else {
                for (i, sel) in t.selected.iter_mut().enumerate() {
                    *sel = i == index;
                }
            }
            t.selected.clone()
        };
        for (i, sel) in selected.into_iter().enumerate() {
            if let Some(mut row) = self.entries.row_data(i)
                && row.selected != sel
            {
                row.selected = sel;
                self.entries.set_row_data(i, row);
            }
        }
    }

    fn row_activated(&self, index: usize) {
        let id = {
            let mut st = self.state.borrow_mut();
            let Some(t) = st.active_tab_mut() else { return };
            let Some(entry) = t.entries.get(index) else {
                return;
            };
            if !entry.is_dir {
                return;
            }
            if t.bucket.is_none() {
                t.bucket = Some(entry.key.clone());
                t.prefix.clear();
            } else {
                t.prefix = entry.key.clone();
            }
            t.id
        };
        self.load(id);
    }

    /// Client, bucket and prefix of the active tab, when it is inside a bucket.
    fn active_target(&self) -> Option<(u64, Client, String, String)> {
        let st = self.state.borrow();
        let t = st.active_tab()?;
        Some((t.id, t.client.clone(), t.bucket.clone()?, t.prefix.clone()))
    }

    // ---- File operations -------------------------------------------------

    fn upload(&self) {
        let Some((tab_id, client, bucket, prefix)) = self.active_target() else {
            return;
        };
        self.rt.spawn(async move {
            let Some(files) = rfd::AsyncFileDialog::new()
                .set_title("Upload files")
                .pick_files()
                .await
            else {
                return;
            };
            let paths: Vec<PathBuf> = files.iter().map(|f| f.path().to_path_buf()).collect();
            let result = upload_files(&client, &bucket, &prefix, &paths).await;
            on_ui(move |app| {
                app.set_status(
                    tab_id,
                    match &result {
                        Ok(n) => format!("Uploaded {n} file(s)."),
                        Err(e) => format!("Upload failed: {e:#}"),
                    },
                );
                // A failed batch may still have stored some files.
                app.reload_if_at(tab_id, &bucket, &prefix);
            });
        });
    }

    fn download(&self) {
        let Some((tab_id, client, bucket, _)) = self.active_target() else {
            return;
        };
        let items = self
            .state
            .borrow()
            .active_tab()
            .map(Tab::selected_entries)
            .unwrap_or_default();
        if items.is_empty() {
            return self.notify("Select files or folders to download first.".into());
        }
        self.rt.spawn(async move {
            let Some(dir) = rfd::AsyncFileDialog::new()
                .set_title("Download to folder")
                .pick_folder()
                .await
            else {
                return;
            };
            let dest = dir.path().to_path_buf();
            let result = download_items(&client, &bucket, &items, &dest).await;
            on_ui(move |app| {
                app.set_status(
                    tab_id,
                    match result {
                        Ok((n, 0)) => format!("Downloaded {n} file(s) to {}", dest.display()),
                        Ok((n, skipped)) => format!(
                            "Downloaded {n} file(s) to {}, skipped {skipped} with unsafe names.",
                            dest.display()
                        ),
                        Err(e) => format!("Download failed: {e:#}"),
                    },
                );
            });
        });
    }

    fn create_folder(&self, name: &str) {
        let name = name.trim();
        if name.is_empty() || name.contains('/') {
            return self.notify("Folder name must not be empty or contain \"/\".".into());
        }
        let Some((tab_id, client, bucket, prefix)) = self.active_target() else {
            return;
        };
        let key = format!("{prefix}{name}/");
        self.rt.spawn(async move {
            let result = s3::create_folder(&client, &bucket, &key).await;
            on_ui(move |app| match result {
                Ok(()) => app.reload_if_at(tab_id, &bucket, &prefix),
                Err(e) => app.set_status(tab_id, format!("Error: {e:#}")),
            });
        });
    }

    fn ask_delete_selected(&self) {
        let Some((tab_id, _, bucket, prefix)) = self.active_target() else {
            return;
        };
        let items = self
            .state
            .borrow()
            .active_tab()
            .map(Tab::selected_entries)
            .unwrap_or_default();
        if items.is_empty() {
            return self.notify("Select files or folders to delete first.".into());
        }
        let folders = items.iter().filter(|e| e.is_dir).count();
        let mut text = format!(
            "Delete {} item(s) from s3://{bucket}/{prefix}?",
            items.len()
        );
        if folders > 0 {
            text.push_str(&format!(
                "\n{folders} folder(s) will be deleted with everything inside."
            ));
        }
        text.push_str("\nThis cannot be undone.");
        self.state.borrow_mut().pending = Some(Pending::DeleteObjects {
            tab_id,
            bucket,
            items,
        });
        let ui = self.ui();
        ui.set_confirm_text(text.into());
        ui.set_confirm_open(true);
    }

    fn confirm_accepted(&self) {
        let pending = self.state.borrow_mut().pending.take();
        match pending {
            None => {}
            Some(Pending::DeleteServer(index)) => {
                let saved = {
                    let mut st = self.state.borrow_mut();
                    if index >= st.config.servers.len() {
                        return;
                    }
                    let mut config = st.config.clone();
                    config.servers.remove(index);
                    config.save_to(&st.config_path).map(|()| st.config = config)
                };
                match saved {
                    Ok(()) => {
                        self.ui().set_selected_server(-1);
                        self.sync_servers();
                    }
                    Err(e) => self.notify(format!("Save failed: {e:#}")),
                }
            }
            Some(Pending::DeleteObjects {
                tab_id,
                bucket,
                items,
            }) => {
                let (client, prefix) = {
                    let mut st = self.state.borrow_mut();
                    let Some(t) = st.tab_mut(tab_id) else { return };
                    (t.client.clone(), t.prefix.clone())
                };
                self.set_status(tab_id, "Deleting…".into());
                self.rt.spawn(async move {
                    let result = delete_items(&client, &bucket, &items).await;
                    on_ui(move |app| {
                        app.set_status(
                            tab_id,
                            match &result {
                                Ok(n) => format!("Deleted {n} object(s)."),
                                Err(e) => format!("Delete failed: {e:#}"),
                            },
                        );
                        // A failed delete may still have removed some objects.
                        app.reload_if_at(tab_id, &bucket, &prefix);
                    });
                });
            }
        }
    }
}

// ---- Background work -------------------------------------------------------

async fn upload_files(
    client: &Client,
    bucket: &str,
    prefix: &str,
    paths: &[PathBuf],
) -> Result<usize> {
    let mut total = 0;
    for p in paths {
        total += tokio::fs::metadata(p).await.map(|m| m.len()).unwrap_or(0);
    }
    let job = Job::start(total);
    let mut count = 0;
    for p in paths {
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            anyhow::bail!("file name is not valid UTF-8: {}", p.display());
        };
        let key = format!("{prefix}{name}");
        s3::upload_file(client, bucket, &key, p, &|n| job.add(n)).await?;
        count += 1;
    }
    Ok(count)
}

/// Returns (files downloaded, keys skipped because they map to no safe path).
async fn download_items(
    client: &Client,
    bucket: &str,
    items: &[Entry],
    dest: &std::path::Path,
) -> Result<(usize, usize)> {
    // (key, path relative to dest, size)
    let mut files: Vec<(String, String, u64)> = Vec::new();
    for item in items {
        if !item.is_dir {
            files.push((item.key.clone(), item.name.clone(), item.size));
            continue;
        }
        for (key, size) in s3::list_all(client, bucket, &item.key).await? {
            // Folder marker objects have nothing to download.
            if key.ends_with('/') {
                continue;
            }
            let rest = key.strip_prefix(&item.key).unwrap_or(&key);
            files.push((key.clone(), format!("{}/{rest}", item.name), size));
        }
    }
    let job = Job::start(files.iter().map(|f| f.2).sum());
    let (mut count, mut skipped) = (0, 0);
    for (key, rel, _) in &files {
        let Some(path) = safe_join(dest, rel) else {
            skipped += 1;
            continue;
        };
        s3::download_file(client, bucket, key, &path, &|n| job.add(n)).await?;
        count += 1;
    }
    Ok((count, skipped))
}

async fn delete_items(client: &Client, bucket: &str, items: &[Entry]) -> Result<usize> {
    let mut keys = Vec::new();
    for item in items {
        if item.is_dir {
            keys.extend(
                s3::list_all(client, bucket, &item.key)
                    .await?
                    .into_iter()
                    .map(|(k, _)| k),
            );
        } else {
            keys.push(item.key.clone());
        }
    }
    s3::delete_keys(client, bucket, &keys).await?;
    Ok(keys.len())
}

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let ui = MainWindow::new()?;

    let mut state = State::default();
    match config::config_path() {
        Ok(path) => {
            match Config::load_from(&path) {
                Ok(cfg) => state.config = cfg,
                Err(e) => state.global_status = format!("Config error: {e:#}"),
            }
            state.config_path = path;
        }
        Err(e) => state.global_status = format!("Config error: {e:#}"),
    }

    let entries = Rc::new(VecModel::<FileEntry>::default());
    ui.set_entries(ModelRc::from(entries.clone()));

    let app = Rc::new(App {
        ui: ui.as_weak(),
        rt: runtime.handle().clone(),
        state: RefCell::new(state),
        entries,
    });
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    app.sync_servers();
    app.sync_view();

    ui.on_connect_server(|i| with_app(|a| a.connect(i as usize)));
    ui.on_add_server(|| with_app(|a| a.open_server_dialog(None)));
    ui.on_edit_server(|i| with_app(|a| a.open_server_dialog(Some(i as usize))));
    ui.on_delete_server(|i| with_app(|a| a.ask_delete_server(i as usize)));
    ui.on_save_server(|| with_app(|a| a.save_server()));
    ui.on_test_server(|| with_app(|a| a.test_server()));
    ui.on_select_tab(|i| with_app(|a| a.select_tab(i as usize)));
    ui.on_close_tab(|i| with_app(|a| a.close_tab(i as usize)));
    ui.on_go_up(|| with_app(|a| a.go_up()));
    ui.on_refresh(|| with_app(|a| a.refresh()));
    ui.on_upload(|| with_app(|a| a.upload()));
    ui.on_download(|| with_app(|a| a.download()));
    ui.on_delete_selected(|| with_app(|a| a.ask_delete_selected()));
    ui.on_create_folder(|name| with_app(|a| a.create_folder(&name)));
    ui.on_row_clicked(|i, toggle| with_app(|a| a.row_clicked(i as usize, toggle)));
    ui.on_row_activated(|i| with_app(|a| a.row_activated(i as usize)));
    ui.on_confirm_accepted(|| with_app(|a| a.confirm_accepted()));

    ui.run()?;
    APP.with(|a| a.borrow_mut().take());
    Ok(())
}
