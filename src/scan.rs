use std::collections::HashSet;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, Metadata, ReadDir};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Node {
    pub name: OsString,
    pub bytes: u64,
    pub is_dir: bool,
    /// The directory itself or an immediate entry could not be read.
    pub incomplete: bool,
    pub identity: (u64, u64, i64, i64),
    pub children: Vec<usize>,
    pub parent: Option<usize>,
}

#[derive(Debug)]
pub struct Scan {
    pub root: PathBuf,
    pub nodes: Vec<Node>,
    /// Number of directory entries or listings we could not read.
    pub skipped: u64,
}

impl Scan {
    pub fn path(&self, mut id: usize) -> PathBuf {
        let mut components = Vec::new();
        while let Some(parent) = self.nodes[id].parent {
            components.push(&self.nodes[id].name);
            id = parent;
        }
        let mut path = self.root.clone();
        for component in components.into_iter().rev() {
            path.push(component);
        }
        path
    }
}

#[derive(Clone, Debug)]
pub struct Volume {
    pub name: String,
    pub path: PathBuf,
    pub total: u64,
    pub free: u64,
    pub fs_type: String,
}

struct Mount {
    id: u64,
    path: PathBuf,
    root: PathBuf,
    fs_type: String,
    source: OsString,
    device: OsString,
}

// Linux octal-escapes whitespace and backslashes in /proc/self/mountinfo.
fn unescape(field: &[u8]) -> OsString {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\' && i + 3 < field.len() {
            let octal = &field[i + 1..i + 4];
            if octal[0] <= b'3' && octal.iter().all(|x| (b'0'..=b'7').contains(x)) {
                out.push((octal[0] - b'0') * 64 + (octal[1] - b'0') * 8 + (octal[2] - b'0'));
                i += 4;
                continue;
            }
        }
        out.push(field[i]);
        i += 1;
    }
    OsString::from_vec(out)
}

fn parse_mount(line: &[u8]) -> Option<Mount> {
    let mut fields = line.split(|c| *c == b' ');
    let id = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    let device = unescape(fields.nth(1)?);
    let root = PathBuf::from(unescape(fields.next()?));
    let path = PathBuf::from(unescape(fields.next()?));
    fields.find(|field| *field == b"-")?;
    let fs_type = String::from_utf8_lossy(fields.next()?).into_owned();
    let source = unescape(fields.next()?);
    Some(Mount {
        id,
        path,
        root,
        fs_type,
        source,
        device,
    })
}

fn mounts() -> io::Result<Vec<Mount>> {
    let data = fs::read("/proc/self/mountinfo")?;
    let mut table: Vec<_> = data
        .split(|c| *c == b'\n')
        .filter_map(parse_mount)
        .collect();
    table.sort_by_key(|m| m.path.components().count());
    Ok(table)
}

/// A symlink itself is a separate directory entry. Resolve its parent but leave
/// the final component untouched so this guard describes the item being moved.
pub fn contains_mount(path: &Path) -> io::Result<bool> {
    if path == Path::new("/") {
        return Ok(true);
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Expected a named file or folder",
        )
    })?;
    let resolved = parent.canonicalize()?.join(name);
    Ok(mounts()?.iter().any(|m| m.path.starts_with(&resolved)))
}

fn same_device(a: &Mount, b: &Mount) -> bool {
    a.device == b.device || (a.fs_type == "btrfs" && b.fs_type == "btrfs" && a.source == b.source)
}

fn storage(m: &Mount) -> bool {
    m.path == Path::new("/")
        || m.source.as_bytes().starts_with(b"/dev/")
        || matches!(
            m.fs_type.as_str(),
            "nfs" | "nfs4" | "cifs" | "smb3" | "ceph" | "zfs"
        )
        || (m.fs_type.starts_with("fuse.")
            && !matches!(m.fs_type.as_str(), "fuse.gvfsd-fuse" | "fuse.portal"))
}

fn capacity(path: &Path) -> io::Result<(u64, u64)> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stats = MaybeUninit::<libc::statvfs>::uninit();
    // Both pointers remain valid for the duration of statvfs.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // A successful statvfs initialized the structure.
    let stats = unsafe { stats.assume_init() };
    Ok((
        stats.f_blocks.saturating_mul(stats.f_frsize),
        stats.f_bavail.saturating_mul(stats.f_frsize),
    ))
}

pub fn volumes() -> io::Result<Vec<Volume>> {
    let table = mounts()?;
    let mut shown: Vec<&Mount> = Vec::new();
    let mut volumes = Vec::new();
    for m in table.iter().filter(|m| storage(m)) {
        if shown
            .iter()
            .any(|prior| same_device(prior, m) && m.path.starts_with(&prior.path))
        {
            continue;
        }
        let (total, free) = match capacity(&m.path) {
            Ok(sizes) => sizes,
            Err(e) if m.path == Path::new("/") => return Err(e),
            Err(_) => continue, // Removable media can disappear during enumeration.
        };
        volumes.push(Volume {
            name: m
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "System disk".to_string()),
            path: m.path.clone(),
            total,
            free,
            fs_type: m.fs_type.clone(),
        });
        shown.push(m);
    }
    volumes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(volumes)
}

