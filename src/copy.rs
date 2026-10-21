//! Copy one folder onto a chosen drive with robocopy.
//!
//! The destination is `{drive}\{Name}_Backup`. robocopy copies the files inside
//! the source, including nested folders. Junctions are skipped so a link such as
//! `Documents\My Music` is not followed into Music.
//!
//! `progress` is called with `(bytes_copied, bytes_total)` while the copy runs.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

pub fn copy_folder(
    source: &Path,
    destination_root: &Path,
    name: &str,
    progress: &mut dyn FnMut(u64, u64),
) -> io::Result<PathBuf> {
    let destination = destination_root.join(format!("{name}_Backup"));
    let total = measure(source);
    progress(0, total);
    let mut copied = 0;
    robocopy(source, &destination, total, &mut copied, total, progress)?;
    progress(total, total);
    Ok(destination)
}

/// Copy the chosen files and folders into `{drive}\{Name}_Backup`.
/// A folder is copied with everything inside it. A file is copied on its own.
pub fn copy_items(
    parent_name: &str,
    destination_root: &Path,
    items: &[crate::scan::Child],
    progress: &mut dyn FnMut(u64, u64),
) -> io::Result<PathBuf> {
    let destination = destination_root.join(format!("{parent_name}_Backup"));
    std::fs::create_dir_all(&destination)?;

    let mut total: u64 = 0;
    for item in items {
        total = total.saturating_add(match item.kind {
            crate::scan::ChildKind::File => item.bytes.max(file_len(&item.path)),
            crate::scan::ChildKind::Folder => measure(&item.path),
        });
    }
    progress(0, total);

    let mut copied = 0;
    for item in items {
        let target = destination.join(&item.name);
        match item.kind {
            crate::scan::ChildKind::Folder => {
                let folder_bytes = measure(&item.path);
                robocopy(
                    &item.path,
                    &target,
                    folder_bytes,
                    &mut copied,
                    total,
                    progress,
                )?;
            }
            crate::scan::ChildKind::File => {
                copy_file(&item.path, &target, &mut copied, total, progress)?;
            }
        }
    }
    progress(total, total);
    Ok(destination)
}

fn file_len(path: &Path) -> u64 {
    fs::symlink_metadata(path)
        .map(|meta| meta.len())
        .unwrap_or(0)
}

/// Bytes in this tree. Junctions and other links are skipped, matching robocopy `/XJ`.
fn measure(path: &Path) -> u64 {
    let mut total: u64 = 0;
    measure_at(path, &mut total);
    total
}

fn measure_at(path: &Path, total: &mut u64) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return;
    }
    if meta.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            measure_at(&entry.path(), total);
        }
    } else {
        *total = total.saturating_add(meta.len());
    }
}

fn copy_file(
    source: &Path,
    destination: &Path,
    copied: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut from = File::open(source)?;
    let mut to = File::create(destination)?;
    let mut buf = vec![0_u8; 1024 * 1024];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n])?;
        *copied = copied.saturating_add(n as u64);
        progress(*copied, total);
    }
    Ok(())
}

fn robocopy(
    source: &Path,
    destination: &Path,
    folder_bytes: u64,
    copied: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> io::Result<()> {
    let mut child = Command::new("robocopy")
        .arg(source)
        .arg(destination)
        .args(["/E", "/XJ", "/R:2", "/W:2", "/BYTES"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;

    let stderr = child.stderr.take();
    let stderr_task = thread::spawn(move || {
        let mut captured = Vec::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_end(&mut captured);
        }
        captured
    });

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("robocopy did not return its output"))?;
    let mut meter = RoboMeter::default();
    let mut chunk = [0_u8; 8192];
    let mut tail = Vec::new();
    let started = *copied;
    loop {
        let n = stdout.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&chunk[..n]);
        if tail.len() > 8000 {
            tail.drain(..tail.len() - 4000);
        }
        let text = String::from_utf8_lossy(&chunk[..n]);
        meter.push(&text);
        let seen = meter.copied().min(folder_bytes);
        progress(started.saturating_add(seen), total);
    }

    let status = child.wait()?;
    let stderr = stderr_task.join().unwrap_or_default();
    let code = status.code().unwrap_or(16);
    if (0..8).contains(&code) {
        *copied = started.saturating_add(folder_bytes);
        progress(*copied, total);
        return Ok(());
    }

    let detail = command_detail(&stderr, &tail);
    let message = if detail.is_empty() {
        format!("robocopy stopped with code {code}")
    } else {
        format!("robocopy stopped with code {code}: {detail}")
    };
    Err(io::Error::other(message))
}

/// Bytes robocopy has reported from its stdout.
#[derive(Debug, Default)]
struct RoboMeter {
    pending: String,
    finished: u64,
    current_size: u64,
    current_pct: f32,
}

impl RoboMeter {
    fn push(&mut self, text: &str) {
        self.pending.push_str(text);
        while let Some(pos) = self.pending.find(['\r', '\n']) {
            let line: String = self.pending.drain(..=pos).collect();
            self.consume_line(line.trim_end_matches(['\r', '\n']));
        }
        if self.pending.contains('%') {
            let line = std::mem::take(&mut self.pending);
            self.consume_line(&line);
        }
    }

    fn consume_line(&mut self, line: &str) {
        if let Some(size) = file_size(line) {
            self.finish_current();
            self.current_size = size;
            self.current_pct = 0.0;
            return;
        }
        if let Some(pct) = line_percent(line) {
            self.current_pct = pct.clamp(0.0, 100.0);
        }
    }

    fn finish_current(&mut self) {
        self.finished = self.finished.saturating_add(self.current_size);
        self.current_size = 0;
        self.current_pct = 0.0;
    }

    fn copied(&self) -> u64 {
        let partial = (self.current_size as f64 * f64::from(self.current_pct) / 100.0) as u64;
        self.finished.saturating_add(partial)
    }
}

fn file_size(line: &str) -> Option<u64> {
    const KINDS: [&str; 4] = ["new file", "newer", "older", "changed"];
    let lower = line.to_ascii_lowercase();
    let kind = KINDS.iter().find(|kind| lower.contains(*kind))?;
    let after = &line[lower.find(kind)? + kind.len()..];
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn line_percent(line: &str) -> Option<f32> {
    let number = line.trim().strip_suffix('%')?.trim();
    if number.is_empty() || number.chars().any(|ch| !(ch.is_ascii_digit() || ch == '.')) {
        return None;
    }
    number.parse().ok()
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
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn counts_a_partial_file_then_the_next_one() {
        let mut meter = RoboMeter::default();
        meter.push("\t    New File  \t\t 1000\tbig.bin\r\n\t  50%\r");
        assert_eq!(meter.copied(), 500);
        meter.push("\t    New File  \t\t 200\tsmall.txt\r\n");
        assert_eq!(meter.copied(), 1000);
        meter.finish_current();
        assert_eq!(meter.copied(), 1200);
    }

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

        let mut last = (0, 0);
        let destination = copy_folder(
            &source,
            &destination_root,
            "Downloads",
            &mut |copied, total| {
                last = (copied, total);
            },
        )
        .unwrap();

        assert_eq!(destination, destination_root.join("Downloads_Backup"));
        assert_eq!(fs::read(destination.join("a.txt")).unwrap(), b"hello");
        assert_eq!(
            fs::read(destination.join("nested").join("b.txt")).unwrap(),
            b"world!"
        );
        assert_eq!(last, (11, 11));
        let _ = fs::remove_dir_all(&root);
    }
}
