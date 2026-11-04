//! Disk cache for message images: `~/Library/Caches/dev.snak.app/images`.
//!
//! Images are keyed by a hash of their URL. A size cap evicts the oldest
//! files when the cache grows past it.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

const MAX_BYTES: u64 = 200 * 1024 * 1024;

fn cache_dir() -> Option<PathBuf> {
    let home = std::env::home_dir()?;
    Some(home.join("Library/Caches/dev.snak.app/images"))
}

fn key(url: &str) -> String {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn path_for(url: &str) -> Option<PathBuf> {
    Some(cache_dir()?.join(format!("{}.bin", key(url))))
}

pub fn read(url: &str) -> Option<Vec<u8>> {
    std::fs::read(path_for(url)?).ok()
}

pub fn write(url: &str, bytes: &[u8]) {
    let Some(path) = path_for(url) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, bytes);
}

pub fn evict() {
    let Some(dir) = cache_dir() else {
        return;
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let accessed = meta
                .accessed()
                .or_else(|_| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            Some((e.path(), meta.len(), accessed))
        })
        .collect();
    let total: u64 = files.iter().map(|(_, size, _)| size).sum();
    if total <= MAX_BYTES {
        return;
    }
    files.sort_by_key(|(_, _, time)| *time);
    let mut freed: u64 = 0;
    let target = total - MAX_BYTES;
    for (path, size, _) in &files {
        if freed >= target {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            freed += size;
        }
    }
    tracing::debug!(freed_mb = freed / (1024 * 1024), "image cache eviction");
}
