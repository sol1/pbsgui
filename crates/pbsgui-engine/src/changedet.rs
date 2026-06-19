//! Change detection: fingerprint a job's sources by file size + mtime so a run
//! can be skipped when nothing changed since the last successful backup.

use std::path::{Path, PathBuf};

use glob::Pattern;
use sha2::{Digest, Sha256};

use crate::archive::excluded;
use crate::config::config_dir;

/// SHA-256 over the sorted (path, size, mtime) of every source file.
pub fn fingerprint(sources: &[String], excludes: &[String]) -> anyhow::Result<[u8; 32]> {
    let patterns: Vec<Pattern> = excludes
        .iter()
        .filter_map(|e| Pattern::new(e).ok())
        .collect();

    let mut items: Vec<(String, u64, i64)> = Vec::new();
    for source in sources {
        let root = Path::new(source);
        if root.is_dir() {
            for entry in walkdir::WalkDir::new(root).follow_links(false) {
                let entry = entry?;
                let path = entry.path();
                if excluded(path, &patterns) {
                    continue;
                }
                if entry.file_type().is_file() {
                    let md = entry.metadata()?;
                    items.push((
                        path.to_string_lossy().replace('\\', "/"),
                        md.len(),
                        mtime_secs(&md),
                    ));
                }
            }
        } else if root.is_file() && !excluded(root, &patterns) {
            let md = std::fs::metadata(root)?;
            items.push((source.clone(), md.len(), mtime_secs(&md)));
        }
    }
    items.sort();

    let mut hasher = Sha256::new();
    for (path, size, mtime) in &items {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(size.to_le_bytes());
        hasher.update(mtime.to_le_bytes());
    }
    Ok(hasher.finalize().into())
}

/// The last successful fingerprint for a job, if recorded.
pub fn load(job_id: &str) -> Option<[u8; 32]> {
    let bytes = std::fs::read(state_path(job_id)).ok()?;
    bytes.as_slice().try_into().ok()
}

/// Record the fingerprint of a successful run.
pub fn save(job_id: &str, fingerprint: &[u8; 32]) -> anyhow::Result<()> {
    let path = state_path(job_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, fingerprint)?;
    Ok(())
}

fn state_path(job_id: &str) -> PathBuf {
    config_dir().join("state").join(format!("{job_id}.fp"))
}

fn mtime_secs(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
