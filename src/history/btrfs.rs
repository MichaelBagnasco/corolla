//! Discover existing Btrfs checkpoints using the kernel metadata and the
//! unprivileged iterator supplied by btrfs-progs. No Dolt database is involved.
use crate::scan;
use libloading::Library;
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, OsString},
    fs::{self, File},
    io,
    mem::{MaybeUninit, size_of},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Default)]
pub struct Report {
    pub source: Option<PathBuf>,
    pub checkpoints: Vec<Checkpoint>,
    pub limited: bool,
}

#[derive(Debug)]
pub struct Checkpoint {
    pub root: PathBuf,
    pub date: String,
    pub readonly: bool,
    pub file: Option<PathBuf>,
}

type Uuid = [u8; 16];

// libbtrfsutil.h, struct btrfs_util_subvolume_info (ABI 1).
#[repr(C)]
struct Info {
    id: u64,
    parent_id: u64,
    dir_id: u64,
    flags: u64,
    uuid: Uuid,
    parent_uuid: Uuid,
    received_uuid: Uuid,
    transactions: [u64; 5],
    ctime: libc::timespec,
    otime: libc::timespec,
    stime: libc::timespec,
    rtime: libc::timespec,
}

type Handle = *mut libc::c_void;
type GetInfo = unsafe extern "C" fn(RawFd, u64, *mut Info) -> i32;
type GetId = unsafe extern "C" fn(RawFd, *mut u64) -> i32;
type Create = unsafe extern "C" fn(RawFd, u64, i32, *mut Handle) -> i32;
type Next = unsafe extern "C" fn(Handle, *mut *mut libc::c_char, *mut u64) -> i32;
type Destroy = unsafe extern "C" fn(Handle);

struct Util {
    info: GetInfo,
    id: GetId,
    create: Create,
    next: Next,
    destroy: Destroy,
    error: unsafe extern "C" fn(i32) -> *const libc::c_char,
    _library: Library,
}

impl Util {
    fn open() -> io::Result<Self> {
        // The library is optional on other filesystems. The functions use the
        // btrfs-progs 1.x ABI and the handle outlives all copied function pointers.
        unsafe {
            let lib = Library::new("libbtrfsutil.so.1").map_err(|e| {
                io::Error::other(format!(
                    "Btrfs checkpoint discovery requires btrfs-progs: {e}"
                ))
            })?;
            Ok(Self {
                info: *lib
                    .get(b"btrfs_util_subvolume_info_fd\0")
                    .map_err(io::Error::other)?,
                id: *lib
                    .get(b"btrfs_util_subvolume_id_fd\0")
                    .map_err(io::Error::other)?,
                create: *lib
                    .get(b"btrfs_util_create_subvolume_iterator_fd\0")
                    .map_err(io::Error::other)?,
                next: *lib
                    .get(b"btrfs_util_subvolume_iterator_next\0")
                    .map_err(io::Error::other)?,
                destroy: *lib
                    .get(b"btrfs_util_destroy_subvolume_iterator\0")
                    .map_err(io::Error::other)?,
                error: *lib
                    .get(b"btrfs_util_strerror\0")
                    .map_err(io::Error::other)?,
                _library: lib,
            })
        }
    }

    fn check(&self, code: i32) -> io::Result<()> {
        if code == 0 {
            return Ok(());
        }
        let msg = unsafe { CStr::from_ptr((self.error)(code)) }.to_string_lossy();
        Err(io::Error::other(format!("Btrfs metadata: {msg}")))
    }

    fn info(&self, dir: &File) -> io::Result<Info> {
        let mut out = MaybeUninit::uninit();
        self.check(unsafe { (self.info)(dir.as_raw_fd(), 0, out.as_mut_ptr()) })?;
        Ok(unsafe { out.assume_init() })
    }

    fn containing_id(&self, dir: &File) -> io::Result<u64> {
        let mut id = 0;
        self.check(unsafe { (self.id)(dir.as_raw_fd(), &mut id) })?;
        Ok(id)
    }

