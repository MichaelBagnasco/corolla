//! Keep the selection tied to the directory entry that was collected.
//! Symlinks are entries in their own right and must never be followed.
use std::{
    env,
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone)]
pub struct Target {
    pub path: PathBuf,
    identity: (u64, u64, i64, i64),
    is_dir: bool,
}

// Protect the system directories themselves. Ordinary entries inside them are
// subject to the same parent-directory permissions as files on a data drive.
fn system_root(path: &Path) -> bool {
    const ROOTS: &[&str] = &[
        "/",
        "/bin",
        "/boot",
        "/dev",
        "/etc",
        "/home",
        "/lib",
        "/lib32",
        "/lib64",
        "/lost+found",
        "/media",
        "/mnt",
        "/opt",
        "/proc",
        "/root",
        "/run",
        "/sbin",
        "/srv",
        "/swap",
        "/sys",
        "/tmp",
        "/usr",
        "/var",
        "/usr/local",
        "/var/tmp",
    ];
    ROOTS.iter().any(|root| path == Path::new(root)) || path.parent() == Some(Path::new("/home"))
}

fn can_remove_from(parent: &Path, entry: &fs::Metadata) -> Result<(), String> {
    let raw = CString::new(parent.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let mut fs_info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // statfs initializes the output structure on success.
    if unsafe { libc::statfs(raw.as_ptr(), fs_info.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let fs_type = unsafe { fs_info.assume_init() }.f_type;
    if matches!(
        fs_type,
        libc::PROC_SUPER_MAGIC
            | libc::SYSFS_MAGIC
            | libc::DEVPTS_SUPER_MAGIC
            | libc::CGROUP_SUPER_MAGIC
            | libc::CGROUP2_SUPER_MAGIC
            | libc::DEBUGFS_MAGIC
            | libc::SECURITYFS_MAGIC
            | libc::TRACEFS_MAGIC
            | libc::BPF_FS_MAGIC
    ) {
        return Err("Kernel filesystem entries are protected.".into());
    }
    let parent_info = fs::metadata(parent).map_err(|e| e.to_string())?;
    // devtmpfs shares its filesystem magic with tmpfs. Compare device IDs to
    // distinguish /dev itself from separate mounts such as /dev/shm.
    if parent.starts_with("/dev")
        && fs::metadata("/dev").is_ok_and(|dev| dev.dev() == parent_info.dev())
    {
        return Err("Device filesystem entries are protected.".into());
    }
    // Let Linux evaluate ACLs, effective IDs and read-only mounts. Unlinking needs
    // write and search permission on the parent, regardless of the file's mode.
    let access = unsafe {
        libc::faccessat(
            libc::AT_FDCWD,
            raw.as_ptr(),
            libc::W_OK | libc::X_OK,
            libc::AT_EACCESS,
        )
    };
    if access != 0 {
        return Err("You do not have permission to remove items from this folder.".into());
    }
    let uid = unsafe { libc::geteuid() };
    if parent_info.mode() & libc::S_ISVTX != 0
        && uid != 0
        && parent_info.uid() != uid
        && entry.uid() != uid
    {
        return Err("This shared folder protects items owned by another user.".into());
    }
    Ok(())
}

impl Target {
    pub fn capture(path: &Path) -> Result<Self, String> {
        let home = env::var_os("HOME").map(PathBuf::from);
        let parent = path.parent().ok_or("The filesystem root is protected.")?;
        let name = path.file_name().ok_or("Choose a file or folder.")?;
        let resolved = fs::canonicalize(parent)
            .map_err(|e| e.to_string())?
            .join(name);
        if resolved != path {
            return Err("This location changed since the scan.".into());
        }
        let canonical_home = home.as_deref().and_then(|h| h.canonicalize().ok());
        if home
            .as_deref()
            .into_iter()
            .chain(canonical_home.as_deref())
            .any(|h| h.starts_with(path))
        {
            return Err("Your home folder and its parents are protected.".into());
        }
        if system_root(path) {
            return Err("This system location is protected.".into());
        }
        if crate::scan::contains_mount(path).map_err(|e| e.to_string())? {
            return Err("This location contains a mounted filesystem.".into());
        }
        let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        can_remove_from(parent, &m)?;
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
    fn protects_system_roots_while_allowing_user_owned_working_directories() {
        for p in [
            "/usr",
            "/usr/local",
            "/etc",
            "/proc",
            "/sys",
            "/boot",
            "/home/alice",
            "/var",
            "/var/tmp",
        ] {
            assert!(system_root(Path::new(p)), "{p}");
        }
        for p in [
            "/home/alice/Downloads",
            "/usr/local/work/tree",
            "/var/home/alice/Downloads",
            "/var/lib/my-app/output",
            "/opt/studio/scratch",
            "/mnt/disk/etc",
            "/media/usb/boot",
            "/run/media/alice/external/photos",
            "/run/user/1000/gvfs/sftp:host=server/data",
            "/srv/team/files",
            "/tmp/usr",
            "/var/tmp/my-file",
        ] {
            assert!(!system_root(Path::new(p)), "{p}");
        }
        assert!(Target::capture(Path::new("/")).is_err());
        assert!(Target::capture(Path::new("/etc")).is_err());
        assert!(Target::capture(Path::new("/proc/uptime")).is_err());
        assert!(Target::capture(Path::new("/dev/null")).is_err());
        if let Some(home) = env::var_os("HOME") {
            assert!(Target::capture(Path::new(&home)).is_err());
        }
    }

    #[test]
    fn removal_eligibility_uses_directory_permissions_and_keeps_symlinks_as_entries() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("owned data");
        fs::create_dir(&parent).unwrap();
        let link = parent.join("system-link");
        symlink("/usr", &link).unwrap();
        let before = Target::capture(&link).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        let read_only = Target::from_scan(&link, before.identity);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert!(read_only.is_err());
        }
        assert!(Target::from_scan(&link, before.identity).is_ok());
    }

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
