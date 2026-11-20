//! Keep the selection tied to the directory entry that was collected.
//! Symlinks are entries in their own right and must never be followed.
use std::{
    env, fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone)]
pub struct Target {
    pub path: PathBuf,
    identity: (u64, u64, i64, i64),
}

impl Target {
    pub fn capture(path: &Path) -> Result<Self, String> {
        let home = env::var_os("HOME").map(PathBuf::from);
        let parent = path.parent().ok_or("The filesystem root is protected.")?;
        let name = path.file_name().ok_or("Choose a file or folder.")?;
        let resolved = fs::canonicalize(parent)
            .map_err(|e| e.to_string())?
            .join(name);
        if resolved != path || home.as_deref().is_some_and(|h| h.starts_with(path)) {
            return Err("This folder or its parent is protected or has changed.".into());
        }
        if crate::scan::contains_mount(path).map_err(|e| e.to_string())? {
            return Err("This location contains a mounted filesystem.".into());
        }
        let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        Ok(Self {
            path: path.into(),
            identity: (m.dev(), m.ino(), m.ctime(), m.ctime_nsec()),
        })
    }

    pub fn from_scan(path: &Path, scanned: (u64, u64, i64, i64)) -> Result<Self, String> {
        let current = Self::capture(path)?;
        if current.identity != scanned {
            return Err("This item changed since the scan. Rescan before collecting it.".into());
        }
        Ok(current)
    }

    fn verify(&self) -> Result<(), String> {
        if Self::capture(&self.path)?.identity != self.identity {
            return Err("This item changed after collection. Remove it and scan again.".into());
        }
        Ok(())
    }
}

fn apply(
    targets: &[Target],
    mut action: impl FnMut(&Path) -> Result<(), String>,
) -> Vec<(PathBuf, String)> {
    targets
        .iter()
        .filter_map(|item| {
            item.verify()
                .and_then(|_| action(&item.path))
                .err()
                .map(|e| (item.path.clone(), e))
        })
        .collect()
}

fn run(command: &mut Command) -> Result<(), String> {
    let output = command.output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

pub fn trash(targets: &[Target]) -> Vec<(PathBuf, String)> {
    apply(targets, |path| {
        run(Command::new("gio").args(["trash", "--"]).arg(path))
    })
}

pub fn delete_permanently(targets: &[Target]) -> Vec<(PathBuf, String)> {
    apply(targets, |path| {
        run(Command::new("rm")
            .args([
                "--recursive",
                "--one-file-system",
                "--preserve-root=all",
                "--interactive=never",
                "--",
            ])
            .arg(path))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_entries_are_retained_and_symlinks_are_unlinked() {
        let dir = env::temp_dir().join(format!("corolla-target-test-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let file = dir.join("file");
        fs::write(&file, "original").unwrap();
        let old = Target::capture(&file).unwrap();
        fs::rename(&file, dir.join("moved")).unwrap();
        fs::write(&file, "replacement").unwrap();
        assert!(Target::from_scan(&file, old.identity).is_err());
        assert_eq!(delete_permanently(&[old]).len(), 1);
        assert_eq!(fs::read(&file).unwrap(), b"replacement");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(delete_permanently(&[Target::capture(&link).unwrap()]).is_empty());
        assert!(file.exists());
        assert!(!link.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
