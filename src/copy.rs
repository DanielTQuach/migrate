//! Copy one folder onto a chosen drive with robocopy.
//!
//! The destination is `{drive}\{Name}_Backup`. robocopy copies the files inside
//! the source, including nested folders. Junctions are skipped so a link such as
//! `Documents\My Music` is not followed into Music.
//!
//! A destination inside the source is refused. robocopy would otherwise copy the
//! new backup folder into itself and nest copies without stopping — for example
//! choosing Desktop while copying Desktop.
//!
//! `progress` is called with `(bytes_copied, bytes_total)` while the copy runs.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// The backup folder `{destination_root}\{name}_Backup` would sit inside `source`.
pub fn backup_would_nest(source: &Path, destination_root: &Path, name: &str) -> bool {
    let destination = destination_root.join(format!("{name}_Backup"));
    destination_is_inside(source, &destination)
}

/// Copying this folder would put the backup inside the folder itself.
pub fn item_would_nest(
    source: &Path,
    destination_root: &Path,
    parent_name: &str,
    item_name: &str,
) -> bool {
    let target = destination_root
        .join(format!("{parent_name}_Backup"))
        .join(item_name);
    destination_is_inside(source, &target)
}

pub fn copy_folder(
    source: &Path,
    destination_root: &Path,
    name: &str,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(u64, u64),
) -> io::Result<PathBuf> {
    let destination = destination_root.join(format!("{name}_Backup"));
    ensure_outside(source, &destination, name)?;
    stopped(cancel)?;
    let total = measure(source, cancel);
    stopped(cancel)?;
    progress(0, total);
    let mut copied = 0;
    robocopy(
        source,
        &destination,
        total,
        &mut copied,
        total,
        cancel,
        progress,
    )?;
    progress(total, total);
    Ok(destination)
}

/// Copy the chosen files and folders into `{drive}\{Name}_Backup`.
/// A folder is copied with everything inside it. A file is copied on its own.
pub fn copy_items(
    parent_name: &str,
    destination_root: &Path,
    items: &[crate::scan::Child],
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(u64, u64),
) -> io::Result<PathBuf> {
    let destination = destination_root.join(format!("{parent_name}_Backup"));
    for item in items {
        if item.kind == crate::scan::ChildKind::Folder {
            let target = destination.join(&item.name);
            ensure_outside(&item.path, &target, &item.name)?;
        }
    }
    stopped(cancel)?;
    std::fs::create_dir_all(&destination)?;

    let mut total: u64 = 0;
    for item in items {
        total = total.saturating_add(match item.kind {
            crate::scan::ChildKind::File => item.bytes.max(file_len(&item.path)),
            crate::scan::ChildKind::Folder => measure(&item.path, cancel),
        });
    }
    stopped(cancel)?;
    progress(0, total);

    let mut copied = 0;
    for item in items {
        stopped(cancel)?;
        let target = destination.join(&item.name);
        match item.kind {
            crate::scan::ChildKind::Folder => {
                let folder_bytes = measure(&item.path, cancel);
                robocopy(
                    &item.path,
                    &target,
                    folder_bytes,
                    &mut copied,
                    total,
                    cancel,
                    progress,
                )?;
            }
            crate::scan::ChildKind::File => {
                copy_file(&item.path, &target, &mut copied, total, cancel, progress)?;
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
///
/// The walk is a heap list. A backup nested dozens of folders deep must not use
/// one stack frame per folder — that is the panic "has overflowed its stack".
fn measure(path: &Path, cancel: &Arc<AtomicBool>) -> u64 {
    let mut total: u64 = 0;
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            return total;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            continue;
        }
        if meta.is_dir() {
            let Ok(entries) = fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                pending.push(entry.path());
            }
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

fn copy_file(
    source: &Path,
    destination: &Path,
    copied: &mut u64,
    total: u64,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(u64, u64),
) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut from = File::open(source)?;
    let mut to = File::create(destination)?;
    let mut buf = vec![0_u8; 1024 * 1024];
    loop {
        stopped(cancel)?;
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
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(u64, u64),
) -> io::Result<()> {
    let folder_name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "this folder".to_string());
    ensure_outside(source, destination, &folder_name)?;

    // `/XD` keeps robocopy from walking into the folder it is writing. That
    // stops a destination inside the source from nesting copies forever.
    let mut child = Command::new("robocopy")
        .arg(source)
        .arg(destination)
        .args(["/E", "/XJ", "/R:2", "/W:2", "/BYTES", "/XD"])
        .arg(destination)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;

    let stop_watch = Arc::new(AtomicBool::new(false));
    let watch = {
        let cancel = Arc::clone(cancel);
        let stop_watch = Arc::clone(&stop_watch);
        let pid = child.id();
        thread::spawn(move || {
            while !stop_watch.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    stop_process(pid);
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(40));
            }
        })
    };

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
    stop_watch.store(true, Ordering::Relaxed);
    let _ = watch.join();
    let stderr = stderr_task.join().unwrap_or_default();
    stopped(cancel)?;
    let code = status.code().unwrap_or(16);
    if (0..8).contains(&code) {
        *copied = started.saturating_add(folder_bytes);
        progress(*copied, total);
        return Ok(());
    }

    let detail = command_detail(&stderr, &tail);
    if runaway_copy(&detail) {
        return Err(io::Error::other(format!(
            "{folder_name} has folders nested too deeply for Windows to copy. Remove the nested backup inside it, then try again."
        )));
    }
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

/// Refuse when `destination` is `source`, or a folder inside it.
///
/// Paths are compared by their real location, so `Desktop` and `desktop`, or a
/// `\\?\` path from the folder picker, still match. A sibling such as
/// `Desktop_Backup` does not.
fn stopped(cancel: &Arc<AtomicBool>) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"))
    } else {
        Ok(())
    }
}

