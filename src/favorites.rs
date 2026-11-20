use std::{
    env, fs,
    fs::File,
    io::{self, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

fn directory() -> io::Result<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|state| state.join("corolla"))
        .ok_or_else(|| io::Error::other("No state directory is available"))
}

fn read_at(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let bytes = match fs::read(dir.join("favorites")) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => result?,
    };
    Ok(bytes
        .split(|&b| b == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(std::ffi::OsStr::from_bytes(path)))
        .collect())
}

pub fn load() -> Vec<PathBuf> {
    directory()
        .and_then(|dir| read_at(&dir))
        .unwrap_or_default()
}

pub fn set(path: &Path, pinned: bool) -> io::Result<Vec<PathBuf>> {
    let dir = directory()?;
    fs::create_dir_all(&dir)?;
    let lock = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("favorites.lock"))?;
    lock.lock()?;
    let mut folders = read_at(&dir)?;
    folders.retain(|saved| saved != path);
    if pinned {
        folders.push(path.into());
    }
    let mut staging = tempfile::NamedTempFile::new_in(&dir)?;
    for folder in &folders {
        staging.write_all(folder.as_os_str().as_bytes())?;
        staging.write_all(&[0])?;
    }
    staging.as_file().sync_all()?;
    staging.persist(dir.join("favorites"))?;
    Ok(folders)
}