struct Boundaries {
    allowed_ids: HashSet<u64>,
    excluded: HashSet<PathBuf>,
    btrfs: bool,
}

fn boundaries(root: &Path, table: &[Mount]) -> io::Result<Boundaries> {
    let base = table
        .iter()
        .rev()
        .find(|m| root.starts_with(&m.path))
        .ok_or_else(|| io::Error::other("Could not identify the filesystem for this folder"))?;
    let btrfs = base.fs_type == "btrfs";
    let mut excluded = HashSet::new();
    let mut allowed_ids = HashSet::from([base.id]);
    let mut included = vec![base.root.join(root.strip_prefix(&base.path).unwrap())];
    for m in table
        .iter()
        .filter(|m| m.path != root && m.path.starts_with(root))
    {
        // Same-disk Btrfs subvolumes such as Omarchy's /home and /var/log
        // have distinct st_dev values. Include their own trees once.
        if btrfs
            && same_device(base, m)
            && !included
                .iter()
                .any(|p| m.root.starts_with(p) || p.starts_with(&m.root))
            && !excluded.iter().any(|p: &PathBuf| m.path.starts_with(p))
        {
            included.push(m.root.clone());
            allowed_ids.insert(m.id);
        } else {
            excluded.insert(m.path.clone());
        }
    }
    Ok(Boundaries {
        allowed_ids,
        excluded,
        btrfs,
    })
}

/// Check nested mount boundaries before inflating a potentially large ancestor snapshot.
pub fn can_cover(root: &Path, requested: &Path) -> bool {
    root == requested
        || (requested.starts_with(root)
            && mounts()
                .and_then(|table| boundaries(root, &table))
                .is_ok_and(|bounds| {
                    !requested
                        .ancestors()
                        .any(|path| bounds.excluded.contains(path))
                }))
}

fn node(
    name: OsString,
    metadata: &Metadata,
    parent: Option<usize>,
    links: &mut HashSet<(u64, u64)>,
) -> Node {
    let is_dir = metadata.is_dir();
    let counted_elsewhere =
        !is_dir && metadata.nlink() > 1 && !links.insert((metadata.dev(), metadata.ino()));
    Node {
        name,
        parent,
        is_dir,
        incomplete: false,
        identity: (
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ),
        bytes: if counted_elsewhere {
            0
        } else {
            metadata.blocks().saturating_mul(512)
        },
        children: Vec::new(),
    }
}

struct Frame {
    dir: usize,
    entries: ReadDir,
    directory: File,
}

