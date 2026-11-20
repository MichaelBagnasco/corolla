//! Linux directory notifications are hints. Each commit reconciles the affected
//! paths against their current metadata, including both ends of a rename.
use super::store::Store;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub kind: Kind,
    pub bytes: u64,
    pub size: u64,
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

#[derive(Debug)]
pub struct Batch {
    pub root: PathBuf,
    pub scopes: Vec<PathBuf>,
    pub entries: Vec<Entry>,
    pub preserved: Vec<PathBuf>,
}

const QUIET: Duration = Duration::from_millis(350);
const MAX_DELAY: Duration = Duration::from_secs(2);
const RECONCILE: Duration = Duration::from_secs(15 * 60);
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn signals() -> io::Result<()> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = stop as *const () as usize;
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // The handler only sets a native atomic. poll notices the signal or
        // wakes on its short timeout, and the main loop flushes the current batch.
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn raw(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)
}

// Resolve from the canonical absolute path without crossing symlink ancestors.
// O_PATH|O_NOFOLLOW still opens a final symlink itself for metadata.
fn open_entry(path: &Path) -> io::Result<File> {
    let path = raw(path)?;
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) as u64;
    how.resolve = libc::RESOLVE_NO_SYMLINKS;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            size_of::<libc::open_how>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

fn mount_id(file: &File) -> io::Result<u64> {
    let mut stat = MaybeUninit::<libc::statx>::uninit();
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
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

struct Watcher {
    root: PathBuf,
    database_parent: PathBuf,
    ignored: BTreeSet<PathBuf>,
    fd: OwnedFd,
    names: HashMap<i32, PathBuf>,
    watches: BTreeMap<PathBuf, i32>,
    anchor: Option<PathBuf>,
}

fn missing(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP | libc::ESTALE)
    )
}

