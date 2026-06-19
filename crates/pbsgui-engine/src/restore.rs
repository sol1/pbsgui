//! Restore: list and extract the tar archive recovered from a snapshot.
//!
//! A backup is a tar streamed into a dynamic-index archive. To browse or restore
//! we download and reassemble that tar (via the reader protocol) and then read or
//! extract its entries. Selective extraction still downloads the whole archive
//! for now; fetching only the needed chunks via a catalog is a future optimization.

use std::collections::HashSet;
use std::io::Cursor;
use std::path::Path;

use pbsgui_ipc::FileInfo;

/// List the files inside a tar archive.
pub fn list_tar(bytes: &[u8]) -> anyhow::Result<Vec<FileInfo>> {
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    let mut files = Vec::new();
    for entry in archive.entries()? {
        let entry = entry?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().replace('\\', "/");
        let size = entry.header().size().unwrap_or(0);
        files.push(FileInfo { path, size });
    }
    Ok(files)
}

/// Extract a tar archive to `dest`. `selected` `None` extracts everything;
/// otherwise only entries whose path is in the set. Returns the number extracted.
pub fn extract(
    bytes: &[u8],
    selected: Option<&HashSet<String>>,
    dest: &Path,
) -> anyhow::Result<usize> {
    std::fs::create_dir_all(dest)?;
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    let mut count = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if let Some(set) = selected {
            let path = entry.path()?.to_string_lossy().replace('\\', "/");
            if !set.contains(&path) {
                continue;
            }
        }
        // unpack_in prevents path traversal outside dest, returning false if unsafe.
        if entry.unpack_in(dest)? {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tar() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in [
            ("a/one.txt", b"hello".as_slice()),
            ("a/two.txt", b"world!!"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn lists_entries() {
        let files = list_tar(&sample_tar()).unwrap();
        let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"a/one.txt"));
        assert!(paths.contains(&"a/two.txt"));
        assert_eq!(
            files.iter().find(|f| f.path == "a/one.txt").unwrap().size,
            5
        );
    }

    #[test]
    fn extracts_selected_only() {
        let dir = std::env::temp_dir().join(format!("pbsgui-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut set = HashSet::new();
        set.insert("a/two.txt".to_string());
        let count = extract(&sample_tar(), Some(&set), &dir).unwrap();
        assert_eq!(count, 1);
        assert!(dir.join("a/two.txt").exists());
        assert!(!dir.join("a/one.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_all() {
        let dir = std::env::temp_dir().join(format!("pbsgui-restore-all-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let count = extract(&sample_tar(), None, &dir).unwrap();
        assert_eq!(count, 2);
        assert_eq!(std::fs::read(dir.join("a/one.txt")).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