    fn descendants(&self, dir: &File, mut visit: impl FnMut(PathBuf, u64)) -> io::Result<()> {
        let mut iter = std::ptr::null_mut();
        self.check(unsafe { (self.create)(dir.as_raw_fd(), 0, 0, &mut iter) })?;
        let result = loop {
            let mut name = std::ptr::null_mut();
            let mut id = 0;
            let code = unsafe { (self.next)(iter, &mut name, &mut id) };
            if code == 1 {
                break Ok(());
            } // BTRFS_UTIL_ERROR_STOP_ITERATION
            if code != 0 {
                break self.check(code);
            }
            let bytes = unsafe { CStr::from_ptr(name) }.to_bytes().to_owned();
            unsafe { libc::free(name.cast()) };
            visit(OsString::from_vec(bytes).into(), id);
        };
        unsafe { (self.destroy)(iter) };
        result
    }
}

fn directory(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

fn is_btrfs(fd: &File) -> io::Result<bool> {
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(fd.as_raw_fd(), stat.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() }.f_type == libc::BTRFS_SUPER_MAGIC)
}

// Linux btrfs.h: btrfs_ioctl_get_subvol_info_args. Only parent_uuid is used;
// reserve the remainder of the UAPI structure so the ioctl request has the
// native 64-bit ABI size on x86_64 and aarch64.
#[repr(C)]
struct SubvolInfoArgs {
    treeid: u64,
    name: [u8; 256],
    parent_dir_generation_flags: [u64; 4],
    uuid: Uuid,
    parent_uuid: Uuid,
    received_transactions_times_reserved: [u8; 176],
}
const _: () = assert!(size_of::<SubvolInfoArgs>() == 504);

/// A watcher opened an O_PATH descriptor. Check only possible subvolume roots
/// (inode 256), then reopen the same directory for the read-only kernel ioctl.
pub(super) fn nested_snapshot(dir: &File, metadata: &fs::Metadata) -> io::Result<bool> {
    if metadata.ino() != 256 || !is_btrfs(dir)? {
        return Ok(false);
    }
    let fd = File::open(format!("/proc/self/fd/{}", dir.as_raw_fd()))?;
    let mut info = MaybeUninit::<SubvolInfoArgs>::zeroed();
    let request = libc::_IOR::<SubvolInfoArgs>(0x94, 60);
    if unsafe { libc::ioctl(fd.as_raw_fd(), request, info.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.assume_init() }.parent_uuid != [0; 16])
}

// The UAPI reserves a full 1 KiB. FS_INFO returns the filesystem UUID at byte
// 16, after max_id and num_devices. st_dev alone differs across subvolumes.
fn filesystem(dir: &File) -> io::Result<Option<Uuid>> {
    if !is_btrfs(dir)? {
        return Ok(None);
    }
    let mut data = [0u8; 1024];
    let request = libc::_IOR::<[u8; 1024]>(0x94, 31);
    if unsafe { libc::ioctl(dir.as_raw_fd(), request, data.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(data[16..32].try_into().unwrap()))
}

fn mount_id(dir: &File) -> io::Result<u64> {
    let mut stat = MaybeUninit::uninit();
    if unsafe {
        libc::statx(
            dir.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_MNT_ID,
            stat.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() }.stx_mnt_id)
}

// Resolve the existing part while keeping a deleted suffix and treating a
// final symlink as the entry in its containing folder, including dangling links.
fn location(path: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let path = std::path::absolute(path)?;
    for dir in path.ancestors() {
        let meta = match fs::symlink_metadata(dir) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if !meta.is_dir() {
            if dir == path {
                continue;
            }
            // For existing symlink ancestors, determine their actual source mount.
            if !meta.is_symlink() || !fs::metadata(dir)?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "A parent is not a directory",
                ));
            }
        }
        let root = dir.canonicalize()?;
        let suffix = path.strip_prefix(dir).unwrap();
        if suffix
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(io::Error::other("Could not resolve the missing path"));
        }
        return Ok((root.clone(), root.join(suffix)));
    }
    Err(io::Error::other("No accessible containing directory"))
}

