//! Compressed, private snapshots of completed scans. Filenames are only indices;
//! the original Unix path and filesystem identity are verified from each header.
use crate::{
    cache::locate,
    scan::{Node, Scan},
};
use flate2::{Compression, bufread::GzDecoder, write::GzEncoder};
use std::{
    collections::hash_map::DefaultHasher,
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, BufReader, BufWriter, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const MAGIC: &[u8; 8] = b"COROLLA1";
const MIB: u64 = 1 << 20;
const MAX_FILE: u64 = 192 * MIB;
const MAX_DISK: u64 = 512 * MIB;
const MAX_FILES: usize = 12;
const MAX_NODES: usize = 3_000_000;
const MAX_NAMES: usize = 128 << 20;
const MAX_PLAIN: u64 = 320 * MIB;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid scan snapshot")
}

fn directory() -> Option<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
        .map(|p| p.join("corolla/scans"))
}

fn lock(dir: &Path, shared: bool) -> io::Result<File> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(dir.join("lock"))?;
    if shared {
        file.lock_shared()?;
    } else {
        file.lock()?;
    }
    Ok(file)
}

fn integer<const N: usize>(r: &mut impl Read) -> io::Result<[u8; N]> {
    let mut n = [0u8; N];
    r.read_exact(&mut n)?;
    Ok(n)
}

fn read_epoch(dir: &Path) -> io::Result<u64> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("epoch"))?;
    if file.metadata()?.len() != 16 {
        return Err(invalid());
    }
    let epoch = u64::from_le_bytes(integer(&mut file)?);
    if u64::from_le_bytes(integer(&mut file)?) != !epoch {
        return Err(invalid());
    }
    Ok(epoch)
}

fn write_epoch(dir: &Path, epoch: u64) -> io::Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(&epoch.to_le_bytes())?;
    temp.write_all(&(!epoch).to_le_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(dir.join("epoch"))?;
    Ok(())
}

fn generation_at(dir: &Path) -> io::Result<u64> {
    let _lock = lock(dir, false)?;
    // A crashed writer may leave a private staging file. Live writers hold an
    // exclusive lock on theirs until the rename completes.
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().as_bytes().starts_with(b".snapshot-")
            && let Ok(file) = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(entry.path())
            && file.try_lock().is_ok()
        {
            let _ = fs::remove_file(entry.path());
        }
    }
    match read_epoch(dir) {
        Ok(value) => Ok(value),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
            ) =>
        {
            // The previous generation is unknown. Do not reuse its snapshots.
            for entry in fs::read_dir(dir)? {
                let path = entry?.path();
                if path.extension() == Some(OsStr::new("csn")) {
                    fs::remove_file(path)?;
                }
            }
            // A freshly recreated cache directory gets a distinct generation.
            let value = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos() as u64;
            write_epoch(dir, value)?;
            Ok(value)
        }
        Err(e) => Err(e),
    }
}

pub fn generation() -> Option<u64> {
    generation_at(&directory()?).ok()
}

fn read_path(r: &mut impl Read) -> io::Result<PathBuf> {
    if &integer::<8>(r)? != MAGIC {
        return Err(invalid());
    }
    let len = u32::from_le_bytes(integer(r)?) as usize;
    if len == 0 || len > 4096 {
        return Err(invalid());
    }
    let mut raw = vec![0u8; len];
    r.read_exact(&mut raw)?;
    if raw.contains(&0) {
        return Err(invalid());
    }
    let path = PathBuf::from(OsString::from_vec(raw));
    if !path.is_absolute() || path.components().any(|x| matches!(x, Component::ParentDir)) {
        return Err(invalid());
    }
    Ok(path)
}

struct Header {
    path: PathBuf,
    root: PathBuf,
    identity: (u64, u64),
    modified: SystemTime,
    bytes: u64,
}

fn headers(dir: &Path) -> io::Result<Vec<Header>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("csn")) {
            continue;
        }
        let header = (|| -> io::Result<Header> {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)?;
            let meta = file.metadata()?;
            if !meta.is_file() || meta.len() > MAX_FILE {
                return Err(invalid());
            }
            let mut reader = BufReader::new(file);
            let root = read_path(&mut reader)?;
            let identity = (
                u64::from_le_bytes(integer(&mut reader)?),
                u64::from_le_bytes(integer(&mut reader)?),
            );
            Ok(Header {
                path,
                root,
                identity,
                modified: meta.modified()?,
                bytes: meta.len(),
            })
        })();
        if let Ok(item) = header {
            entries.push(item);
        }
    }
    entries.sort_unstable_by_key(|item| std::cmp::Reverse(item.modified));
    Ok(entries)
}

