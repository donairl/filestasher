//! S3 FStasher: an S3 file explorer.
//!
//! All state lives on the UI thread in [`App`]. S3 work runs on a tokio
//! runtime and reports back through [`on_ui`].

// Release builds on Windows must not open a console window next to the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

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
use s3::{Entry, Page};
use util::{format_size, format_time, is_image, parent_prefix, safe_join};

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
    /// Continuation token when the folder has more entries to load.
    next_token: Option<String>,
    /// Search text; only entries whose name contains it are shown.
    filter: String,
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

    /// The search text as compared: trimmed and lower case. Empty means no
    /// search. `filter` itself stays as typed, so the search box is not
    /// rewritten under the cursor.
    fn needle(&self) -> String {
        self.filter.trim().to_lowercase()
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

/// A file or folder in the local pane.
#[derive(Clone, Debug)]
struct LocalEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
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
    /// Bumped per preview; results of older previews are dropped.
    preview_generation: u64,
    /// Order of every tab's listing.
    sort: SortOrder,
    /// Folder shown in the local pane, with its listing and selection.
    local_dir: PathBuf,
    local_entries: Vec<LocalEntry>,
    local_selected: Vec<bool>,
    local_status: String,
    /// Bumped per local listing; older results are dropped.
    local_generation: u64,
    /// For each row on screen, its index in the active tab's entries. Differs
    /// from the identity while a search filter hides entries.
    visible: Vec<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SortColumn {
    Name,
    Size,
    Modified,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SortOrder {
    column: SortColumn,
    descending: bool,
}

impl Default for SortOrder {
    /// Newest first.
    fn default() -> Self {
        SortOrder {
            column: SortColumn::Modified,
            descending: true,
        }
    }
}

impl SortOrder {
    /// Folders always come before files. Ties fall back to the name.
    fn compare(self, a: &Entry, b: &Entry) -> std::cmp::Ordering {
        let by_column = match self.column {
            SortColumn::Name => a.name.cmp(&b.name),
            SortColumn::Size => a.size.cmp(&b.size),
            SortColumn::Modified => a.modified.cmp(&b.modified),
        };
        let by_column = if self.descending {
            by_column.reverse()
        } else {
            by_column
        };
        b.is_dir
            .cmp(&a.is_dir)
            .then(by_column)
            .then_with(|| a.name.cmp(&b.name))
    }

    /// Sorts a listing, keeping each entry's selection flag with it.
    fn apply(self, entries: &mut Vec<Entry>, selected: &mut Vec<bool>) {
        let mut rows: Vec<(Entry, bool)> = std::mem::take(entries)
            .into_iter()
            .zip(std::mem::take(selected))
            .collect();
        rows.sort_by(|a, b| self.compare(&a.0, &b.0));
        (*entries, *selected) = rows.into_iter().unzip();
    }
}

/// Larger images are not downloaded for preview.
const MAX_PREVIEW_BYTES: u64 = 64 * 1024 * 1024;
/// Longest side of the decoded preview, in pixels.
const MAX_PREVIEW_SIDE: u32 = 2048;

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
    local_rows: Rc<VecModel<FileEntry>>,
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

    /// Everything about the active tab except its rows.
    fn sync_status(&self) {
        let st = self.state.borrow();
        let ui = self.ui();
        let titles: Vec<SharedString> = st.tabs.iter().map(|t| t.title.as_str().into()).collect();
        ui.set_tabs(ModelRc::new(VecModel::from(titles)));
        ui.set_active_tab(st.active.map_or(-1, |i| i as i32));
        ui.set_sort_column(match st.sort.column {
            SortColumn::Name => 0,
            SortColumn::Size => 1,
            SortColumn::Modified => 2,
        });
        ui.set_sort_descending(st.sort.descending);
        match st.active_tab() {
            Some(t) => {
                ui.set_location(t.location().into());
                ui.set_status(t.status.as_str().into());
                ui.set_busy(t.busy);
                ui.set_in_bucket(t.bucket.is_some());
                ui.set_has_more(t.next_token.is_some());
                ui.set_search_text(t.filter.as_str().into());
                // An active search is always visible. An empty box stays as
                // the user left it.
                if !t.filter.is_empty() {
                    ui.set_search_open(true);
                }
            }
            None => {
                ui.set_location(SharedString::new());
                ui.set_status(st.global_status.as_str().into());
                ui.set_busy(false);
                ui.set_in_bucket(false);
                ui.set_has_more(false);
                ui.set_search_text(SharedString::new());
                ui.set_search_open(false);
            }
        }
    }

    /// Status and rows. Replaces the row model, so use it only when the
    /// listing itself changed.
    fn sync_view(&self) {
        self.sync_status();
        let (rows, info) = {
            let mut st = self.state.borrow_mut();
            let mut visible = Vec::new();
            let mut rows = Vec::new();
            let mut info = String::new();
            if let Some(t) = st.active_tab() {
                let lowered = t.needle();
                for (i, (e, sel)) in t.entries.iter().zip(&t.selected).enumerate() {
                    if lowered.is_empty() || e.name.to_lowercase().contains(&lowered) {
                        visible.push(i);
                        rows.push(file_row(e, t.bucket.is_some(), *sel));
                    }
                }
                if !lowered.is_empty() {
                    info = format!("{} of {}", rows.len(), t.entries.len());
                    if t.next_token.is_some() {
                        info.push_str(", searching…");
                    }
                }
            }
            st.visible = visible;
            (rows, info)
        };
        self.ui().set_search_info(info.into());
        self.entries.set_vec(rows);
    }

    /// Closes the search box when the active tab has no search. Called when
    /// the tab or the folder changes.
    fn reset_search_box(&self) {
        let searching = self
            .state
            .borrow()
            .active_tab()
            .is_some_and(|t| !t.filter.is_empty());
        self.ui().set_search_open(searching);
    }

    /// Entry index behind a row on screen.
    fn entry_index(&self, row: usize) -> Option<usize> {
        self.state.borrow().visible.get(row).copied()
    }

    /// Search box edited. While a search is active the remaining pages are
    /// loaded in the background, so the whole folder gets searched.
    fn search_changed(&self, text: &str) {
        {
            let mut st = self.state.borrow_mut();
            let Some(t) = st.active_tab_mut() else { return };
            t.filter = text.to_string();
            // Never keep a selection the user can no longer see.
            let needle = t.needle();
            if !needle.is_empty() {
                for (e, sel) in t.entries.iter().zip(t.selected.iter_mut()) {
                    if *sel && !e.name.to_lowercase().contains(&needle) {
                        *sel = false;
                    }
                }
            }
        }
        self.sync_view();
        self.continue_search();
    }

    /// Loads the next page when the active tab has a search and more pages.
    fn continue_search(&self) {
        let searching = self
            .state
            .borrow()
            .active_tab()
            .is_some_and(|t| !t.needle().is_empty() && t.next_token.is_some());
        if searching {
            self.load_more();
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
        self.sync_status();
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
        self.sync_status();
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
                next_token: None,
                filter: String::new(),
                generation: 0,
            });
            st.active = Some(st.tabs.len() - 1);
            id
        };
        self.reset_search_box();
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
        self.reset_search_box();
        self.sync_view();
        self.continue_search();
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
        self.reset_search_box();
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
                None => s3::list_buckets(&client).await.map(|entries| Page {
                    entries,
                    next: None,
                }),
                Some(b) => s3::list_page(&client, b, &prefix, None).await,
            };
            on_ui(move |app| app.loaded(tab_id, generation, result));
        });
    }

    fn loaded(&self, tab_id: u64, generation: u64, result: Result<Page>) {
        {
            let mut st = self.state.borrow_mut();
            let sort = st.sort;
            let Some(t) = st.tab_mut(tab_id) else { return };
            if t.generation != generation {
                return;
            }
            t.busy = false;
            match result {
                Ok(page) => {
                    t.selected = vec![false; page.entries.len()];
                    t.entries = page.entries;
                    sort.apply(&mut t.entries, &mut t.selected);
                    t.next_token = page.next;
                    t.status = count_status(t.entries.len(), t.next_token.is_some());
                }
                Err(e) => {
                    t.status = format!("Error: {e:#}");
                    t.entries.clear();
                    t.selected.clear();
                    t.next_token = None;
                }
            }
        }
        self.sync_view();
        self.continue_search();
    }

    /// Loads the next page of the active tab, if there is one. Called when
    /// the list is scrolled near its end.
    fn load_more(&self) {
        let (tab_id, client, bucket, prefix, token, generation) = {
            let mut st = self.state.borrow_mut();
            let Some(t) = st.active_tab_mut() else { return };
            // `busy` also covers a page that is already on its way.
            if t.busy {
                return;
            }
            let (Some(bucket), Some(token)) = (t.bucket.clone(), t.next_token.clone()) else {
                return;
            };
            t.busy = true;
            t.status = format!("{} item(s) loaded, loading more…", t.entries.len());
            (
                t.id,
                t.client.clone(),
                bucket,
                t.prefix.clone(),
                token,
                t.generation,
            )
        };
        self.sync_status();
        self.rt.spawn(async move {
            let result = s3::list_page(&client, &bucket, &prefix, Some(token)).await;
            on_ui(move |app| app.loaded_more(tab_id, generation, result));
        });
    }

    fn loaded_more(&self, tab_id: u64, generation: u64, result: Result<Page>) {
        let changed = {
            let mut st = self.state.borrow_mut();
            let sort = st.sort;
            let Some(t) = st.tab_mut(tab_id) else { return };
            // The tab navigated or reloaded while the page was loading.
            if t.generation != generation {
                return;
            }
            t.busy = false;
            match result {
                Ok(page) => {
                    t.selected
                        .extend(std::iter::repeat_n(false, page.entries.len()));
                    t.entries.extend(page.entries);
                    // New entries belong anywhere in the sorted order.
                    sort.apply(&mut t.entries, &mut t.selected);
                    t.next_token = page.next;
                    t.status = count_status(t.entries.len(), t.next_token.is_some());
                    true
                }
                Err(e) => {
                    // Keep the token so scrolling again retries.
                    t.status = format!("Error loading more: {e:#}");
                    false
                }
            }
        };
        if changed {
            self.sync_view();
            self.continue_search();
        } else {
            self.sync_status();
        }
    }

    /// Header click: a new column starts in its natural direction, the
    /// current column flips.
    fn sort_by(&self, column: SortColumn) {
        {
            let mut st = self.state.borrow_mut();
            st.sort = if st.sort.column == column {
                SortOrder {
                    column,
                    descending: !st.sort.descending,
                }
            } else {
                SortOrder {
                    column,
                    descending: column != SortColumn::Name,
                }
            };
            let sort = st.sort;
            for t in &mut st.tabs {
                sort.apply(&mut t.entries, &mut t.selected);
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
            t.filter.clear();
            t.id
        };
        self.reset_search_box();
        self.load(id);
    }

    fn row_clicked(&self, row: usize, toggle: bool) {
        let Some(index) = self.entry_index(row) else {
            return;
        };
        let shown: Vec<bool> = {
            let mut st = self.state.borrow_mut();
            let visible = st.visible.clone();
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
            visible.iter().map(|&i| t.selected[i]).collect()
        };
        for (i, sel) in shown.into_iter().enumerate() {
            if let Some(mut row) = self.entries.row_data(i)
                && row.selected != sel
            {
                row.selected = sel;
                self.entries.set_row_data(i, row);
            }
        }
    }

    /// Right click: keeps a multi-selection that contains the row, otherwise
    /// selects just that row.
    fn row_context(&self, row: usize) {
        let Some(index) = self.entry_index(row) else {
            return;
        };
        let already = self
            .state
            .borrow()
            .active_tab()
            .is_some_and(|t| t.selected.get(index) == Some(&true));
        if !already {
            self.row_clicked(row, false);
        }
    }

    /// Copies the selected entries' paths to the clipboard, one per line:
    /// object keys, or `s3://bucket/key` URIs.
    fn copy_path(&self, as_uri: bool) {
        let text = {
            let st = self.state.borrow();
            let Some(t) = st.active_tab() else { return };
            let paths: Vec<String> = t
                .selected_entries()
                .iter()
                .map(|e| match (&t.bucket, as_uri) {
                    (Some(b), true) => format!("s3://{b}/{}", e.key),
                    // In the bucket list the key is the bucket name.
                    (None, true) => format!("s3://{}", e.key),
                    (_, false) => e.key.clone(),
                })
                .collect();
            if paths.is_empty() {
                return;
            }
            paths.join("\n")
        };
        let count = text.lines().count();
        self.ui().invoke_copy_to_clipboard(text.into());
        self.notify(format!("Copied {count} path(s) to the clipboard."));
    }

    /// Opens the preview overlay and loads the image in the background.
    fn preview(&self, client: Client, bucket: String, entry: Entry) {
        if entry.size > MAX_PREVIEW_BYTES {
            return self.notify(format!(
                "{} is too large to preview ({}).",
                entry.name,
                format_size(entry.size)
            ));
        }
        let generation = {
            let mut st = self.state.borrow_mut();
            st.preview_generation += 1;
            st.preview_generation
        };
        let ui = self.ui();
        ui.set_preview_title(entry.name.as_str().into());
        ui.set_preview_info(SharedString::new());
        ui.set_preview_message("Loading…".into());
        ui.set_preview_image(slint::Image::default());
        ui.set_preview_open(true);
        self.rt.spawn(async move {
            let result = load_preview(&client, &bucket, &entry.key).await;
            on_ui(move |app| {
                let ui = app.ui();
                // Closed, or replaced by a newer preview.
                if app.state.borrow().preview_generation != generation || !ui.get_preview_open() {
                    return;
                }
                match result {
                    Ok((pixels, width, height)) => {
                        ui.set_preview_image(slint::Image::from_rgba8(pixels));
                        ui.set_preview_info(
                            format!("{width} × {height} · {}", format_size(entry.size)).into(),
                        );
                        ui.set_preview_message(SharedString::new());
                    }
                    Err(e) => ui.set_preview_message(format!("Cannot preview: {e:#}").into()),
                }
            });
        });
    }

    fn row_activated(&self, row: usize) {
        let Some(index) = self.entry_index(row) else {
            return;
        };
        let id = {
            let mut st = self.state.borrow_mut();
            let Some(t) = st.active_tab_mut() else { return };
            let Some(entry) = t.entries.get(index) else {
                return;
            };
            if !entry.is_dir {
                let target = match &t.bucket {
                    Some(b) if is_image(&entry.name) => {
                        Some((t.client.clone(), b.clone(), entry.clone()))
                    }
                    _ => None,
                };
                drop(st);
                if let Some((client, bucket, entry)) = target {
                    self.preview(client, bucket, entry);
                }
                return;
            }
            if t.bucket.is_none() {
                t.bucket = Some(entry.key.clone());
                t.prefix.clear();
            } else {
                t.prefix = entry.key.clone();
            }
            t.filter.clear();
            t.id
        };
        self.reset_search_box();
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
        if self.active_target().is_none() {
            return;
        }
        self.rt.spawn(async move {
            let Some(files) = rfd::AsyncFileDialog::new()
                .set_title("Upload files")
                .pick_files()
                .await
            else {
                return;
            };
            let paths: Vec<PathBuf> = files.iter().map(|f| f.path().to_path_buf()).collect();
            on_ui(move |app| app.upload_paths(paths));
        });
    }

    fn download(&self) {
        self.download_to(None);
    }

    /// Downloads the selected entries into `dest`, or into a folder the user
    /// picks when `dest` is `None`.
    fn download_to(&self, dest: Option<PathBuf>) {
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
            let dest = match dest {
                Some(d) => d,
                None => {
                    let Some(dir) = rfd::AsyncFileDialog::new()
                        .set_title("Download to folder")
                        .pick_folder()
                        .await
                    else {
                        return;
                    };
                    dir.path().to_path_buf()
                }
            };
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
                // Show the new files when they landed in the local pane's folder.
                if app.state.borrow().local_dir == dest {
                    app.local_load();
                }
            });
        });
    }

    /// Uploads local files and folders into the active tab's folder.
    fn upload_paths(&self, paths: Vec<PathBuf>) {
        let Some((tab_id, client, bucket, prefix)) = self.active_target() else {
            return self.notify("Open a bucket before uploading.".into());
        };
        if paths.is_empty() {
            return;
        }
        self.rt.spawn(async move {
            let result = upload_files(&client, &bucket, &prefix, paths).await;
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

    // ---- Local pane ------------------------------------------------------

    fn sync_local(&self) {
        let st = self.state.borrow();
        let ui = self.ui();
        ui.set_local_path(st.local_dir.display().to_string().into());
        ui.set_local_status(st.local_status.as_str().into());
        let rows: Vec<FileEntry> = st
            .local_entries
            .iter()
            .zip(&st.local_selected)
            .map(|(e, sel)| FileEntry {
                name: e.name.as_str().into(),
                is_dir: e.is_dir,
                is_image: false,
                size: if e.is_dir {
                    SharedString::new()
                } else {
                    format_size(e.size).into()
                },
                modified: SharedString::new(),
                selected: *sel,
            })
            .collect();
        self.local_rows.set_vec(rows);
    }

    /// Reads the local pane's folder in the background.
    fn local_load(&self) {
        let (dir, generation) = {
            let mut st = self.state.borrow_mut();
            st.local_generation += 1;
            (st.local_dir.clone(), st.local_generation)
        };
        self.rt.spawn_blocking(move || {
            let result = read_local_dir(&dir);
            on_ui(move |app| {
                {
                    let mut st = app.state.borrow_mut();
                    if st.local_generation != generation {
                        return;
                    }
                    match result {
                        Ok(entries) => {
                            st.local_status = format!("{} item(s)", entries.len());
                            st.local_selected = vec![false; entries.len()];
                            st.local_entries = entries;
                        }
                        Err(e) => {
                            st.local_status = format!("Error: {e:#}");
                            st.local_entries.clear();
                            st.local_selected.clear();
                        }
                    }
                }
                app.sync_local();
            });
        });
    }

    fn local_open_dir(&self, dir: PathBuf) {
        self.state.borrow_mut().local_dir = dir;
        self.local_load();
    }

    fn local_up(&self) {
        let parent = self.state.borrow().local_dir.parent().map(PathBuf::from);
        if let Some(parent) = parent {
            self.local_open_dir(parent);
        }
    }

    fn local_choose(&self) {
        self.rt.spawn(async move {
            let Some(dir) = rfd::AsyncFileDialog::new()
                .set_title("Local folder")
                .pick_folder()
                .await
            else {
                return;
            };
            let dir = dir.path().to_path_buf();
            on_ui(move |app| app.local_open_dir(dir));
        });
    }

    fn local_row_clicked(&self, index: usize, toggle: bool) {
        let selected = {
            let mut st = self.state.borrow_mut();
            if index >= st.local_selected.len() {
                return;
            }
            if toggle {
                st.local_selected[index] = !st.local_selected[index];
            } else {
                for (i, sel) in st.local_selected.iter_mut().enumerate() {
                    *sel = i == index;
                }
            }
            st.local_selected.clone()
        };
        for (i, sel) in selected.into_iter().enumerate() {
            if let Some(mut row) = self.local_rows.row_data(i)
                && row.selected != sel
            {
                row.selected = sel;
                self.local_rows.set_row_data(i, row);
            }
        }
    }

    /// Press without Ctrl: keeps a selection that contains the row.
    fn local_row_context(&self, index: usize) {
        let already = self.state.borrow().local_selected.get(index) == Some(&true);
        if !already {
            self.local_row_clicked(index, false);
        }
    }

    fn local_row_activated(&self, index: usize) {
        let dir = {
            let st = self.state.borrow();
            match st.local_entries.get(index) {
                Some(e) if e.is_dir => e.path.clone(),
                _ => return,
            }
        };
        self.local_open_dir(dir);
    }

    // ---- Drag and drop ---------------------------------------------------

    /// Label that follows the pointer during a drag.
    fn drag_started(&self, from_local: bool) {
        let names: Vec<String> = {
            let st = self.state.borrow();
            if from_local {
                st.local_entries
                    .iter()
                    .zip(&st.local_selected)
                    .filter(|(_, sel)| **sel)
                    .map(|(e, _)| e.name.clone())
                    .collect()
            } else {
                st.active_tab()
                    .map(Tab::selected_entries)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| e.name)
                    .collect()
            }
        };
        let label = match names.as_slice() {
            [one] => one.clone(),
            many => format!("{} items", many.len()),
        };
        self.ui().set_drag_label(label.into());
    }

    /// Local rows dropped on the S3 list upload; S3 rows dropped on the local
    /// pane download into its folder.
    fn dropped(&self, from_local: bool) {
        if from_local {
            let paths: Vec<PathBuf> = {
                let st = self.state.borrow();
                st.local_entries
                    .iter()
                    .zip(&st.local_selected)
                    .filter(|(_, sel)| **sel)
                    .map(|(e, _)| e.path.clone())
                    .collect()
            };
            self.upload_paths(paths);
        } else {
            let dest = self.state.borrow().local_dir.clone();
            self.download_to(Some(dest));
        }
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

fn file_row(e: &Entry, in_bucket: bool, selected: bool) -> FileEntry {
    FileEntry {
        name: e.name.as_str().into(),
        is_dir: e.is_dir,
        is_image: !e.is_dir && in_bucket && is_image(&e.name),
        size: if e.is_dir {
            SharedString::new()
        } else {
            format_size(e.size).into()
        },
        modified: e.modified.map(format_time).unwrap_or_default().into(),
        selected,
    }
}

fn count_status(count: usize, has_more: bool) -> String {
    if has_more {
        format!("{count} item(s) loaded, scroll down for more")
    } else {
        format!("{count} item(s)")
    }
}

// ---- Background work -------------------------------------------------------

/// Lists a local folder: folders first, then by name. Hidden entries (names
/// starting with a dot) are left out.
fn read_local_dir(dir: &std::path::Path) -> Result<Vec<LocalEntry>> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        // Follows symlinks; entries that cannot be read are skipped.
        let Ok(meta) = std::fs::metadata(item.path()) else {
            continue;
        };
        out.push(LocalEntry {
            name,
            path: item.path(),
            is_dir: meta.is_dir(),
            size: meta.len(),
        });
    }
    out.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase()));
    Ok(out)
}

