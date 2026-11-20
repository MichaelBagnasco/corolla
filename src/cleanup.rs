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
    is_dir: bool,
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
            is_dir: m.is_dir(),
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

#[derive(Default)]
pub struct CleanupReport {
    pub failed: Vec<(PathBuf, String)>,
    /// A recursive removal may have removed children before reporting failure.
    pub recheck: Vec<PathBuf>,
}

pub fn remove(targets: &[Target], permanent: bool) -> CleanupReport {
    let mut report = CleanupReport::default();
    for target in targets {
        if let Err(error) = target.verify() {
            report.failed.push((target.path.clone(), error));
            continue;
        }
        let mut command = if permanent {
            let mut rm = Command::new("rm");
            rm.args([
                "--recursive",
                "--one-file-system",
                "--preserve-root=all",
                "--interactive=never",
                "--",
            ]);
            rm
        } else {
            let mut gio = Command::new("gio");
            gio.args(["trash", "--"]);
            gio
        };
        let error = match command.arg(&target.path).output() {
            Ok(output) if output.status.success() => continue,
            Ok(output) => {
                if permanent && target.is_dir {
                    report.recheck.push(target.path.clone());
                }
                String::from_utf8_lossy(&output.stderr).trim().to_owned()
            }
            Err(error) => error.to_string(),
        };
        report.failed.push((target.path.clone(), error));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_recursive_removal_marks_only_the_attempted_directory_for_recheck() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("tree");
        let protected = dir.join("protected");
        fs::create_dir_all(&protected).unwrap();
        fs::write(protected.join("keep"), "data").unwrap();
        fs::write(dir.join("removable"), "data").unwrap();
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o500)).unwrap();
        let target = Target::capture(&dir).unwrap();
        let report = remove(&[target], true);
        if !protected.exists() {
            // Privileged test runners can remove the entire fixture.
            assert!(report.failed.is_empty() && report.recheck.is_empty());
            return;
        }
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(report.recheck, vec![dir.clone()]);
        assert_eq!(report.failed[0].0, dir);
        assert!(!dir.join("removable").exists());
    }

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
        let report = remove(&[old], true);
        assert_eq!(report.failed.len(), 1);
        assert!(report.recheck.is_empty());
        assert_eq!(fs::read(&file).unwrap(), b"replacement");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(
            remove(&[Target::capture(&link).unwrap()], true)
                .failed
                .is_empty()
        );
        assert!(file.exists());
        assert!(!link.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
