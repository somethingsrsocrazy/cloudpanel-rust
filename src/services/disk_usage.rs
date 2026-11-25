//! How much disk a site's home takes up.

use crate::fsutil::{identity, Confined};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A site can hold any number of files, so a count stops here and says it is incomplete.
const MAX_ENTRIES: u64 = 2_000_000;
const MAX_TIME: Duration = Duration::from_secs(30);

#[derive(Serialize, Debug, PartialEq)]
pub struct DiskUsage {
    /// Space the files and folders occupy, as `du` counts it.
    pub bytes: u64,
    /// Everything that isn't a folder.
    pub files: u64,
    /// False if a limit was reached or something couldn't be read, so the real total is higher.
    pub complete: bool,
}

#[cfg(unix)]
fn on_disk(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn on_disk(meta: &fs::Metadata) -> u64 {
    meta.len()
}

#[cfg(unix)]
fn hard_linked(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1
}

#[cfg(not(unix))]
fn hard_linked(_meta: &fs::Metadata) -> bool {
    false
}

/// Add up everything under `root`. The tree belongs to a site user while the panel runs as root,
/// so links are counted as themselves and never followed, and each folder is opened confined.
pub fn measure(root: &Path) -> std::io::Result<DiskUsage> {
    measure_up_to(root, MAX_ENTRIES)
}

fn measure_up_to(root: &Path, max_entries: u64) -> std::io::Result<DiskUsage> {
    let tree = Confined::open(root)?;
    let started = Instant::now();
    let mut usage = DiskUsage {
        bytes: on_disk(&fs::metadata(tree.canonical())?),
        files: 0,
        complete: true,
    };
    let mut seen_links = HashSet::new();
    let mut entries = 0;
    let mut folders = vec![PathBuf::new()];
    while let Some(rel) = folders.pop() {
        // A folder can vanish or be swapped while we walk: that makes the total partial, not an error.
        let Ok(held) = tree.pin_dir(&rel) else {
            usage.complete = false;
            continue;
        };
        let Ok(dir) = fs::read_dir(held.path()) else {
            usage.complete = false;
            continue;
        };
        for entry in dir {
            if entries >= max_entries || started.elapsed() > MAX_TIME {
                usage.complete = false;
                return Ok(usage);
            }
            entries += 1;
            // `DirEntry::metadata` describes a link itself, not what it points at.
            let Ok((name, meta)) = entry.and_then(|e| Ok((e.file_name(), e.metadata()?))) else {
                usage.complete = false;
                continue;
            };
            if meta.is_dir() {
                folders.push(rel.join(name));
            } else {
                usage.files += 1;
                if hard_linked(&meta) && !seen_links.insert(identity(&meta)) {
                    continue;
                }
            }
            usage.bytes += on_disk(&meta);
        }
    }
    Ok(usage)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn counts_nested_files_once() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/big"), vec![1u8; 64 * 1024]).unwrap();
        let one = measure(dir.path()).unwrap();
        assert!(one.complete);
        assert_eq!(one.files, 1);
        assert!(one.bytes >= 64 * 1024);

        // A second name for the same file takes no more space.
        fs::hard_link(dir.path().join("a/b/big"), dir.path().join("a/again")).unwrap();
        let two = measure(dir.path()).unwrap();
        assert_eq!(two.files, 2);
        assert!(two.bytes < one.bytes + 64 * 1024);
    }

    #[test]
    fn does_not_follow_links() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), vec![1u8; 1024 * 1024]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
        let usage = measure(dir.path()).unwrap();
        assert_eq!(usage.files, 2);
        assert!(usage.complete);
        assert!(usage.bytes < 1024 * 1024);
    }

    #[test]
    fn says_when_it_stops_early() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            fs::write(dir.path().join(i.to_string()), "x").unwrap();
        }
        let usage = measure_up_to(dir.path(), 3).unwrap();
        assert_eq!(usage.files, 3);
        assert!(!usage.complete);
    }
}
