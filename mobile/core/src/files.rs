//! Browsing the roost's filesystem: directory listings and file contents
//! over the SSH session's SFTP subsystem.
//!
//! SFTP is used rather than `ls` and `cat` because it reports file types and
//! sizes without parsing, copes with any file name, and is binary safe. The
//! session is opened lazily on first use and dropped with the SSH connection.

use serde::Serialize;

/// How many leading bytes [`is_binary`] inspects.
const SNIFF: usize = 8192;

/// One entry of a directory listing.
#[derive(Debug, Clone, Serialize)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
    /// `true` when the entry is a symlink; `kind` then describes its target.
    pub symlink: bool,
    pub size: u64,
    /// Modification time as seconds since the Unix epoch, when known.
    pub modified: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Dir,
    File,
    Other,
}

/// A directory and its entries, directories first, names sorted ignoring case.
#[derive(Debug, Serialize)]
pub struct DirListing {
    /// The absolute, canonical path that was listed.
    pub path: String,
    pub entries: Vec<DirEntry>,
}

/// The first `max_bytes` of a file.
#[derive(Debug, Serialize)]
pub struct FileContent {
    pub path: String,
    /// Size on disk, which exceeds the bytes returned when `truncated`.
    pub size: u64,
    pub truncated: bool,
    /// The bytes look like they are not text; `text` is then empty.
    pub binary: bool,
    /// The content, decoded as UTF-8 with replacement characters.
    pub text: String,
}

/// A NUL byte in the leading bytes is the usual heuristic for "not text".
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(SNIFF)].contains(&0)
}

/// Directories first, then by name ignoring case.
pub fn sort_entries(entries: &mut [DirEntry]) {
    entries.sort_by(|a, b| {
        (a.kind != EntryKind::Dir)
            .cmp(&(b.kind != EntryKind::Dir))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Expand a leading `~` to `home`. Other paths are returned as they are.
pub fn expand_home(path: &str, home: &str) -> String {
    if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{rest}", home.trim_end_matches('/'))
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind) -> DirEntry {
        DirEntry {
            name: name.into(),
            kind,
            symlink: false,
            size: 0,
            modified: None,
        }
    }

    #[test]
    fn binary_sniff() {
        assert!(!is_binary(b"hello\nworld\n"));
        assert!(!is_binary(b""));
        assert!(is_binary(b"\x7fELF\0\0"));
        assert!(!is_binary("héllo".as_bytes()));
    }

    #[test]
    fn sorting() {
        let mut entries = vec![
            entry("zeta.rs", EntryKind::File),
            entry("Alpha.md", EntryKind::File),
            entry("src", EntryKind::Dir),
            entry(".git", EntryKind::Dir),
            entry("beta", EntryKind::Other),
        ];
        sort_entries(&mut entries);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, [".git", "src", "Alpha.md", "beta", "zeta.rs"]);
    }

    #[test]
    fn home_expansion() {
        assert_eq!(expand_home("~", "/Users/b5"), "/Users/b5");
        assert_eq!(expand_home("~/code", "/Users/b5/"), "/Users/b5/code");
        assert_eq!(expand_home("/tmp", "/Users/b5"), "/tmp");
        assert_eq!(expand_home("~x", "/Users/b5"), "~x");
    }

    #[test]
    fn json_shape() {
        let listing = DirListing {
            path: "/p".into(),
            entries: vec![entry("a", EntryKind::Dir)],
        };
        let v: serde_json::Value = serde_json::to_value(&listing).unwrap();
        assert_eq!(v["entries"][0]["kind"], "dir");
        assert_eq!(v["entries"][0]["modified"], serde_json::Value::Null);
    }
}
