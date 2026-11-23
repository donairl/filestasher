# Show Hidden Files in File Browser Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow hidden files and directories (starting with `.`, like `.env`, `.static`) to be visible in the local file browser and remote file lists, styled with subtle opacity dimming.

**Architecture:** Remove dotfile exclusion from `read_local_dir` in `src/main.rs`. Extend the Slint `FileEntry` struct with an `is-hidden` boolean field and populate it in both local and remote row mappings. Apply an opacity dimming (`0.6`) in `ui/app.slint` when an unselected row is hidden.

**Tech Stack:** Rust, Slint UI.

## Global Constraints

- File names starting with `.` are considered hidden.
- Hidden items must be visually dimmed (`opacity: 0.6`) when not selected, and fully opaque (`opacity: 1.0`) when selected.
- No regression in directory sorting (directories first, then files sorted by selected column).

---

### Task 1: Include Hidden Files in `read_local_dir` (TDD)

**Files:**
- Modify: `src/main.rs:2189-2216` (`read_local_dir`)
- Test: `src/main.rs:2592-2630` (`local_listing_and_upload_keys`)

**Interfaces:**
- Consumes: `std::fs::read_dir`, `LocalEntry`
- Produces: `read_local_dir(dir: &std::path::Path) -> Result<Vec<LocalEntry>>` returning all entries including those starting with `.`

- [ ] **Step 1: Write the failing test**

In `src/main.rs`, update `local_listing_and_upload_keys` to expect `.hidden` in the sorted listing:
```rust
        // Folders first, dotfiles and regular files sorted by name ascending.
        assert_eq!(names, ["docs", ".hidden", "A.txt", "b.txt"]);
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test local_listing_and_upload_keys`
Expected: FAIL because `names` contains `["docs", "A.txt", "b.txt"]` and `.hidden` is currently filtered out.

- [ ] **Step 3: Remove dotfile filtering in `read_local_dir`**

In `src/main.rs`, update `read_local_dir`:
```rust
/// Lists a local folder, unsorted. Follows symlinks; entries that cannot be read
/// are skipped.
fn read_local_dir(dir: &std::path::Path) -> Result<Vec<LocalEntry>> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        // Follows symlinks; entries that cannot be read are skipped.
        let Ok(meta) = std::fs::metadata(item.path()) else {
            continue;
        };
        out.push(LocalEntry {
            name,
            path: item.path(),
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
        });
    }
    Ok(out)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test local_listing_and_upload_keys`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: include hidden files in local directory listing"
```

---

### Task 2: Add `is-hidden` to `FileEntry` and Populate in Rust

**Files:**
- Modify: `ui/app.slint:14-22` (`FileEntry` struct)
- Modify: `src/main.rs:1746-1763` (`sync_local`)
- Modify: `src/main.rs:2139-2152` (`file_row`)

**Interfaces:**
- Consumes: `e.name.starts_with('.')`
- Produces: `FileEntry.is_hidden: bool` (Slint `is-hidden: bool`)

- [ ] **Step 1: Add `is-hidden` field to `FileEntry` in `ui/app.slint`**

In `ui/app.slint`:
```slint
export struct FileEntry {
    name: string,
    is-dir: bool,
    is-image: bool,
    is-hidden: bool,
    size: string,
    modified: string,
    selected: bool,
}
```

- [ ] **Step 2: Update `sync_local` and `file_row` in `src/main.rs`**

In `sync_local`:
```rust
        let rows: Vec<FileEntry> = st
            .local_entries
            .iter()
            .zip(&st.local_selected)
            .map(|(e, sel)| FileEntry {
                name: e.name.as_str().into(),
                is_dir: e.is_dir,
                is_image: !e.is_dir && is_image(&e.name),
                is_hidden: e.name.starts_with('.'),
                size: if e.is_dir {
                    SharedString::new()
                } else {
                    format_size(e.size).into()
                },
                modified: e.modified.map(format_time).unwrap_or_default().into(),
                selected: *sel,
            })
            .collect();
```

In `file_row`:
```rust
fn file_row(e: &Entry, in_bucket: bool, selected: bool) -> FileEntry {
    FileEntry {
        name: e.name.as_str().into(),
        is_dir: e.is_dir,
        is_image: !e.is_dir && in_bucket && is_image(&e.name),
        is_hidden: e.name.starts_with('.'),
        size: if e.is_dir {
            SharedString::new()
        } else {
            format_size(e.size).into()
        },
        modified: e.modified.map(format_time).unwrap_or_default().into(),
        selected,
    }
}
```

- [ ] **Step 3: Run compiler and tests**

Run: `cargo test`
Expected: PASS (all unit tests build and pass)

- [ ] **Step 4: Commit**

```bash
git add ui/app.slint src/main.rs
git commit -m "feat: add is-hidden property to FileEntry"
```

---

### Task 3: Style Hidden Entries in UI with Dimmed Opacity

**Files:**
- Modify: `ui/app.slint:814-935` (remote rows)
- Modify: `ui/app.slint:1126-1235` (local rows)

**Interfaces:**
- Consumes: `entry.is-hidden`, `entry.selected`
- Produces: Visual dimming `opacity: entry.is-hidden && !entry.selected ? 0.6 : 1.0;` on item layout

- [ ] **Step 1: Update row layout opacity in `ui/app.slint`**

In `ui/app.slint`, find the row's inner `HorizontalLayout` in `root.local-entries`:
```slint
                        HorizontalLayout {
                            opacity: entry.is-hidden && !entry.selected ? 0.6 : 1.0;
                            padding-left: 6px;
                            padding-right: 6px;
                            spacing: 8px;
                            // ...
```

In `ui/app.slint`, find the row's inner `HorizontalLayout` in `root.entries` (remote list):
```slint
                    HorizontalLayout {
                        opacity: entry.is-hidden && !entry.selected ? 0.6 : 1.0;
                        padding-left: 8px;
                        padding-right: 8px;
                        spacing: 8px;
                        // ...
```

- [ ] **Step 2: Run build and tests to verify syntax and compilation**

Run: `cargo test`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add ui/app.slint
git commit -m "feat: style hidden files with subtle opacity dimming in file lists"
```