impl Watcher {
    fn new(root: &Path, database: &Path) -> io::Result<Self> {
        let database_parent = database
            .parent()
            .ok_or_else(|| io::Error::other("Database needs a parent directory"))?
            .to_owned();
        let name = database
            .file_name()
            .ok_or_else(|| io::Error::other("Database needs a filename"))?;
        let mut ignored = BTreeSet::from([database.to_owned()]);
        for (prefix, suffix) in [
            (".", "-lock"),
            ("", ".watch-lock"),
            ("", "-wal"),
            ("", "-shm"),
            ("", "-journal"),
        ] {
            let mut sidecar = OsString::from(prefix);
            sidecar.push(name);
            sidecar.push(suffix);
            ignored.insert(database_parent.join(sidecar));
        }
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            root: root.to_owned(),
            database_parent,
            ignored,
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            names: HashMap::new(),
            watches: BTreeMap::new(),
            anchor: None,
        })
    }

    fn watch(&mut self, path: &Path, dir: &File) -> io::Result<()> {
        let fd_path = CString::new(format!("/proc/self/fd/{}", dir.as_raw_fd())).unwrap();
        let mask = libc::IN_ONLYDIR
            | libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_CLOSE_WRITE
            | libc::IN_MODIFY
            | libc::IN_ATTRIB
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF
            | libc::IN_EXCL_UNLINK;
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), fd_path.as_ptr(), mask) };
        if wd < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENOSPC) {
                return Err(io::Error::other(format!(
                    "Inotify watch limit reached at {}. Choose a smaller directory or raise fs.inotify.max_user_watches.",
                    path.display()
                )));
            }
            return Err(e);
        }
        if let Some(old) = self.names.insert(wd, path.to_owned()) {
            self.watches.remove(&old);
        }
        if let Some(old) = self.watches.insert(path.to_owned(), wd)
            && old != wd
        {
            self.names.remove(&old);
            unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), old) };
        }
        Ok(())
    }

    fn forget(&mut self, path: &Path) {
        if let Some(wd) = self.watches.remove(path) {
            self.names.remove(&wd);
            unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
        }
    }

    fn watch_parent(&mut self) -> io::Result<()> {
        // Stay subscribed to the nearest surviving ancestor so a deleted, moved
        // or later recreated root can be reconciled using the same database.
        let root = self.root.clone();
        for path in root.ancestors().skip(1) {
            let file = match open_entry(path) {
                Ok(f) if f.metadata()?.is_dir() => f,
                Ok(_) => continue,
                Err(e) if missing(&e) || e.kind() == io::ErrorKind::PermissionDenied => continue,
                Err(e) => return Err(e),
            };
            if self.anchor.as_deref() != Some(path)
                && let Some(old) = self.anchor.take()
            {
                self.forget(&old);
            }
            self.watch(path, &file)?;
            self.anchor = Some(path.to_owned());
            break;
        }
        Ok(())
    }

    fn add_row(&self, batch: &mut Batch, path: PathBuf, meta: &fs::Metadata) {
        let kind = if meta.is_dir() {
            Kind::Directory
        } else if meta.is_file() {
            Kind::File
        } else if meta.is_symlink() {
            Kind::Symlink
        } else {
            Kind::Other
        };
        let timestamp = |s: i64, ns: i64| s.saturating_mul(1_000_000_000).saturating_add(ns);
        // DoltLite creates and removes a lock in its parent directory. Ignore
        // those volatile directory attributes when the DB is inside the tree.
        let own_parent = path == self.database_parent;
        batch.entries.push(Entry {
            path,
            kind,
            bytes: if own_parent {
                0
            } else {
                meta.blocks().saturating_mul(512)
            },
            size: if own_parent { 0 } else { meta.size() },
            dev: meta.dev(),
            ino: meta.ino(),
            mode: meta.mode(),
            mtime_ns: if own_parent {
                0
            } else {
                timestamp(meta.mtime(), meta.mtime_nsec())
            },
            ctime_ns: if own_parent {
                0
            } else {
                timestamp(meta.ctime(), meta.ctime_nsec())
            },
        });
    }

    fn collect(&mut self, paths: &BTreeSet<PathBuf>) -> io::Result<Batch> {
        self.watch_parent()?;
        let mut batch = Batch {
            root: self.root.clone(),
            scopes: Vec::new(),
            entries: Vec::new(),
            preserved: Vec::new(),
        };
        for path in paths {
            if !path.starts_with(&self.root) || self.ignored.contains(path) {
                continue;
            }
            if batch
                .scopes
                .last()
                .is_none_or(|ancestor| !path.starts_with(ancestor))
            {
                batch.scopes.push(path.clone());
            }
        }
        let root_mount = match open_entry(&self.root) {
            Ok(f) => Some(mount_id(&f)?),
            Err(e) if missing(&e) => None,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                batch.preserved.push(self.root.clone());
                return Ok(batch);
            }
            Err(e) => return Err(e),
        };
        let mut parents = BTreeSet::new();
        for scope in batch.scopes.clone() {
            // Reconcile subscriptions at the same time as data. Watches tied to
            // moved inodes must no longer describe their previous paths.
            let mut stale: BTreeSet<_> = self
                .watches
                .range(scope.clone()..)
                .take_while(|(p, _)| p.starts_with(&scope))
                .map(|(p, _)| p.clone())
                .collect();
            if scope != self.root
                && let Some(p) = scope.parent()
            {
                parents.insert(p.to_owned());
            }
            let mut pending = vec![scope];
            while let Some(path) = pending.pop() {
                if self.ignored.contains(&path) {
                    continue;
                }
                let result = (|| -> io::Result<()> {
                    let file = open_entry(&path)?;
                    if Some(mount_id(&file)?) != root_mount {
                        return Ok(());
                    }
                    let meta = file.metadata()?;
                    // Stat can succeed even when listing fails. Keep the known
                    // directory metadata and preserve its unobservable descendants.
                    self.add_row(&mut batch, path.clone(), &meta);
                    if meta.is_dir() {
                        self.watch(&path, &file)?;
                        stale.remove(&path);
                        let entries = fs::read_dir(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
                        for e in entries {
                            pending.push(path.join(e?.file_name()));
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    if missing(&e) {
                        continue;
                    }
                    if e.kind() == io::ErrorKind::PermissionDenied {
                        batch.preserved.push(path);
                    } else {
                        return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display())));
                    }
                }
            }
            for path in stale {
                self.forget(&path);
            }
        }
        for path in parents {
            if let Ok(file) = open_entry(&path)
                && Some(mount_id(&file)?) == root_mount
                && let Ok(meta) = file.metadata()
            {
                self.add_row(&mut batch, path, &meta);
            }
        }
        Ok(batch)
    }

    // Read a bounded number of kernel buffers so continuous writes cannot
    // starve the maximum-delay flush or the termination check.
    fn events(&mut self, dirty: &mut BTreeSet<PathBuf>, timeout: Duration) -> io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe {
            libc::poll(
                &mut poll,
                1,
                timeout.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(e);
        }
        if ready == 0 {
            return Ok(false);
        }
        let mut changed = false;
        let mut buffer = [0u8; 64 * 1024];
        for _ in 0..8 {
            let count = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if count < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if count == 0 {
                return Err(io::Error::other("Inotify descriptor closed"));
            }
            let mut pos = 0;
            while pos < count as usize {
                let e = unsafe {
                    buffer
                        .as_ptr()
                        .add(pos)
                        .cast::<libc::inotify_event>()
                        .read_unaligned()
                };
                pos += size_of::<libc::inotify_event>();
                let names = &buffer[pos..pos + e.len as usize];
                pos += e.len as usize;
                if e.mask & libc::IN_Q_OVERFLOW != 0 {
                    dirty.clear();
                    dirty.insert(self.root.clone());
                    changed = true;
                    continue;
                }
                let Some(base) = self.names.get(&e.wd) else {
                    continue;
                };
                if e.mask & libc::IN_IGNORED != 0 {
                    let old = self.names.remove(&e.wd).unwrap();
                    self.watches.remove(&old);
                    continue;
                }
                let n = names.iter().position(|c| *c == 0).unwrap_or(names.len());
                let path = if n == 0 {
                    base.clone()
                } else {
                    base.join(OsStr::from_bytes(&names[..n]))
                };
                let scope = if path.starts_with(&self.root) {
                    path
                } else if self.root.starts_with(&path) {
                    self.root.clone()
                } else {
                    continue;
                };
                if self.ignored.contains(&scope) {
                    continue;
                }
                changed = true;
                if dirty.len() >= 4096 {
                    dirty.clear();
                    dirty.insert(self.root.clone());
                }
                if !dirty.contains(&self.root) {
                    dirty.insert(scope);
                }
            }
        }
        Ok(changed)
    }
}