fn matches_root(header: &Header) -> bool {
    fs::symlink_metadata(&header.root)
        .is_ok_and(|m| m.is_dir() && (m.dev(), m.ino()) == header.identity)
}

pub fn roots() -> Vec<PathBuf> {
    let Some(dir) = directory() else {
        return Vec::new();
    };
    let Ok(_lock) = lock(&dir, true) else {
        return Vec::new();
    };
    headers(&dir)
        .unwrap_or_default()
        .into_iter()
        .filter(matches_root)
        .map(|h| h.root)
        .collect()
}

fn write_data(scan: &Scan, file: &mut File) -> io::Result<()> {
    let path = scan.root.as_os_str().as_bytes();
    if path.len() > 4096 || scan.nodes.is_empty() || scan.nodes.len() > MAX_NODES {
        return Err(invalid());
    }
    let root = &scan.nodes[0];
    let mut writer = BufWriter::new(file);
    writer.write_all(MAGIC)?;
    writer.write_all(&(path.len() as u32).to_le_bytes())?;
    writer.write_all(path)?;
    writer.write_all(&root.identity.0.to_le_bytes())?;
    writer.write_all(&root.identity.1.to_le_bytes())?;
    let mut zip = BufWriter::new(GzEncoder::new(writer, Compression::fast()));
    zip.write_all(&(scan.nodes.len() as u32).to_le_bytes())?;
    zip.write_all(&scan.skipped.to_le_bytes())?;
    let mut name_bytes = 0;
    for node in &scan.nodes {
        let name = node.name.as_bytes();
        name_bytes += name.len();
        if name.len() > 255 || name_bytes > MAX_NAMES {
            return Err(invalid());
        }
        zip.write_all(&[
            u8::from(node.is_dir) | (u8::from(node.incomplete) << 1),
            name.len() as u8,
        ])?;
        zip.write_all(name)?;
        zip.write_all(&node.bytes.to_le_bytes())?;
        zip.write_all(&node.parent.map_or(u32::MAX, |p| p as u32).to_le_bytes())?;
        let (dev, ino, ctime, nanos) = node.identity;
        zip.write_all(&dev.to_le_bytes())?;
        zip.write_all(&ino.to_le_bytes())?;
        zip.write_all(&ctime.to_le_bytes())?;
        zip.write_all(&nanos.to_le_bytes())?;
    }
    zip.into_inner()?.finish()?.flush()
}

fn decode(header: &Header) -> io::Result<Scan> {
    let mut file = BufReader::new(File::open(&header.path)?);
    if read_path(&mut file)? != header.root {
        return Err(invalid());
    }
    let identity = (
        u64::from_le_bytes(integer(&mut file)?),
        u64::from_le_bytes(integer(&mut file)?),
    );
    if identity != header.identity {
        return Err(invalid());
    }
    let gzip = GzDecoder::new(file);
    let mut data = BufReader::new(gzip.take(MAX_PLAIN + 1));
    let count = u32::from_le_bytes(integer(&mut data)?) as usize;
    if count == 0 || count > MAX_NODES {
        return Err(invalid());
    }
    let skipped = u64::from_le_bytes(integer(&mut data)?);
    let mut nodes: Vec<Node> = Vec::with_capacity(count.min(8192));
    let mut name_bytes = 0usize;
    for id in 0..count {
        let [kind, len] = integer(&mut data)?;
        name_bytes += usize::from(len);
        if !matches!(kind, 0 | 1 | 3) || len == 0 || name_bytes > MAX_NAMES {
            return Err(invalid());
        }
        let mut raw = vec![0; usize::from(len)];
        data.read_exact(&mut raw)?;
        let name = OsString::from_vec(raw);
        let bytes = u64::from_le_bytes(integer(&mut data)?);
        let parent_id = u32::from_le_bytes(integer(&mut data)?);
        let identity = (
            u64::from_le_bytes(integer(&mut data)?),
            u64::from_le_bytes(integer(&mut data)?),
            i64::from_le_bytes(integer(&mut data)?),
            i64::from_le_bytes(integer(&mut data)?),
        );
        let parent = if id == 0 {
            if parent_id != u32::MAX
                || kind & 1 == 0
                || (identity.0, identity.1) != header.identity
                || name != header.root.file_name().unwrap_or(OsStr::new("/"))
            {
                return Err(invalid());
            }
            None
        } else {
            let parent = parent_id as usize;
            let raw = name.as_bytes();
            if parent >= id
                || !nodes[parent].is_dir
                || raw.contains(&b'/')
                || raw.contains(&0)
                || raw == b"."
                || raw == b".."
            {
                return Err(invalid());
            }
            nodes[parent].children.push(id);
            Some(parent)
        };
        nodes.push(Node {
            name,
            bytes,
            parent,
            is_dir: kind & 1 != 0,
            incomplete: kind & 2 != 0,
            children: Vec::new(),
            identity,
        });
    }
    // Reading to the end verifies the gzip length and CRC. Reject trailing records.
    if data.read(&mut [0u8; 1])? != 0 {
        return Err(invalid());
    }
    for id in 0..count {
        let mut children = std::mem::take(&mut nodes[id].children);
        if children.is_empty() {
            continue;
        }
        let bytes = children
            .iter()
            .try_fold(0u64, |sum, &i| sum.checked_add(nodes[i].bytes));
        if bytes.is_none_or(|sum| sum > nodes[id].bytes) {
            return Err(invalid());
        }
        children.sort_unstable_by(|&a, &b| nodes[a].name.cmp(&nodes[b].name));
        if children
            .windows(2)
            .any(|pair| nodes[pair[0]].name == nodes[pair[1]].name)
        {
            return Err(invalid());
        }
        children.sort_by_key(|&i| std::cmp::Reverse(nodes[i].bytes));
        nodes[id].children = children;
    }
    Ok(Scan {
        root: header.root.clone(),
        nodes,
        skipped,
    })
}

