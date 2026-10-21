//! Read one folder and list the files and folders sitting directly inside it.
//!
//! Nothing nested is opened. A folder's own contents stay unread.
//! Junctions such as `Documents\My Music` are skipped. Hidden items stay in
//! the list and are marked, so the window can show or ignore them.

use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildKind {
    Folder,
    File,
}

#[derive(Debug, Clone)]
pub struct Child {
    pub name: String,
    pub path: PathBuf,
    pub kind: ChildKind,
    pub bytes: u64,
    pub selected: bool,
    pub hidden: bool,
}

/// One pass over `path`. Subfolders are names only; their contents are not read.
pub fn list_folder(path: &Path) -> io::Result<Vec<Child>> {
    let mut children = Vec::new();

    for entry in fs::read_dir(path)? {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let child_path = entry.path();
        // `symlink_metadata` does not follow a link, so a junction that denies
        // access is still readable as a link and can be skipped.
        let Ok(meta) = fs::symlink_metadata(&child_path) else {
            continue;
        };
        let attributes = meta.file_attributes();
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            continue;
        }
        let hidden = attributes & FILE_ATTRIBUTE_HIDDEN != 0;

        if meta.is_dir() {
            children.push(Child {
                name,
                path: child_path,
                kind: ChildKind::Folder,
                bytes: 0,
                selected: false,
                hidden,
            });
        } else {
            children.push(Child {
                name,
                path: child_path,
                kind: ChildKind::File,
                bytes: meta.len(),
                selected: false,
                hidden,
            });
        }
    }

    children.sort_by(|left, right| {
        kind_rank(left.kind)
            .cmp(&kind_rank(right.kind))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(children)
}

fn kind_rank(kind: ChildKind) -> u8 {
    match kind {
        ChildKind::Folder => 0,
        ChildKind::File => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace() -> PathBuf {
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
    fn lists_only_the_top_level() {
        let root = temp_workspace();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        fs::write(root.join("nested").join("b.txt"), b"world!").unwrap();

        let children = list_folder(&root).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].name, "nested");
        assert_eq!(children[0].kind, ChildKind::Folder);
        assert_eq!(children[1].name, "a.txt");
        assert_eq!(children[1].kind, ChildKind::File);
        assert_eq!(children[1].bytes, 5);
        assert!(!children[1].hidden);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn marks_a_hidden_file() {
        let root = temp_workspace();
        let hidden = root.join("secret.txt");
        fs::write(&hidden, b"x").unwrap();
        set_hidden(&hidden);

        let children = list_folder(&root).unwrap();
        let secret = children.iter().find(|child| child.name == "secret.txt");
        assert_eq!(secret.map(|child| child.hidden), Some(true));

        let _ = fs::remove_dir_all(&root);
    }

    fn set_hidden(path: &Path) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN);
        }
    }

    #[test]
    fn reports_a_missing_root() {
        let missing = std::env::temp_dir().join("migrate-scan-missing-folder");
        let _ = fs::remove_dir_all(&missing);
        let error = list_folder(&missing).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
