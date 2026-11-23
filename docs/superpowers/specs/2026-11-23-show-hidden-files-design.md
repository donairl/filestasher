# Show Hidden Files in File Browser — Design

Show hidden files and folders (names starting with `.`, such as `.env`, `.static`, `.gitignore`) in the local file browser and remote listings, with visual distinction.

## Problem

In `src/main.rs`, `read_local_dir` skips any filesystem entry starting with a dot (`if name.starts_with('.') { continue; }`). Consequently, important configuration and project files like `.env`, `.static`, or dot-directories never appear in the local pane.

## Goal & Scope

1. **Local Listing:** Include dotfiles and dot-directories in `read_local_dir` so they appear in the local pane.
2. **Visual Styling:** Distinguish hidden items visually by dimming them slightly (`opacity: 0.6`) when unselected, restoring full opacity when selected.
3. **Consistency:** Ensure both local pane (`local-entries`) and remote pane (`entries`) convey `is-hidden` through the shared Slint `FileEntry` model.
4. **Testing:** Update unit tests to verify that hidden files are listed and sorted correctly alongside regular entries.

Out of scope: Toggling hidden files via a button or menu setting (per user decision, they are always displayed directly with visual distinction).

## Architecture & Changes

### 1. Slint Model (`ui/app.slint`)
- In `export struct FileEntry`:
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
- In both local list row (`root.local-entries`) and remote list row (`root.entries`):
  Apply opacity to the inner `HorizontalLayout`:
  ```slint
  opacity: entry.is-hidden && !entry.selected ? 0.6 : 1.0;
  ```

### 2. Rust Application Logic (`src/main.rs`)
- In `read_local_dir`:
  - Remove `if name.starts_with('.') { continue; }`.
  - Update doc comment to reflect that hidden files are included.
- In `sync_local`:
  - When constructing `FileEntry`, set `is_hidden: e.name.starts_with('.')`.
- In `file_row`:
  - When constructing `FileEntry` for remote items, set `is_hidden: e.name.starts_with('.')`.

### 3. Testing
- In `local_listing_and_upload_keys` (`src/main.rs`):
  - Update expected listing names. Folders still sort first (`docs`), followed by files sorted by name ascending. Since `.` (ASCII 46) sorts before `A` (ASCII 65), the expected file order is `[".hidden", "A.txt", "b.txt"]`.
  - Verify that `read_local_dir` correctly contains `.hidden` and that uploads continue to function as expected.