fn load_at(dir: &Path, requested: &Path) -> Option<(Arc<Scan>, usize)> {
    let canonical = requested.canonicalize().ok()?;
    let metadata = fs::metadata(&canonical).ok()?;
    if !metadata.is_dir() {
        return None;
    }
    let _lock = lock(dir, true).ok()?;
    read_epoch(dir).ok()?;
    for h in headers(dir).ok()? {
        if !crate::scan::can_cover(&h.root, &canonical) || !matches_root(&h) {
            continue;
        }
        if let Ok(scan) = decode(&h)
            && let Some(id) = locate(&scan, &canonical)
        {
            let (dev, ino, _, _) = scan.nodes[id].identity;
            if (dev, ino) == (metadata.dev(), metadata.ino()) && scan.nodes[id].is_dir {
                return Some((Arc::new(scan), id));
            }
        }
    }
    None
}

pub fn load(path: &Path) -> Option<(Arc<Scan>, usize)> {
    load_at(&directory()?, path)
}

fn filename(root: &Path) -> String {
    let mut hash = DefaultHasher::new();
    root.as_os_str().as_bytes().hash(&mut hash);
    format!("{:016x}.csn", hash.finish())
}

fn save_at(dir: &Path, scan: &Scan, epoch: u64) -> io::Result<()> {
    let mut temp = {
        let _lock = lock(dir, true)?;
        if read_epoch(dir)? != epoch {
            return Ok(());
        }
        let temp = tempfile::Builder::new()
            .prefix(".snapshot-")
            .tempfile_in(dir)?;
        temp.as_file().lock()?;
        temp
    };
    write_data(scan, temp.as_file_mut())?;
    let bytes = temp.as_file().metadata()?.len();
    if bytes > MAX_FILE {
        return Err(invalid());
    }
    temp.as_file().sync_all()?;
    let _lock = lock(dir, false)?;
    if read_epoch(dir)? != epoch {
        return Ok(());
    }
    let destination = dir.join(filename(&scan.root));
    let mut total = bytes;
    let mut count = 1;
    // Keep recent usable snapshots within count and byte quotas. Clean up damaged
    // snapshots, too, so an interrupted or obsolete cache never consumes the quota.
    let mut keep = std::collections::HashSet::new();
    keep.insert(destination.clone());
    for h in headers(dir)? {
        if h.path == destination {
            continue;
        }
        if count < MAX_FILES && total + h.bytes <= MAX_DISK {
            total += h.bytes;
            count += 1;
            keep.insert(h.path);
        }
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension() == Some(OsStr::new("csn")) && !keep.contains(&path) {
            fs::remove_file(path)?;
        }
    }
    temp.persist(destination)?;
    Ok(())
}

pub fn save_if_current(scan: &Scan, generation: Option<u64>) {
    if let (Some(dir), Some(epoch)) = (directory(), generation) {
        let _ = save_at(&dir, scan, epoch);
    }
}

