//! Walk a folder and total how many files it holds and how large they are.
//!
//! Directory junctions and symlinks are not followed, so a cyclic link cannot
//! make the walk run forever.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct ScanTotals {
    pub files: u64,
    pub bytes: u64,
}

/// `on_progress` is called every so often with the running totals.
/// The final totals are the returned value, not the last progress call.
pub fn scan_folder(path: &Path, mut on_progress: impl FnMut(u64, u64)) -> io::Result<ScanTotals> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![path.to_path_buf()];
    let mut root_pending = true;
    let mut last_report = Instant::now();

    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if root_pending => return Err(err),
            Err(_) => continue,
        };
        root_pending = false;

        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            // Does not follow links, so a junction still looks like a junction.
            let Ok(meta) = entry.metadata() else {
                continue;
            };

            if meta.is_dir() {
                if is_reparse_point(&meta) {
                    continue;
                }
                stack.push(entry.path());
                continue;
            }

            if meta.is_file() {
                files += 1;
                bytes = bytes.saturating_add(meta.len());
                if last_report.elapsed() >= Duration::from_millis(80) {
                    on_progress(files, bytes);
                    last_report = Instant::now();
                }
            }
        }
    }

    Ok(ScanTotals { files, bytes })
}

#[cfg(windows)]
fn is_reparse_point(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace() -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("migrate-scan-{stamp}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("nested")).unwrap();
        path
    }

    #[test]
    fn sums_files_in_nested_folders() {
        let root = temp_workspace();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        fs::write(root.join("nested").join("b.txt"), b"world!").unwrap();

        let totals = scan_folder(&root, |_, _| {}).unwrap();
        assert_eq!(totals.files, 2);
        assert_eq!(totals.bytes, 11);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reports_a_missing_root() {
        let missing = std::env::temp_dir().join("migrate-scan-missing-folder");
        let _ = fs::remove_dir_all(&missing);
        let error = scan_folder(&missing, |_, _| {}).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