fn beneath(dir: &File, relative: &Path, directory_only: bool) -> io::Result<File> {
    let raw = if relative.as_os_str().is_empty() {
        c".".into()
    } else {
        CString::new(relative.as_os_str().as_bytes())?
    };
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (if directory_only {
        libc::O_DIRECTORY | libc::O_RDONLY
    } else {
        libc::O_PATH | libc::O_NOFOLLOW
    } | libc::O_CLOEXEC) as u64;
    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
    if !directory_only {
        how.resolve |= libc::RESOLVE_NO_XDEV;
    }
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir.as_raw_fd(),
            raw.as_ptr(),
            &how,
            size_of::<libc::open_how>(),
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd as i32) })
    }
}

fn checkpoint_path(dir: &File, root: &Path, relative: &Path) -> io::Result<Option<PathBuf>> {
    match beneath(dir, relative, false) {
        Ok(file) => {
            let meta = file.metadata()?;
            // An unmounted nested subvolume still has a distinct device, while
            // its empty placeholder in a snapshot is inode 2.
            Ok((meta.dev() == dir.metadata()?.dev() && meta.ino() != 2)
                .then(|| root.join(relative)))
        }
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP | libc::EXDEV)
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

struct Candidate {
    path: PathBuf,
    tree_path: PathBuf,
    info: Info,
}

fn date(sec: i64) -> String {
    if sec == 0 {
        return "Unknown".into();
    }
    let mut tm = MaybeUninit::uninit();
    if unsafe { libc::gmtime_r(&sec, tm.as_mut_ptr()) }.is_null() {
        return "Unknown".into();
    }
    let tm = unsafe { tm.assume_init() };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

pub fn discover(path: &Path) -> io::Result<Report> {
    let (anchor, requested) = location(path)?;
    let here = directory(&anchor)?;
    let source_mount = mount_id(&here)?;
    if requested != anchor {
        match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
            .open(&requested)
        {
            Ok(leaf) if mount_id(&leaf)? != source_mount => {
                return Err(io::Error::other(
                    "This file has a separate mount; its path in the source subvolume is unavailable",
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    let Some(fsid) = filesystem(&here)? else {
        return Ok(Report::default());
    };
    let util = Util::open()?;
    let live_id = util.containing_id(&here)?;
    let mounts = scan::mounts()?;
    let mount = mounts
        .iter()
        .find(|m| m.id == source_mount)
        .ok_or_else(|| io::Error::other("The mount changed during discovery"))?;
    let tree_path = mount.root.join(
        requested
            .strip_prefix(&mount.path)
            .map_err(io::Error::other)?,
    );

    let mut candidates = Vec::new();
    let mut limited = false;
    let mut top = false;
    for m in mounts.iter().filter(|m| m.fs_type == "btrfs") {
        let Ok(dir) = directory(&m.path) else {
            limited = true;
            continue;
        };
        let (Ok(actual_mount), Ok(filesystem_id), Ok(metadata)) =
            (mount_id(&dir), filesystem(&dir), dir.metadata())
        else {
            limited = true;
            continue;
        };
        if actual_mount != m.id {
            limited = true;
            continue;
        }
        // A bind mount rooted below a subvolume cannot enumerate the parent root.
        if filesystem_id != Some(fsid) || metadata.ino() != 256 {
            continue;
        }
        let Ok(info) = util.info(&dir) else {
            limited = true;
            continue;
        };
        top |= info.id == 5;
        candidates.push(Candidate {
            path: m.path.clone(),
            tree_path: m.root.clone(),
            info,
        });
        let result = util.descendants(&dir, |rel, id| {
            let candidate = (|| -> io::Result<_> {
                let fd = beneath(&dir, &rel, true)?;
                let actual = util.info(&fd)?;
                // Recheck visible identity: another filesystem may be mounted
                // over the pathname reported by a privileged traversal.
                if actual.id != id || filesystem(&fd)? != Some(fsid) {
                    return Err(io::Error::other("Subvolume path is obscured"));
                }
                Ok(Candidate {
                    path: m.path.join(&rel),
                    tree_path: m.root.join(rel),
                    info: actual,
                })
            })();
            match candidate {
                Ok(c) => candidates.push(c),
                Err(_) => limited = true,
            }
        });
        if result.is_err() {
            limited = true;
        }
    }

    // Iterators can skip a restricted ancestor even if the caller can open a
    // specific live subvolume by name. Locate it from the selected path as well.
    for ancestor in anchor
        .ancestors()
        .take_while(|p| p.starts_with(&mount.path))
    {
        let Ok(dir) = directory(ancestor) else {
            continue;
        };
        if dir.metadata().is_ok_and(|m| m.ino() == 256)
            && let Ok(info) = util.info(&dir)
            && info.id == live_id
        {
            candidates.insert(
                0,
                Candidate {
                    path: ancestor.to_owned(),
                    tree_path: mount.root.join(ancestor.strip_prefix(&mount.path).unwrap()),
                    info,
                },
            );
            break;
        }
    }
    let source = candidates
        .iter()
        .find(|c| c.info.id == live_id && tree_path.starts_with(&c.tree_path))
        .ok_or_else(|| {
            io::Error::other("The source subvolume root is outside the accessible mounts")
        })?;
    let relative = tree_path.strip_prefix(&source.tree_path).unwrap();
    let mut checkpoints = BTreeMap::new();
    for c in &candidates {
        if c.info.parent_uuid == [0; 16] || c.info.parent_uuid != source.info.uuid {
            continue;
        }
        let dir = match directory(&c.path) {
            Ok(f) => f,
            Err(_) => {
                limited = true;
                continue;
            }
        };
        if !filesystem(&dir).is_ok_and(|id| id == Some(fsid))
            || !util.info(&dir).is_ok_and(|i| i.uuid == c.info.uuid)
        {
            limited = true;
            continue;
        }
        let file = match checkpoint_path(&dir, &c.path, relative) {
            Ok(file) => file,
            Err(_) => {
                limited = true;
                None
            }
        };
        let time = (c.info.otime.tv_sec, c.info.otime.tv_nsec);
        let row = Checkpoint {
            root: c.path.clone(),
            date: date(time.0),
            readonly: c.info.flags & 1 != 0,
            file,
        };
        match checkpoints.entry(c.info.uuid) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert((time, row));
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if slot.get().1.file.is_none() && row.file.is_some() {
                    slot.insert((time, row));
                }
            }
        }
    }
    let mut ordered: Vec<_> = checkpoints.into_values().collect();
    ordered.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.root.cmp(&b.1.root)));
    // The unprivileged iterator silently skips descendants denied by the VFS.
    let sys_admin = fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("CapEff:"))
                .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        })
        .is_some_and(|caps| caps & (1 << 21) != 0);
    Ok(Report {
        source: Some(source.path.clone()),
        checkpoints: ordered.into_iter().map(|(_, s)| s).collect(),
        limited: limited || !top || !sys_admin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn resolves_missing_suffix_and_preserves_a_final_link() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let missing = root.join("former folder/deleted.txt");
        assert_eq!(location(&missing).unwrap(), (root.clone(), missing));
        let link = root.join("link");
        symlink("/does/not/exist", &link).unwrap();
        assert_eq!(location(&link).unwrap(), (root, link));
    }

    #[test]
    fn checkpoint_paths_keep_symlink_entries_without_following_ancestor_links() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("snapshot");
        fs::create_dir(&root).unwrap();
        fs::write(fixture.path().join("outside"), b"outside").unwrap();
        let dir = directory(&root).unwrap();
        symlink(fixture.path(), root.join("link")).unwrap();
        assert_eq!(
            checkpoint_path(&dir, &root, Path::new("link")).unwrap(),
            Some(root.join("link"))
        );
        for relative in ["link/outside", "../outside", "absent"] {
            assert!(
                checkpoint_path(&dir, &root, Path::new(relative))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn non_btrfs_discovery_needs_no_snapshot_or_history_database() {
        let dir = directory(Path::new("/proc")).unwrap();
        assert!(filesystem(&dir).unwrap().is_none());
        let report = discover(Path::new("/proc")).unwrap();
        assert!(report.source.is_none() && report.checkpoints.is_empty());
    }
}