fn invalidate_at(dir: &Path, path: &Path) -> io::Result<(u64, u64)> {
    let _lock = lock(dir, false)?;
    let before = read_epoch(dir)?;
    let after = before.checked_add(1).ok_or_else(invalid)?;
    write_epoch(dir, after)?;
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    for h in headers(dir)? {
        if path.starts_with(&h.root)
            || h.root.starts_with(path)
            || canonical.starts_with(&h.root)
            || h.root.starts_with(&canonical)
        {
            fs::remove_file(h.path)?;
        }
    }
    Ok((before, after))
}

pub fn invalidate(path: &Path) -> Option<(u64, u64)> {
    let dir = directory()?;
    generation_at(&dir).ok()?;
    invalidate_at(&dir, path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;
    use std::sync::atomic::AtomicBool;

    fn scan(path: &Path) -> Scan {
        scan::scan(path, &AtomicBool::new(false), |_, _| {}).unwrap()
    }

    #[test]
    fn restores_raw_names_subtree_focus_and_snapshot_metadata() {
        let files = tempfile::tempdir().unwrap();
        let sub = files.path().join("Folder");
        let raw = OsStr::from_bytes(b"name-\xff");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join(raw), b"sample").unwrap();
        let original = scan(files.path());
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path();
        let epoch = generation_at(dir).unwrap();
        save_at(dir, &original, epoch).unwrap();
        let (restored, focus) = load_at(dir, &sub).unwrap();
        let item = restored.nodes[focus].children[0];
        assert_eq!(restored.path(item), sub.join(raw));
        assert_eq!(restored.nodes[item].identity, original.nodes[item].identity);
        assert_eq!(restored.nodes[0].bytes, original.nodes[0].bytes);
        let path = dir.join(filename(files.path()));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let mut contents = fs::read(&path).unwrap();
        let last = contents.len() - 8;
        contents[last] ^= 0x40;
        fs::write(path, contents).unwrap();
        assert!(load_at(dir, &sub).is_none());
    }

    #[test]
    fn invalidation_removes_overlaps_keeps_siblings_and_blocks_late_writes() {
        let files = tempfile::tempdir().unwrap();
        let a = files.path().join("a");
        let b = files.path().join("b");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        let old = scan(files.path());
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path();
        let before = generation_at(dir).unwrap();
        for tree in [&old, &scan(&a), &scan(&b)] {
            save_at(dir, tree, before).unwrap();
        }
        let (previous, next) = invalidate_at(dir, &a).unwrap();
        assert_eq!(previous, before);
        assert_eq!(next, generation_at(dir).unwrap());
        save_at(dir, &old, before).unwrap();
        assert!(load_at(dir, &a).is_none());
        assert!(load_at(dir, files.path()).is_none());
        assert!(load_at(dir, &b).is_some());
    }

    #[test]
    fn damaged_epoch_resets_cache_and_only_abandoned_writers_are_reclaimed() {
        let files = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path();
        let epoch = generation_at(dir).unwrap();
        save_at(dir, &scan(files.path()), epoch).unwrap();
        let live = tempfile::Builder::new()
            .prefix(".snapshot-")
            .tempfile_in(dir)
            .unwrap();
        live.as_file().lock().unwrap();
        fs::write(dir.join(".snapshot-abandoned"), b"interrupted").unwrap();
        fs::write(dir.join("epoch"), b"truncated").unwrap();
        assert_ne!(generation_at(dir).unwrap(), epoch);
        assert!(live.path().exists());
        assert!(!dir.join(".snapshot-abandoned").exists());
        assert!(load_at(dir, files.path()).is_none());
    }

    #[test]
    fn rejects_bad_graphs_and_replaced_roots_and_enforces_count_quota() {
        let files = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path();
        let epoch = generation_at(dir).unwrap();
        let mut last = PathBuf::new();
        for n in 0..=MAX_FILES {
            last = files.path().join(n.to_string());
            fs::create_dir(&last).unwrap();
            save_at(dir, &scan(&last), epoch).unwrap();
        }
        assert_eq!(headers(dir).unwrap().len(), MAX_FILES);
        fs::rename(&last, files.path().join("old")).unwrap();
        fs::create_dir(&last).unwrap();
        assert!(load_at(dir, &last).is_none());
        fs::create_dir(last.join("child")).unwrap();
        let mut bad = scan(&last);
        bad.nodes[1].parent = Some(1);
        save_at(dir, &bad, epoch).unwrap();
        assert!(load_at(dir, &last).is_none());
    }
}
