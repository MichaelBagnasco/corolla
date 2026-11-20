//! Compressed, private snapshots of completed scans. Filenames are only indices;
//! the original Unix path and filesystem identity are verified from each header.
use crate::{
    cache::locate,
    patch::{self, Changes},
    scan::{Node, Scan},
};
use flate2::{Compression, bufread::GzDecoder, write::GzEncoder};
use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const OLD_MAGIC: &[u8; 8] = b"COROLLA1";
const MAGIC: &[u8; 8] = b"COROLLA2";
const CATALOG_MAGIC: &[u8; 8] = b"CORIDX02";
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
        .map(|p| p.join("corolla/scans/v2"))
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

/// Legacy catalogs contain just the epoch and its complement. New catalogs
/// identify every readable snapshot by filename and the revision in its header.
#[derive(Clone)]
struct Catalog {
    epoch: u64,
    revisions: Option<HashMap<String, u64>>,
}

impl Catalog {
    fn permits(&self, filename: &str, revision: u64) -> bool {
        match &self.revisions {
            Some(revisions) => revisions.get(filename) == Some(&revision),
            None => revision == 0,
        }
    }

    fn publish(dir: &Path, epoch: u64, entries: &[Header]) -> io::Result<Self> {
        let mut payload = Vec::new();
        payload.extend_from_slice(CATALOG_MAGIC);
        payload.extend_from_slice(&epoch.to_le_bytes());
        payload.extend_from_slice(&(!epoch).to_le_bytes());
        payload.push(entries.len().try_into().map_err(|_| invalid())?);
        let mut revisions = HashMap::new();
        for entry in entries {
            let name = entry.filename().ok_or_else(invalid)?;
            payload.push(name.len().try_into().map_err(|_| invalid())?);
            payload.extend_from_slice(name.as_bytes());
            payload.extend_from_slice(&entry.revision.to_le_bytes());
            revisions.insert(name.to_owned(), entry.revision);
        }
        let mut crc = flate2::Crc::new();
        crc.update(&payload);
        payload.extend_from_slice(&crc.sum().to_le_bytes());
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        temp.write_all(&payload)?;
        temp.as_file().sync_all()?;
        temp.persist(dir.join("epoch"))?;
        File::open(dir)?.sync_all()?;
        Ok(Self {
            epoch,
            revisions: Some(revisions),
        })
    }
}

fn read_catalog(dir: &Path) -> io::Result<Catalog> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("epoch"))?;
    if !file.metadata()?.is_file() {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() == 16 {
        let mut input = &bytes[..];
        let epoch = u64::from_le_bytes(integer(&mut input)?);
        if u64::from_le_bytes(integer(&mut input)?) != !epoch {
            return Err(invalid());
        }
        return Ok(Catalog {
            epoch,
            revisions: None,
        });
    }
    if !(29..=4096).contains(&bytes.len()) {
        return Err(invalid());
    }
    let (mut payload, checksum) = bytes.split_at(bytes.len() - 4);
    let mut crc = flate2::Crc::new();
    crc.update(payload);
    if checksum != crc.sum().to_le_bytes() || &integer::<8>(&mut payload)? != CATALOG_MAGIC {
        return Err(invalid());
    }
    let epoch = u64::from_le_bytes(integer(&mut payload)?);
    if u64::from_le_bytes(integer(&mut payload)?) != !epoch {
        return Err(invalid());
    }
    let [count] = integer(&mut payload)?;
    if usize::from(count) > MAX_FILES {
        return Err(invalid());
    }
    let mut revisions = HashMap::new();
    for _ in 0..count {
        let [len] = integer(&mut payload)?;
        let mut name = vec![0; usize::from(len)];
        payload.read_exact(&mut name)?;
        let name = String::from_utf8(name).map_err(|_| invalid())?;
        if !name.ends_with(".csn")
            || !name
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b'.' || b == b's' || b == b'n')
        {
            return Err(invalid());
        }
        let revision = u64::from_le_bytes(integer(&mut payload)?);
        if revisions.insert(name, revision).is_some() {
            return Err(invalid());
        }
    }
    if !payload.is_empty() {
        return Err(invalid());
    }
    Ok(Catalog {
        epoch,
        revisions: Some(revisions),
    })
}

