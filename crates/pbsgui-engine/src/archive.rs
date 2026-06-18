//! Build a tar archive of a job's sources.
//!
//! Folders are walked recursively; files are added with a path relative to the
//! source's parent (so the top folder name is preserved), using forward slashes.
//! Glob excludes are applied to each path. This is a single sequential stream
//! that the dedup backup then chunks and uploads.

use std::path::Path;

use glob::Pattern;

/// Write a tar of `sources` (minus `excludes`) to `out`.
pub fn build_tar(sources: &[String], excludes: &[String], out: &Path) -> anyhow::Result<()> {
    let patterns: Vec<Pattern> = excludes
        .iter()
        .filter_map(|e| Pattern::new(e).ok())
        .collect();
    let file = std::fs::File::create(out)?;
    let mut builder = tar::Builder::new(std::io::BufWriter::new(file));

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
                    builder.append_path_with_name(path, archive_name(root, path))?;
                }
            }
        } else if root.is_file() && !excluded(root, &patterns) {
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            builder.append_path_with_name(root, name)?;
        }
    }

    builder.finish()?;
    Ok(())
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