/// Expands files and folders into (local path, object key, size). Folders are
/// walked recursively; symlinked folders are not followed.
fn collect_uploads(paths: &[PathBuf], prefix: &str) -> Result<Vec<(PathBuf, String, u64)>> {
    fn walk(
        path: &std::path::Path,
        key: String,
        out: &mut Vec<(PathBuf, String, u64)>,
    ) -> Result<()> {
        if std::fs::symlink_metadata(path)?.is_dir() {
            for item in std::fs::read_dir(path)? {
                let item = item?;
                let name = item.file_name();
                let Some(name) = name.to_str() else {
                    anyhow::bail!("file name is not valid UTF-8: {}", item.path().display());
                };
                walk(&item.path(), format!("{key}/{name}"), out)?;
            }
        } else if let Ok(meta) = std::fs::metadata(path)
            && meta.is_file()
        {
            out.push((path.to_path_buf(), key, meta.len()));
        }
        Ok(())
    }

    let mut out = Vec::new();
    for p in paths {
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            anyhow::bail!("file name is not valid UTF-8: {}", p.display());
        };
        walk(p, format!("{prefix}{name}"), &mut out)?;
    }
    Ok(out)
}

/// Uploads files and whole folders under `prefix`. Returns the file count.
async fn upload_files(
    client: &Client,
    bucket: &str,
    prefix: &str,
    paths: Vec<PathBuf>,
) -> Result<usize> {
    let prefix_owned = prefix.to_string();
    let files =
        tokio::task::spawn_blocking(move || collect_uploads(&paths, &prefix_owned)).await??;
    let job = Job::start(files.iter().map(|f| f.2).sum());
    for (path, key, _) in &files {
        s3::upload_file(client, bucket, key, path, &|n| job.add(n)).await?;
    }
    Ok(files.len())
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

/// Downloads and decodes an image. Returns the pixels and the original size;
/// large images are scaled down to [`MAX_PREVIEW_SIDE`].
async fn load_preview(
    client: &Client,
    bucket: &str,
    key: &str,
) -> Result<(slint::SharedPixelBuffer<slint::Rgba8Pixel>, u32, u32)> {
    let bytes = s3::get_bytes(client, bucket, key).await?;
    tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&bytes)?;
        let (width, height) = (img.width(), img.height());
        let img = if width.max(height) > MAX_PREVIEW_SIDE {
            img.thumbnail(MAX_PREVIEW_SIDE, MAX_PREVIEW_SIDE)
        } else {
            img
        };
        let rgba = img.to_rgba8();
        let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
        );
        Ok((pixels, width, height))
    })
    .await?
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
    let local_rows = Rc::new(VecModel::<FileEntry>::default());
    ui.set_local_entries(ModelRc::from(local_rows.clone()));
    state.local_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

    let app = Rc::new(App {
        ui: ui.as_weak(),
        rt: runtime.handle().clone(),
        state: RefCell::new(state),
        entries,
        local_rows,
    });
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    app.sync_servers();
    app.sync_view();
    app.local_load();

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
    ui.on_load_more(|| with_app(|a| a.load_more()));
    ui.on_search_changed(|text| with_app(|a| a.search_changed(&text)));
    ui.on_sort_by(|column| {
        let column = match column {
            0 => SortColumn::Name,
            1 => SortColumn::Size,
            _ => SortColumn::Modified,
        };
        with_app(|a| a.sort_by(column));
    });
    ui.on_upload(|| with_app(|a| a.upload()));
    ui.on_download(|| with_app(|a| a.download()));
    ui.on_delete_selected(|| with_app(|a| a.ask_delete_selected()));
    ui.on_create_folder(|name| with_app(|a| a.create_folder(&name)));
    ui.on_row_clicked(|i, toggle| with_app(|a| a.row_clicked(i as usize, toggle)));
    ui.on_row_activated(|i| with_app(|a| a.row_activated(i as usize)));
    ui.on_copy_path(|as_uri| with_app(|a| a.copy_path(as_uri)));
    ui.on_row_context(|i| with_app(|a| a.row_context(i as usize)));
    ui.on_local_up(|| with_app(|a| a.local_up()));
    ui.on_local_refresh(|| with_app(|a| a.local_load()));
    ui.on_local_choose(|| with_app(|a| a.local_choose()));
    ui.on_local_row_clicked(|i, toggle| with_app(|a| a.local_row_clicked(i as usize, toggle)));
    ui.on_local_row_context(|i| with_app(|a| a.local_row_context(i as usize)));
    ui.on_local_row_activated(|i| with_app(|a| a.local_row_activated(i as usize)));
    ui.on_drag_started(|from_local| with_app(|a| a.drag_started(from_local)));
    ui.on_dropped(|from_local| with_app(|a| a.dropped(from_local)));
    ui.on_confirm_accepted(|| with_app(|a| a.confirm_accepted()));

    ui.run()?;
    APP.with(|a| a.borrow_mut().take());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool, size: u64, modified: Option<i64>) -> Entry {
        Entry {
            name: name.into(),
            key: name.into(),
            is_dir,
            size,
            modified,
        }
    }

    fn sorted(column: SortColumn, descending: bool) -> (Vec<String>, Vec<bool>) {
        let mut entries = vec![
            entry("b.txt", false, 30, Some(100)),
            entry("zdir", true, 0, None),
            entry("a.txt", false, 10, Some(300)),
            entry("c.txt", false, 20, Some(200)),
            entry("adir", true, 0, None),
        ];
        // Only "a.txt" is selected.
        let mut selected = vec![false, false, true, false, false];
        SortOrder { column, descending }.apply(&mut entries, &mut selected);
        (entries.into_iter().map(|e| e.name).collect(), selected)
    }

    #[test]
    fn local_listing_and_upload_keys() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("docs/sub")).unwrap();
        std::fs::write(root.join("b.txt"), b"12345").unwrap();
        std::fs::write(root.join("A.txt"), b"1").unwrap();
        std::fs::write(root.join(".hidden"), b"x").unwrap();
        std::fs::write(root.join("docs/one.md"), b"12").unwrap();
        std::fs::write(root.join("docs/sub/two.md"), b"123").unwrap();

        let listing = read_local_dir(root).unwrap();
        let names: Vec<&str> = listing.iter().map(|e| e.name.as_str()).collect();
        // Folders first, case-insensitive order, hidden files left out.
        assert_eq!(names, ["docs", "A.txt", "b.txt"]);
        assert!(listing[0].is_dir);
        assert_eq!(listing[2].size, 5);

        let mut uploads =
            collect_uploads(&[root.join("docs"), root.join("b.txt")], "base/").unwrap();
        uploads.sort_by(|a, b| a.1.cmp(&b.1));
        let keys: Vec<(&str, u64)> = uploads.iter().map(|u| (u.1.as_str(), u.2)).collect();
        assert_eq!(
            keys,
            [
                ("base/b.txt", 5),
                ("base/docs/one.md", 2),
                ("base/docs/sub/two.md", 3)
            ]
        );
    }

    #[test]
    fn default_is_newest_first() {
        let order = SortOrder::default();
        let (names, selected) = sorted(order.column, order.descending);
        assert_eq!(names, ["adir", "zdir", "a.txt", "c.txt", "b.txt"]);
        // The selection flag moved with its entry.
        assert_eq!(selected, [false, false, true, false, false]);
    }

    #[test]
    fn sort_columns() {
        let names = |c, d| sorted(c, d).0;
        assert_eq!(
            names(SortColumn::Modified, false),
            ["adir", "zdir", "b.txt", "c.txt", "a.txt"]
        );
        assert_eq!(
            names(SortColumn::Name, false),
            ["adir", "zdir", "a.txt", "b.txt", "c.txt"]
        );
        assert_eq!(
            names(SortColumn::Name, true),
            ["zdir", "adir", "c.txt", "b.txt", "a.txt"]
        );
        assert_eq!(
            names(SortColumn::Size, true),
            ["adir", "zdir", "b.txt", "c.txt", "a.txt"]
        );
    }
}