// Keep the old format's directory and epoch untouched: a running 0.1.1
// process treats a new-format epoch as corruption and removes its own maps.
fn import_legacy(dir: &Path) -> io::Result<Vec<Header>> {
    let Some(old_dir) = dir
        .parent()
        .filter(|_| dir.file_name() == Some(OsStr::new("v2")))
    else {
        return Ok(Vec::new());
    };
    if !old_dir.join("epoch").is_file() {
        return Ok(Vec::new());
    }
    let _old_lock = lock(old_dir, true)?;
    let Ok(legacy) = read_catalog(old_dir) else {
        return Ok(Vec::new());
    };
    if legacy.revisions.is_some() {
        return Ok(Vec::new());
    }
    let mut candidates: Vec<_> = headers(old_dir, &legacy)?
        .into_iter()
        .filter(matches_root)
        .collect();
    trim(&mut candidates);
    let mut imported = Vec::new();
    for mut h in candidates {
        let output = dir.join(filename(&h.root));
        if fs::hard_link(&h.path, &output).is_ok() {
            h.path = output;
            imported.push(h);
        }
    }
    Ok(imported)
}

fn record_migration(dir: &Path) -> io::Result<()> {
    // Once any new-format catalog is in use, absence or corruption of that
    // catalog should start empty rather than resurrecting pre-cleanup maps.
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join("initialized"))
    {
        Ok(marker) => {
            marker.sync_all()?;
            File::open(dir)?.sync_all()
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
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
    match read_catalog(dir) {
        Ok(catalog) => {
            record_migration(dir)?;
            Ok(catalog.epoch)
        }
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
            let imported =
                if e.kind() == io::ErrorKind::NotFound && !dir.join("initialized").try_exists()? {
                    import_legacy(dir)?
                } else {
                    Vec::new()
                };
            Catalog::publish(dir, value, &imported)?;
            record_migration(dir)?;
            Ok(value)
        }
        Err(e) => Err(e),
    }
}

pub fn generation() -> Option<u64> {
    generation_at(&directory()?).ok()
}

fn read_path(r: &mut impl Read) -> io::Result<(PathBuf, u64)> {
    let magic = integer::<8>(r)?;
    let revision = if &magic == OLD_MAGIC {
        0
    } else if &magic == MAGIC {
        u64::from_le_bytes(integer(r)?)
    } else {
        return Err(invalid());
    };
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
    Ok((path, revision))
}

#[derive(Clone)]
struct Header {
    revision: u64,
    path: PathBuf,
    root: PathBuf,
    identity: (u64, u64),
    modified: SystemTime,
    bytes: u64,
}

impl Header {
    fn filename(&self) -> Option<&str> {
        self.path.file_name()?.to_str()
    }
}

