//! Copy one folder onto a chosen drive with robocopy.
//!
//! The destination is `{drive}\{Name}_Backup`. robocopy copies the files inside
//! the source, including nested folders. Junctions are skipped so a link such as
//! `Documents\My Music` is not followed into Music.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn copy_folder(source: &Path, destination_root: &Path, name: &str) -> io::Result<PathBuf> {
    let destination = destination_root.join(format!("{name}_Backup"));
    let output = Command::new("robocopy")
        .arg(source)
        .arg(&destination)
        .args(["/E", "/XJ", "/R:2", "/W:2"])
        .output()?;

    let code = output.status.code().unwrap_or(16);
    if (0..8).contains(&code) {
        return Ok(destination);
    }

    let detail = command_detail(&output.stderr, &output.stdout);
    let message = if detail.is_empty() {
        format!("robocopy stopped with code {code}")
    } else {
        format!("robocopy stopped with code {code}: {detail}")
    };
    Err(io::Error::other(message))
}

fn command_detail(stderr: &[u8], stdout: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return tail(stderr);
    }
    tail(String::from_utf8_lossy(stdout).trim())
}

fn tail(text: &str) -> String {
    const LIMIT: usize = 400;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    text.chars()
        .rev()
        .take(LIMIT)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn copies_nested_files_into_a_backup_folder() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("migrate-copy-{stamp}"));
        let source = root.join("source");
        let destination_root = root.join("drive");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        fs::write(source.join("a.txt"), b"hello").unwrap();
        fs::write(source.join("nested").join("b.txt"), b"world!").unwrap();

        let destination = copy_folder(&source, &destination_root, "Downloads").unwrap();

        assert_eq!(destination, destination_root.join("Downloads_Backup"));
        assert_eq!(fs::read(destination.join("a.txt")).unwrap(), b"hello");
        assert_eq!(
            fs::read(destination.join("nested").join("b.txt")).unwrap(),
            b"world!"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