pub fn run(root: &Path, database: &Path) -> io::Result<()> {
    STOP.store(false, Ordering::Relaxed);
    signals()?;
    let mut watcher = Watcher::new(root, database)?;
    let store = Store::open(database, root)?;
    let all = BTreeSet::from([root.to_owned()]);
    store.record(&watcher.collect(&all)?, "Reconcile watched directory")?;
    eprintln!("Watching {} for changes", root.display());
    let mut dirty = BTreeSet::new();
    let mut first: Option<Instant> = None;
    let mut last = Instant::now();
    let mut full = last;
    loop {
        let mut timeout = RECONCILE
            .saturating_sub(full.elapsed())
            .min(Duration::from_secs(1));
        if let Some(start) = first {
            timeout = timeout
                .min(QUIET.saturating_sub(last.elapsed()))
                .min(MAX_DELAY.saturating_sub(start.elapsed()));
        }
        if STOP.load(Ordering::Relaxed) {
            timeout = Duration::ZERO;
        }
        let changed = watcher.events(&mut dirty, timeout)?;
        let now = Instant::now();
        if changed {
            first.get_or_insert(now);
            last = now;
        }
        let stopping = STOP.load(Ordering::Relaxed);
        if stopping {
            // poll may have been interrupted before it read already queued writes.
            // Cover even an unusually deep queue while bounding work on shutdown.
            for i in 0..4 {
                if !watcher.events(&mut dirty, Duration::ZERO)? {
                    break;
                }
                if i == 3 {
                    dirty = all.clone();
                }
            }
        }
        let periodic = full.elapsed() >= RECONCILE;
        if periodic {
            dirty = all.clone();
        }
        let due = stopping
            || periodic
            || last.elapsed() >= QUIET
            || first.is_some_and(|start: Instant| start.elapsed() >= MAX_DELAY);
        if !dirty.is_empty() && due {
            if dirty.contains(root) {
                full = now;
            }
            let batch = watcher.collect(&dirty)?;
            store.record(
                &batch,
                if periodic {
                    "Reconcile watched directory"
                } else {
                    "Record filesystem changes"
                },
            )?;
            dirty.clear();
            first = None;
        }
        if stopping {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{
        ffi::OsStringExt,
        fs::{PermissionsExt, symlink},
    };

    fn full(w: &mut Watcher) -> Batch {
        w.collect(&BTreeSet::from([w.root.clone()])).unwrap()
    }

    fn next(w: &mut Watcher) -> Batch {
        let mut dirty = BTreeSet::new();
        assert!(w.events(&mut dirty, Duration::from_secs(2)).unwrap());
        w.collect(&dirty).unwrap()
    }

    #[test]
    fn watches_moved_directories_at_the_new_name_and_recovers_recreated_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let before = root.join("before");
        let after = root.join("after");
        fs::create_dir_all(before.join("nested")).unwrap();
        let mut w = Watcher::new(&root, &temp.path().join("history.dolt")).unwrap();
        full(&mut w);
        fs::rename(&before, &after).unwrap();
        let batch = next(&mut w);
        assert!(batch.scopes.contains(&before));
        assert!(batch.entries.iter().any(|e| e.path == after.join("nested")));
        let raw = after.join("nested").join(OsString::from_vec(vec![0xff]));
        fs::write(&raw, b"hello").unwrap();
        let batch = next(&mut w);
        assert!(
            batch
                .entries
                .iter()
                .any(|e| e.path == raw && e.kind == Kind::File && e.size == 5)
        );
        fs::rename(&root, temp.path().join("moved-out")).unwrap();
        let batch = next(&mut w);
        assert_eq!(batch.scopes, vec![root.clone()]);
        assert!(batch.entries.is_empty());
        fs::create_dir(&root).unwrap();
        fs::write(root.join("back"), "back").unwrap();
        assert!(
            next(&mut w)
                .entries
                .iter()
                .any(|e| e.path == root.join("back"))
        );
    }

    #[test]
    fn internal_writes_and_symlink_targets_do_not_produce_records() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir(&root).unwrap();
        let outside = temp.path().join("elsewhere");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("private"), b"outside").unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let db = root.join("history.dolt");
        let mut w = Watcher::new(&root, &db).unwrap();
        let batch = full(&mut w);
        assert_eq!(batch.entries.len(), 2);
        assert!(batch.entries.iter().any(|e| e.kind == Kind::Symlink));
        for p in [
            db,
            root.join(".history.dolt-lock"),
            root.join("history.dolt.watch-lock"),
        ] {
            fs::write(p, b"internal").unwrap();
        }
        let mut dirty = BTreeSet::new();
        assert!(!w.events(&mut dirty, Duration::from_millis(150)).unwrap());
        assert!(dirty.is_empty());
        let batch = full(&mut w);
        assert_eq!(batch.entries.len(), 2);
        let root_row = batch.entries.iter().find(|e| e.path == root).unwrap();
        assert_eq!(
            (
                root_row.mtime_ns,
                root_row.ctime_ns,
                root_row.size,
                root_row.bytes
            ),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn unreadable_listings_preserve_previously_measured_subtrees() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("blocked");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), b"keep").unwrap();
        let mut w = Watcher::new(temp.path(), &temp.path().join("db")).unwrap();
        full(&mut w);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let result = full(&mut w);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.preserved.contains(&path));
        assert!(
            result
                .entries
                .iter()
                .any(|e| e.path == path && e.mode & 0o777 == 0)
        );
    }
}