fn stop_process(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return;
        }
        TerminateProcess(handle, 1);
        CloseHandle(handle);
    }
}

fn ensure_outside(source: &Path, destination: &Path, name: &str) -> io::Result<()> {
    if destination_is_inside(source, destination) {
        Err(nest_error(name))
    } else {
        Ok(())
    }
}

fn nest_error(name: &str) -> io::Error {
    io::Error::other(format!(
        "Choose a destination outside {name}. A folder inside {name} would be copied into itself over and over."
    ))
}

/// Windows says this when a copy nests folders until the path is too deep.
fn runaway_copy(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("overflowed its stack")
        || lower.contains("stack overflowed")
        || lower.contains("recursion too deep")
        || lower.contains("filename or extension is too long")
}

fn destination_is_inside(source: &Path, destination: &Path) -> bool {
    let source = normalize_path(source);
    let destination = normalize_path(destination);
    destination.starts_with(&source)
}

fn normalize_path(path: &Path) -> PathBuf {
    let resolved = resolve_existing_prefix(path);
    lowercase_path(&lexical_normalize(&strip_verbatim(&resolved)))
}

/// Canonicalize the longest prefix that already exists, then put the rest back.
/// The backup folder usually does not exist yet, but its parent does.
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    let mut pending = Vec::new();
    loop {
        if let Ok(canon) = fs::canonicalize(&current) {
            let mut resolved = canon;
            for part in pending.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        let Some(name) = current.file_name().map(|name| name.to_os_string()) else {
            return path.to_path_buf();
        };
        if !current.pop() {
            return path.to_path_buf();
        }
        pending.push(name);
    }
}

fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.file_name().is_some() {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn lowercase_path(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
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

    fn stay() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

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
            &stay(),
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

    #[test]
    fn refuses_a_destination_inside_the_source() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("migrate-nest-{stamp}"));
        let source = root.join("Desktop");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"hello").unwrap();

        let error = copy_folder(&source, &source, "Desktop", &stay(), &mut |_, _| {}).unwrap_err();
        assert!(error.to_string().contains("outside Desktop"), "{}", error);
        assert!(!source.join("Desktop_Backup").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_a_selected_folder_that_contains_the_destination() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("migrate-nest-item-{stamp}"));
        let projects = root.join("Projects");
        fs::create_dir_all(&projects).unwrap();
        fs::write(projects.join("a.txt"), b"hello").unwrap();
        let item = crate::scan::Child {
            name: "Projects".to_string(),
            path: projects.clone(),
            kind: crate::scan::ChildKind::Folder,
            bytes: 0,
            selected: true,
            hidden: false,
        };

        let error = copy_items("Desktop", &projects, &[item], &stay(), &mut |_, _| {}).unwrap_err();
        assert!(error.to_string().contains("outside Projects"), "{}", error);
        assert!(!projects.join("Desktop_Backup").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn measures_a_deep_tree_without_recursing_on_the_stack() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("migrate-deep-{stamp}"));
        let mut path = root.clone();
        for index in 0..80 {
            path.push(format!("level{index}"));
        }
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("a.txt"), b"hi").unwrap();
        assert_eq!(measure(&root, &stay()), 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_destination_inside_downloads_is_left_out_of_the_export() {
        let source = Path::new(r"C:\Users\me\Downloads");
        let inside = source.join("BACKUP");
        assert!(backup_would_nest(source, &inside, "Downloads"));
        assert!(!backup_would_nest(
            source,
            Path::new(r"D:\Drive"),
            "Downloads"
        ));
        assert!(item_would_nest(&inside, &inside, "Downloads", "BACKUP"));
    }

    #[test]
    fn a_sibling_whose_name_starts_like_the_source_is_outside_it() {
        let source = Path::new(r"C:\Users\me\Desktop");
        let sibling = Path::new(r"C:\Users\me\Desktop_Backup");
        assert!(!destination_is_inside(source, sibling));
    }

    #[test]
    fn a_backup_directory_inside_the_source_is_rejected() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("migrate-backup-dir-{stamp}"));
        let source = root.join("Downloads");
        let destination_root = source.join("BACKUP");
        fs::create_dir_all(&destination_root).unwrap();
        fs::write(source.join("a.txt"), b"hello").unwrap();

        let error = copy_folder(
            &source,
            &destination_root,
            "Downloads",
            &stay(),
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("outside Downloads"), "{}", error);
        assert!(!destination_root.join("Downloads_Backup").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_windows_nesting_failure_uses_the_plain_message() {
        assert!(runaway_copy(
            "ERROR 1001 (0x000003E9) Creating Destination Directory C:\\Users\\me\\Downloads\\BACKUP\\Downloads_Backup The recursion too deep; the stack overflowed."
        ));
        assert!(!runaway_copy("access is denied"));
    }

    #[test]
    fn nesting_matches_case_and_a_verbatim_prefix() {
        let source = Path::new(r"\\?\C:\Users\me\Desktop");
        let destination = Path::new(r"c:\users\me\desktop\Desktop_Backup");
        assert!(destination_is_inside(source, destination));
    }
}
