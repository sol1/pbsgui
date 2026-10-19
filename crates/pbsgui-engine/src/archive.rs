//! Build a tar archive of a job's sources.
//!
//! Folders are walked recursively; files are added with a path relative to the
//! source's parent (so the top folder name is preserved), using forward slashes.
//! Glob excludes are applied to each path. This is a single sequential stream
//! that the dedup backup then chunks and uploads.
//!
//! Each source can be read from somewhere other than its live path (a VSS
//! shadow copy), while archive names and excludes still use the live path, so a
//! snapshot run and a live run produce the same archive.
//!
//! A file that cannot be read (locked, permission denied, a cloud-only
//! placeholder) is skipped and reported instead of failing the whole backup.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;

use glob::Pattern;

/// A file or folder left out of the archive, and why.
#[derive(Debug, Clone)]
pub struct Skipped {
    /// The live path.
    pub path: String,
    pub reason: String,
}

/// What [`write_tar`] archived and what it had to skip.
#[derive(Debug, Default)]
pub struct ArchiveReport {
    /// (archive path, size) of each file added, for the snapshot catalog.
    pub entries: Vec<(String, u64)>,
    pub skipped: Vec<Skipped>,
    /// Run-level problems (no VSS snapshot, a failed writer), for the log.
    pub warnings: Vec<String>,
}

/// Write a tar of `sources` (minus `excludes`) to `out`. `read_root` maps a
/// source's live path to the path to read it from (`None` = read it live).
/// Unreadable files and folders are skipped and listed in the report; only a
/// failure writing the archive itself is an error.
pub fn write_tar<W: Write>(
    sources: &[String],
    excludes: &[String],
    read_root: &dyn Fn(&Path) -> Option<PathBuf>,
    out: W,
) -> anyhow::Result<ArchiveReport> {
    let patterns: Vec<Pattern> = excludes
        .iter()
        .filter_map(|e| Pattern::new(e).ok())
        .collect();
    let mut builder = tar::Builder::new(out);
    let mut report = ArchiveReport::default();

    for source in sources {
        let live_root = Path::new(source);
        let read = read_root(live_root).unwrap_or_else(|| live_root.to_path_buf());
        let meta = match std::fs::metadata(&read) {
            Ok(m) => m,
            Err(e) => {
                report.skip(live_root, format!("source not readable: {e}"));
                continue;
            }
        };
        if meta.is_dir() {
            // Sort for a deterministic tar across runs, so unchanged files
            // produce identical chunks and dedup actually reuses them.
            for entry in walkdir::WalkDir::new(&read)
                .follow_links(false)
                .sort_by_file_name()
            {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        // An unreadable folder: walkdir skips its contents.
                        let live = e
                            .path()
                            .map(|p| to_live(live_root, &read, p))
                            .unwrap_or_else(|| live_root.to_path_buf());
                        report.skip(&live, io_reason(&e));
                        continue;
                    }
                };
                let live = to_live(live_root, &read, entry.path());
                if excluded(&live, &patterns) {
                    continue;
                }
                if entry.file_type().is_file() {
                    let name = archive_name(live_root, &live);
                    add_file(&mut builder, &mut report, entry.path(), &live, name)?;
                }
            }
        } else if meta.is_file() && !excluded(live_root, &patterns) {
            let name = live_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            add_file(&mut builder, &mut report, &read, live_root, name)?;
        }
    }

    builder.into_inner()?.flush()?;
    Ok(report)
}

impl ArchiveReport {
    fn skip(&mut self, live: &Path, reason: String) {
        self.skipped.push(Skipped {
            path: live.display().to_string(),
            reason,
        });
    }
}

/// Append one file read from `read`, or record why it was skipped. Opening the
/// file first means a locked or denied file is skipped before any tar header is
/// written; an error after that (the archive writer failing) is fatal.
fn add_file<W: Write>(
    builder: &mut tar::Builder<W>,
    report: &mut ArchiveReport,
    read: &Path,
    live: &Path,
    name: String,
) -> anyhow::Result<()> {
    if let Some(reason) = cloud_only(read) {
        report.skip(live, reason.to_string());
        return Ok(());
    }
    let mut file = match std::fs::File::open(read) {
        Ok(f) => f,
        Err(e) => {
            report.skip(live, e.to_string());
            return Ok(());
        }
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    builder.append_file(&name, &mut file)?;
    report.entries.push((name, size));
    Ok(())
}

/// A OneDrive-style cloud placeholder whose data is not on disk. Reading it
/// would either start a download or fail, so it is skipped with a clear reason.
#[cfg(windows)]
fn cloud_only(path: &Path) -> Option<&'static str> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    let attrs = std::fs::symlink_metadata(path).ok()?.file_attributes();
    let cloud = FILE_ATTRIBUTE_OFFLINE
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
    (attrs & cloud != 0).then_some("cloud-only file (not downloaded to this machine)")
}

#[cfg(not(windows))]
fn cloud_only(_path: &Path) -> Option<&'static str> {
    None
}

/// A walkdir error as a short reason, preferring the underlying I/O error.
fn io_reason(e: &walkdir::Error) -> String {
    match e.io_error() {
        Some(io) => io.to_string(),
        None => e.to_string(),
    }
}