impl Frame {
    fn new(dir: usize, directory: File) -> io::Result<Self> {
        let entries = fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))?;
        Ok(Self {
            dir,
            entries,
            directory,
        })
    }

    fn child(
        &self,
        id: usize,
        name: &OsStr,
        metadata: &Metadata,
        boundaries: &Boundaries,
    ) -> io::Result<Self> {
        let name = CString::new(name.as_bytes())?;
        // Resolve only the final component inside the still-open parent. A directory
        // renamed to a symlink during the scan cannot redirect us into its target.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // openat returned a new descriptor whose ownership passes to File.
        let directory = unsafe { File::from_raw_fd(fd) };
        let mut stat = MaybeUninit::<libc::statx>::uninit();
        // statx identifies bind mounts even when their device and inode are shared.
        // Checking the descriptor catches new mounts attached after we read mountinfo.
        if unsafe {
            libc::statx(
                fd,
                c"".as_ptr(),
                libc::AT_EMPTY_PATH,
                libc::STATX_INO | libc::STATX_MNT_ID,
                stat.as_mut_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let opened = unsafe { stat.assume_init() };
        if opened.stx_mask & libc::STATX_MNT_ID != 0
            && !boundaries.allowed_ids.contains(&opened.stx_mnt_id)
        {
            return Err(io::Error::other("Mount changed during the scan"));
        }
        let device = libc::makedev(opened.stx_dev_major, opened.stx_dev_minor);
        if (device, opened.stx_ino) != (metadata.dev(), metadata.ino()) {
            return Err(io::Error::other("Directory changed during the scan"));
        }
        Self::new(id, directory)
    }
}

pub fn scan(
    root: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, &Path),
) -> io::Result<Scan> {
    let root = root.canonicalize()?;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&root)?;
    let metadata = directory.metadata()?;
    let device = metadata.dev();
    let mut links = HashSet::new();
    let mut nodes = vec![node(
        root.file_name().unwrap_or(OsStr::new("/")).to_owned(),
        &metadata,
        None,
        &mut links,
    )];
    let boundaries = boundaries(&root, &mounts()?)?;
    let mut stack = vec![Frame::new(0, directory)?];
    let mut current = root.clone();
    let mut last_update = Instant::now();
    let mut skipped = 0;
    progress(1, &root);

    while let Some(frame) = stack.last_mut() {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Scan cancelled"));
        }
        let parent = frame.dir;
        let entry = match frame.entries.next() {
            Some(Ok(entry)) => entry,
            Some(Err(_)) => {
                nodes[parent].incomplete = true;
                skipped += 1;
                continue;
            }
            None => {
                // Child sizes are final once the directory iterator is exhausted.
                let mut children = std::mem::take(&mut nodes[parent].children);
                children.sort_unstable_by(|&a, &b| {
                    nodes[b]
                        .bytes
                        .cmp(&nodes[a].bytes)
                        .then_with(|| nodes[a].name.cmp(&nodes[b].name))
                });
                nodes[parent].children = children;
                if let Some(ancestor) = nodes[parent].parent {
                    nodes[ancestor].bytes =
                        nodes[ancestor].bytes.saturating_add(nodes[parent].bytes);
                }
                current.pop();
                stack.pop();
                continue;
            }
        };
        let name = entry.file_name();
        current.push(&name);
        if !boundaries.excluded.contains(&current) {
            match entry.metadata() {
                Ok(metadata) if boundaries.btrfs || metadata.dev() == device => {
                    let child = nodes.len();
                    let is_dir = metadata.is_dir();
                    nodes.push(node(name, &metadata, Some(parent), &mut links));
                    nodes[parent].children.push(child);
                    if last_update.elapsed() >= Duration::from_millis(80) {
                        progress(nodes.len() as u64, &current);
                        last_update = Instant::now();
                    }
                    if is_dir {
                        match frame.child(child, &nodes[child].name, &metadata, &boundaries) {
                            Ok(frame) => {
                                stack.push(frame);
                                continue;
                            }
                            Err(_) => {
                                nodes[child].incomplete = true;
                                skipped += 1;
                            }
                        }
                    }
                    nodes[parent].bytes = nodes[parent].bytes.saturating_add(nodes[child].bytes);
                }
                Ok(_) => {}
                Err(_) => {
                    nodes[parent].incomplete = true;
                    skipped += 1;
                }
            }
        }
        current.pop();
    }
    progress(nodes.len() as u64, &root);
    Ok(Scan {
        root,
        nodes,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "corolla-scan-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn accounts_for_allocated_blocks_links_and_raw_names() {
        let temp = Fixture::new();
        let dir = temp.0.join("nested");
        fs::create_dir(&dir).unwrap();
        let unusual_name = OsString::from_vec(vec![b'x', 0xff]);
        let file = dir.join(&unusual_name);
        fs::write(&file, [0x36; 32 * 1024]).unwrap();
        fs::hard_link(&file, temp.0.join("hard link")).unwrap();
        symlink(&temp.0, dir.join("cycle")).unwrap();
        File::create(dir.join("sparse"))
            .unwrap()
            .set_len(1 << 30)
            .unwrap();

        let scanned = scan(&temp.0, &AtomicBool::new(false), |_, _| {}).unwrap();
        let du = Command::new("du")
            .args(["-s", "-B1"])
            .arg(&temp.0)
            .output()
            .unwrap();
        assert!(du.status.success());
        let bytes = std::str::from_utf8(&du.stdout)
            .unwrap()
            .split('\t')
            .next()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert_eq!(scanned.nodes[0].bytes, bytes);
        assert_eq!(scanned.skipped, 0);
        assert_eq!(scanned.nodes.len(), 6);
        for (id, node) in scanned.nodes.iter().enumerate() {
            assert!(
                node.children
                    .windows(2)
                    .all(|ids| scanned.nodes[ids[0]].bytes >= scanned.nodes[ids[1]].bytes)
            );
            if node.name == unusual_name {
                assert_eq!(scanned.path(id), file);
            }
            if node.name == "cycle" {
                assert!(!node.is_dir);
            }
        }
    }

    #[test]
    fn cancellation_returns_interrupted_without_partial_results() {
        let temp = Fixture::new();
        let cancel = AtomicBool::new(false);
        let result = scan(&temp.0, &cancel, |_, _| {
            cancel.store(true, Ordering::Relaxed)
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn includes_omarchy_subvolumes_and_excludes_external_and_bind_mounts() {
        let table: Vec<_> = [
            &b"1 0 0:1 /@ / rw - btrfs /dev/test rw"[..],
            b"2 1 0:2 /@home /home rw - btrfs /dev/test rw",
            b"3 1 0:3 / /mnt/Backup\\040Disk rw - ext4 /dev/backup rw",
            b"4 1 0:2 /@home /mirror rw - btrfs /dev/test rw",
            b"5 1 0:1 /@/usr /mnt/usr rw - btrfs /dev/test rw",
        ]
        .iter()
        .map(|line| parse_mount(line).unwrap())
        .collect();
        let b = boundaries(Path::new("/"), &table).unwrap();
        assert_eq!(b.allowed_ids, HashSet::from([1, 2]));
        assert!(!b.excluded.contains(Path::new("/home")));
        assert!(b.excluded.contains(Path::new("/mnt/Backup Disk")));
        assert!(b.excluded.contains(Path::new("/mirror")));
        assert!(b.excluded.contains(Path::new("/mnt/usr")));
    }
}
