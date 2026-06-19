//! Build a tar archive of a job's sources.
//!
//! Folders are walked recursively; files are added with a path relative to the
//! source's parent (so the top folder name is preserved), using forward slashes.
//! Glob excludes are applied to each path. This is a single sequential stream
//! that the dedup backup then chunks and uploads.

use std::path::Path;

use glob::Pattern;

/// Write a tar of `sources` (minus `excludes`) to `out`, returning the
/// (archive path, size) of each file added so a catalog can be stored.
pub fn build_tar(
    sources: &[String],
    excludes: &[String],
    out: &Path,
) -> anyhow::Result<Vec<(String, u64)>> {
    let patterns: Vec<Pattern> = excludes
        .iter()
        .filter_map(|e| Pattern::new(e).ok())
        .collect();
    let file = std::fs::File::create(out)?;
    let mut builder = tar::Builder::new(std::io::BufWriter::new(file));
    let mut entries = Vec::new();

    for source in sources {
        let root = Path::new(source);
        if root.is_dir() {
            // Sort for a deterministic tar across runs, so unchanged files
            // produce identical chunks and dedup actually reuses them.
            for entry in walkdir::WalkDir::new(root)
                .follow_links(false)
                .sort_by_file_name()
            {
                let entry = entry?;
                let path = entry.path();
                if excluded(path, &patterns) {
                    continue;
                }
                if entry.file_type().is_file() {
                    let name = archive_name(root, path);
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    builder.append_path_with_name(path, &name)?;
                    entries.push((name, size));
                }
            }
        } else if root.is_file() && !excluded(root, &patterns) {
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            let size = std::fs::metadata(root).map(|m| m.len()).unwrap_or(0);
            builder.append_path_with_name(root, &name)?;
            entries.push((name, size));
        }
    }

    builder.finish()?;
    Ok(entries)
}

fn excluded(path: &Path, patterns: &[Pattern]) -> bool {
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