/// The live path for `path` found while walking `read` (the read location of
/// `live_root`).
fn to_live(live_root: &Path, read: &Path, path: &Path) -> PathBuf {
    match path.strip_prefix(read) {
        Ok(rel) if rel.as_os_str().is_empty() => live_root.to_path_buf(),
        Ok(rel) => live_root.join(rel),
        Err(_) => path.to_path_buf(),
    }
}

/// The total size of the files `write_tar` would archive, from metadata only,
/// for progress. Unreadable paths count as zero.
pub fn estimate_size(sources: &[String], excludes: &[String]) -> u64 {
    let patterns: Vec<Pattern> = excludes
        .iter()
        .filter_map(|e| Pattern::new(e).ok())
        .collect();
    let mut total = 0u64;
    for source in sources {
        for entry in walkdir::WalkDir::new(source)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() && !excluded(entry.path(), &patterns) {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

pub(crate) fn excluded(path: &Path, patterns: &[Pattern]) -> bool {
    let as_str = path.to_string_lossy();
    patterns.iter().any(|p| p.matches(as_str.as_ref()))
}

/// Name of `path` inside the archive: relative to the source root's parent, with
/// forward slashes and no drive letter.
fn archive_name(source_root: &Path, path: &Path) -> String {
    let base = source_root.parent().unwrap_or(source_root);
    let rel = path.strip_prefix(base).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

/// A [`Write`] that hands the archive to the uploader through a bounded channel
/// in 1 MiB pieces (a full channel is backpressure on the archiver). Pair it
/// with [`crate::sql::vdi::ChannelReader`] on the upload side.
pub struct ChannelWriter {
    tx: SyncSender<Vec<u8>>,
    buf: Vec<u8>,
}

const PIECE: usize = 1 << 20;

impl ChannelWriter {
    pub fn new(tx: SyncSender<Vec<u8>>) -> Self {
        Self {
            tx,
            buf: Vec::with_capacity(PIECE),
        }
    }

    fn send(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let piece = std::mem::replace(&mut self.buf, Vec::with_capacity(PIECE));
        self.tx.send(piece).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the PBS upload stopped")
        })
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= PIECE {
            self.send()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.send()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pbsgui-archive-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(tar_bytes: &[u8]) -> Vec<String> {
        let mut a = tar::Archive::new(tar_bytes);
        a.entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn reads_from_the_mapped_root_but_names_by_the_live_path() {
        let dir = tmpdir("map");
        // The "shadow copy" holds the data; the live path does not exist.
        let shadow = dir.join("shadow").join("Users");
        std::fs::create_dir_all(shadow.join("pat")).unwrap();
        std::fs::write(shadow.join("pat").join("a.txt"), b"hello").unwrap();
        std::fs::write(shadow.join("pat").join("skip.tmp"), b"x").unwrap();
        let live = dir.join("live").join("Users");
        let live_s = live.to_string_lossy().into_owned();

        let shadow_c = shadow.clone();
        let live_c = live.clone();
        let map = move |p: &Path| (p == live_c).then(|| shadow_c.clone());
        let excludes = vec![format!("{}", live.join("*").join("*.tmp").display())];
        let mut out = Vec::new();
        let report = write_tar(&[live_s], &excludes, &map, &mut out).unwrap();

        assert_eq!(names(&out), vec!["Users/pat/a.txt".to_string()]);
        assert_eq!(report.entries, vec![("Users/pat/a.txt".to_string(), 5)]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_source_is_skipped_not_fatal() {
        let dir = tmpdir("missing");
        let good = dir.join("good");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join("f"), b"1").unwrap();
        let sources = vec![
            dir.join("nope").to_string_lossy().into_owned(),
            good.to_string_lossy().into_owned(),
        ];
        let mut out = Vec::new();
        let report = write_tar(&sources, &[], &|_| None, &mut out).unwrap();
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].path.ends_with("nope"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_is_skipped_and_the_rest_archived() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("denied");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a"), b"1").unwrap();
        std::fs::write(src.join("b"), b"2").unwrap();
        std::fs::set_permissions(src.join("a"), std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores permissions, so this only proves something as a user.
        if std::fs::File::open(src.join("a")).is_ok() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let mut out = Vec::new();
        let report = write_tar(
            &[src.to_string_lossy().into_owned()],
            &[],
            &|_| None,
            &mut out,
        )
        .unwrap();
        assert_eq!(names(&out), vec!["src/b".to_string()]);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].path.ends_with("a"));
        std::fs::set_permissions(src.join("a"), std::fs::Permissions::from_mode(0o644)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_writer_delivers_everything_in_order() {
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let reader = std::thread::spawn(move || rx.into_iter().flatten().collect::<Vec<u8>>());
        let mut w = ChannelWriter::new(tx);
        let data: Vec<u8> = (0..(3 * PIECE + 17)).map(|i| (i % 251) as u8).collect();
        for part in data.chunks(4096) {
            w.write_all(part).unwrap();
        }
        w.flush().unwrap();
        drop(w);
        assert_eq!(reader.join().unwrap(), data);
    }
}
