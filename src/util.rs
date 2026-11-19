//! Pure helpers.

use std::path::{Component, Path, PathBuf};

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub fn format_time(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// Parent of a folder prefix: `a/b/` -> `a/`, `a/` -> ``.
pub fn parent_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => trimmed[..=i].to_string(),
        None => String::new(),
    }
}

/// Maps an S3-relative key to a path under `base`. Returns `None` when the key
/// has no usable component or would escape `base` (`..`, absolute paths).
pub fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    let mut pushed = false;
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        let mut comps = Path::new(part).components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(name)), None) => {
                out.push(name);
                pushed = true;
            }
            _ => return None,
        }
    }
    pushed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn parents() {
        assert_eq!(parent_prefix(""), "");
        assert_eq!(parent_prefix("a/"), "");
        assert_eq!(parent_prefix("a/b/"), "a/");
        assert_eq!(parent_prefix("a/b/c/"), "a/b/");
    }

    #[test]
    fn safe_paths() {
        let base = Path::new("/tmp/dl");
        assert_eq!(safe_join(base, "a.txt"), Some(base.join("a.txt")));
        assert_eq!(safe_join(base, "d/e/a.txt"), Some(base.join("d/e/a.txt")));
        assert_eq!(safe_join(base, "d//a.txt"), Some(base.join("d/a.txt")));
        assert_eq!(safe_join(base, "../a.txt"), None);
        assert_eq!(safe_join(base, "d/../../a.txt"), None);
        assert_eq!(safe_join(base, ""), None);
        assert_eq!(safe_join(base, "/"), None);
        // A leading slash must not turn the result into an absolute path.
        assert_eq!(safe_join(base, "/etc/passwd"), Some(base.join("etc/passwd")));
    }
}
