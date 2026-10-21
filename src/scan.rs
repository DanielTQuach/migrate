//! Read one folder and list the files and folders sitting directly inside it.
//!
//! Nothing nested is opened. A folder's own contents stay unread.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
        let Ok(meta) = entry.metadata() else {
            children.push(Child {
                name,
                path: child_path,
                kind: ChildKind::File,
                bytes: 0,
                selected: false,
            });
            continue;
        };

        if meta.is_dir() {
            children.push(Child {
                name,
                path: child_path,
                kind: ChildKind::Folder,
                bytes: 0,
                selected: false,
            });
        } else {
            children.push(Child {
                name,
                path: child_path,
                kind: ChildKind::File,
                bytes: meta.len(),
                selected: false,
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

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reports_a_missing_root() {
        let missing = std::env::temp_dir().join("migrate-scan-missing-folder");
        let _ = fs::remove_dir_all(&missing);
        let error = list_folder(&missing).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