fn headers(dir: &Path, catalog: &Catalog) -> io::Result<Vec<Header>> {
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
            let (root, revision) = read_path(&mut reader)?;
            if !path
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|n| catalog.permits(n, revision))
            {
                return Err(invalid());
            }
            let identity = (
                u64::from_le_bytes(integer(&mut reader)?),
                u64::from_le_bytes(integer(&mut reader)?),
            );
            Ok(Header {
                revision,
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

fn write_data(scan: &Scan, file: &mut File, revision: u64) -> io::Result<()> {
    let path = scan.root.as_os_str().as_bytes();
    if path.len() > 4096 || scan.nodes.is_empty() || scan.nodes.len() > MAX_NODES {
        return Err(invalid());
    }
    let root = &scan.nodes[0];
    let mut writer = BufWriter::new(file);
    writer.write_all(MAGIC)?;
    writer.write_all(&revision.to_le_bytes())?;
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
    let (root, revision) = read_path(&mut file)?;
    if root != header.root || revision != header.revision {
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

fn find_in(scan: &Scan, path: &Path) -> Option<usize> {
    let id = locate(scan, path)?;
    let meta = fs::metadata(path).ok()?;
    let (dev, ino, _, _) = scan.nodes[id].identity;
    (meta.is_dir() && (dev, ino) == (meta.dev(), meta.ino())).then_some(id)
}

fn load_at(dir: &Path, requested: &Path) -> Option<(Arc<Scan>, usize)> {
    let canonical = requested.canonicalize().ok()?;
    let _lock = lock(dir, true).ok()?;
    for h in headers(dir, &read_catalog(dir).ok()?).ok()? {
        if crate::scan::can_cover(&h.root, &canonical)
            && matches_root(&h)
            && let Ok(scan) = decode(&h)
            && let Some(id) = find_in(&scan, &canonical)
        {
            return Some((Arc::new(scan), id));
        }
    }
    None
}

pub fn load(path: &Path) -> Option<(Arc<Scan>, usize)> {
    let dir = directory()?;
    if !dir.join("epoch").is_file() {
        generation_at(&dir).ok()?;
    }
    load_at(&dir, path)
}

fn filename(root: &Path) -> String {
    // FNV-1a over the unmodified Unix path, stable across Rust toolchains.
    let hash = root
        .as_os_str()
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
    format!("{hash:016x}.csn")
}

fn trim(entries: &mut Vec<Header>) {
    let mut bytes = 0;
    let mut roots = HashSet::new();
    let mut names = HashSet::new();
    entries.retain(|h| {
        if roots.len() == MAX_FILES
            || bytes + h.bytes > MAX_DISK
            || roots.contains(&h.root)
            || names.contains(&h.path)
        {
            return false;
        }
        bytes += h.bytes;
        roots.insert(h.root.clone());
        names.insert(h.path.clone());
        true
    });
}

fn reap(dir: &Path, catalog: &Catalog) -> io::Result<()> {
    let Some(revisions) = &catalog.revisions else {
        return Ok(());
    };
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension() == Some(OsStr::new("csn"))
            && !entry
                .file_name()
                .to_str()
                .is_some_and(|s| revisions.contains_key(s))
        {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn stage(dir: &Path, scan: &Scan, epoch: u64) -> io::Result<tempfile::NamedTempFile> {
    let mut temp = tempfile::Builder::new()
        .prefix(".snapshot-")
        .tempfile_in(dir)?;
    temp.as_file().lock()?;
    write_data(scan, temp.as_file_mut(), epoch)?;
    if temp.as_file().metadata()?.len() > MAX_FILE {
        return Err(invalid());
    }
    temp.as_file().sync_all()?;
    Ok(temp)
}

fn persist(
    dir: &Path,
    scan: &Scan,
    epoch: u64,
    temp: tempfile::NamedTempFile,
) -> io::Result<Header> {
    let path = dir.join(filename(&scan.root));
    let file = temp.persist(&path)?;
    let meta = file.metadata()?;
    let (dev, ino, _, _) = scan.nodes[0].identity;
    Ok(Header {
        root: scan.root.clone(),
        path,
        identity: (dev, ino),
        revision: epoch,
        bytes: meta.len(),
        modified: meta.modified()?,
    })
}

fn save_at(dir: &Path, scan: &Scan, expected: u64) -> io::Result<()> {
    let temp = {
        let _lock = lock(dir, true)?;
        if read_catalog(dir)?.epoch != expected {
            return Ok(());
        }
        stage(dir, scan, expected)?
    };
    let _lock = lock(dir, false)?;
    let current = read_catalog(dir)?;
    if current.epoch != expected {
        return Ok(());
    }
    let mut entries = headers(dir, &current)?;
    entries.insert(0, persist(dir, scan, expected, temp)?);
    trim(&mut entries);
    let published = Catalog::publish(dir, expected, &entries)?;
    reap(dir, &published)
}

pub fn save_if_current(scan: &Scan, generation: Option<u64>) {
    if let (Some(dir), Some(epoch)) = (directory(), generation) {
        let _ = save_at(&dir, scan, epoch);
    }
}

fn update_at(
    dir: &Path,
    changes: &Changes,
    expected: Option<u64>,
) -> io::Result<Option<(u64, u64)>> {
    let _lock = lock(dir, false)?;
    let current = read_catalog(dir)?;
    if expected.is_some_and(|epoch| epoch != current.epoch) {
        return Ok(None);
    }
    let before = current.epoch;
    let after = before.checked_add(1).ok_or_else(invalid)?;
    let (affected, mut keep): (Vec<_>, Vec<_>) = headers(dir, &current)?
        .into_iter()
        .partition(|header| changes.affects(&header.root));
    trim(&mut keep);
    // Commit the invalidation first. A crash at any point after this rename
    // leaves touched files unreadable, including files only partially replaced.
    // Unrelated scans remain readable at their previous individual revisions.
    Catalog::publish(dir, after, &keep)?;
    let mut replacements = Vec::new();
    for header in affected {
        if changes
            .refreshed
            .as_ref()
            .is_some_and(|s| s.root == header.root)
            || !matches_root(&header)
        {
            continue;
        }
        let Ok(previous) = decode(&header) else {
            continue;
        };
        if !changes.touches(&previous) {
            keep.push(header);
            continue;
        }
        if let Some(patched) = patch::apply(&previous, changes)
            && let Ok(temp) = stage(dir, &patched, after)
        {
            replacements.push(persist(dir, &patched, after, temp)?);
        }
    }
    if let Some(fresh) = &changes.refreshed {
        let cleaned;
        let snapshot = if changes.removed.is_empty() {
            Some(fresh.as_ref())
        } else {
            cleaned = patch::apply(fresh, changes);
            cleaned.as_ref()
        };
        if let Some(scan) = snapshot
            && let Ok(temp) = stage(dir, scan, after)
        {
            replacements.insert(0, persist(dir, scan, after, temp)?);
        }
    }
    replacements.append(&mut keep);
    trim(&mut replacements);
    let published = Catalog::publish(dir, after, &replacements)?;
    reap(dir, &published)?;
    Ok(Some((before, after)))
}

pub fn update(changes: &Changes, expected: Option<u64>) -> Option<(u64, u64)> {
    let dir = directory()?;
    generation_at(&dir).ok()?;
    update_at(&dir, changes, expected).ok()?
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
    fn cleanup_patches_parent_and_keeps_unrelated_roots_in_one_generation() {
        let files = tempfile::tempdir().unwrap();
        let a = files.path().join("a");
        let b = files.path().join("b");
        let c = files.path().join("c");
        for p in [&a, &b, &c] {
            fs::create_dir(p).unwrap();
        }
        let old = scan(files.path());
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path();
        let before = generation_at(dir).unwrap();
        for tree in [&old, &scan(&a), &scan(&b)] {
            save_at(dir, tree, before).unwrap();
        }
        fs::remove_dir(&a).unwrap();
        fs::remove_dir(&c).unwrap();
        let changes = Changes {
            removed: vec![a.clone(), c],
            ..Default::default()
        };
        let pair = update_at(dir, &changes, None).unwrap().unwrap();
        assert_eq!(pair, (before, before + 1));
        save_at(dir, &old, before).unwrap();
        let (updated, _) = load_at(dir, files.path()).unwrap();
        assert_eq!(updated.nodes[0].children.len(), 1);
        assert_eq!(updated.path(updated.nodes[0].children[0]), b);
        assert_eq!(updated.nodes[0].bytes, scan(files.path()).nodes[0].bytes);
        assert!(load_at(dir, &b).is_some());
        assert_eq!(
            read_catalog(dir)
                .unwrap()
                .revisions
                .unwrap()
                .get(&filename(&b)),
            Some(&before)
        );
    }

    #[test]
    fn refresh_persists_both_ancestor_and_subtree_and_rejects_stale_writers() {
        let files = tempfile::tempdir().unwrap();
        let root = files.path();
        let branch = root.join("branch");
        fs::create_dir(&branch).unwrap();
        fs::write(branch.join("old"), "old").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path();
        let before = generation_at(dir).unwrap();
        save_at(dir, &scan(root), before).unwrap();
        let old = fs::read(dir.join(filename(root))).unwrap();
        fs::rename(branch.join("old"), branch.join("new")).unwrap();
        let changes = Changes {
            refreshed: Some(Arc::new(scan(&branch))),
            ..Default::default()
        };
        assert_eq!(
            update_at(dir, &changes, Some(before)).unwrap(),
            Some((before, before + 1))
        );
        assert!(update_at(dir, &changes, Some(before)).unwrap().is_none());
        for path in [root, branch.as_path()] {
            let (updated, _) = load_at(dir, path).unwrap();
            assert!(updated.nodes.iter().any(|n| n.name == "new"));
            assert!(!updated.nodes.iter().any(|n| n.name == "old"));
        }
        assert_eq!(headers(dir, &read_catalog(dir).unwrap()).unwrap().len(), 2);
        // Even restoring an otherwise intact old gzip must not revive its revision.
        fs::write(dir.join(filename(root)), old).unwrap();
        assert!(load_at(dir, root).is_none());
    }

    #[test]
    fn failed_batch_exposes_only_untouched_snapshots_after_restart() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("parent");
        let branch = root.join("branch");
        let other = base.path().join("other");
        fs::create_dir_all(&branch).unwrap();
        fs::create_dir(&other).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path();
        let epoch = generation_at(dir).unwrap();
        for s in [scan(&root), scan(&other)] {
            save_at(dir, &s, epoch).unwrap();
        }
        fs::write(branch.join("new"), "new").unwrap();
        // Fail the last rename, after the patched parent has reached disk.
        fs::create_dir(dir.join(filename(&branch))).unwrap();
        let changes = Changes {
            refreshed: Some(Arc::new(scan(&branch))),
            ..Default::default()
        };
        assert!(update_at(dir, &changes, Some(epoch)).is_err());
        assert_eq!(generation_at(dir).unwrap(), epoch + 1);
        assert!(load_at(dir, &root).is_none());
        assert!(load_at(dir, &branch).is_none());
        assert!(load_at(dir, &other).is_some());
        save_at(dir, &scan(&root), epoch).unwrap();
        assert!(load_at(dir, &root).is_none());
    }

    #[test]
    fn legacy_snapshots_open_then_migrate_without_duplicate_files() {
        let files = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path();
        let epoch = 12345u64;
        let tree = scan(files.path());
        fs::write(
            dir.join("epoch"),
            [epoch.to_le_bytes(), (!epoch).to_le_bytes()].concat(),
        )
        .unwrap();
        let mut buf = tempfile::tempfile().unwrap();
        write_data(&tree, &mut buf, epoch).unwrap();
        use std::io::{Seek, SeekFrom};
        buf.seek(SeekFrom::Start(16)).unwrap();
        let mut body = OLD_MAGIC.to_vec();
        buf.read_to_end(&mut body).unwrap();
        let legacy = dir.join("abababababababab.csn");
        fs::write(&legacy, body).unwrap();
        let new = dir.join("v2");
        let next = generation_at(&new).unwrap();
        let (restored, _) = load_at(&new, files.path()).unwrap();
        assert_eq!(restored.nodes[0].identity, tree.nodes[0].identity);
        assert!(load_at(dir, files.path()).is_some());
        // Old readers can still open their original file after the upgrade.
        assert_eq!(fs::metadata(dir.join("epoch")).unwrap().len(), 16);
        assert_eq!(
            fs::metadata(&legacy).unwrap().ino(),
            fs::metadata(new.join(filename(files.path())))
                .unwrap()
                .ino()
        );
        save_at(&new, &tree, next).unwrap();
        assert!(legacy.exists());
        assert_eq!(
            headers(&new, &read_catalog(&new).unwrap()).unwrap().len(),
            1
        );
        fs::remove_file(new.join("epoch")).unwrap();
        generation_at(&new).unwrap();
        assert!(load_at(&new, files.path()).is_none());
        assert!(load_at(dir, files.path()).is_some());
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
        assert_eq!(
            headers(dir, &read_catalog(dir).unwrap()).unwrap().len(),
            MAX_FILES
        );
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
