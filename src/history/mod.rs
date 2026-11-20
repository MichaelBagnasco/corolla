pub mod store;
mod watch;

use std::{
    env,
    ffi::OsString,
    fs::{self, DirBuilder, OpenOptions, TryLockError},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::ExitCode,
};

const USAGE: &str = "Usage: corolla history watch DIRECTORY\n       corolla history log [PATH]";

pub fn database_path() -> io::Result<PathBuf> {
    if let Some(custom) = env::var_os("COROLLA_HISTORY_DB").filter(|s| !s.is_empty()) {
        return std::path::absolute(custom);
    }
    let state = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| {
            io::Error::other("Set XDG_STATE_HOME or COROLLA_HISTORY_DB to store file history")
        })?;
    Ok(state.join("corolla/history/files.dolt"))
}

fn record(directory: &Path) -> io::Result<()> {
    let root = fs::canonicalize(directory).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("watch directory {}: {e}", directory.display()),
        )
    })?;
    if !root.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let path = database_path()?;
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::other(
            "COROLLA_HISTORY_DB must name a database file",
        ));
    };
    // This subcommand runs in a dedicated process. DoltLite inherits this mask
    // for the database and any auxiliary files it creates.
    unsafe { libc::umask(0o077) };
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let db = fs::canonicalize(parent)?.join(name);
    match fs::symlink_metadata(&db) {
        Ok(meta) if !meta.is_file() => {
            return Err(io::Error::other(format!(
                "{} must be a regular file",
                db.display()
            )));
        }
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut lock_path = db.as_os_str().to_owned();
    lock_path.push(".watch-lock");
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(lock_path)?;
    lock.try_lock().map_err(|e| match e {
        TryLockError::WouldBlock => {
            io::Error::other(format!("a watcher already owns {}", db.display()))
        }
        TryLockError::Error(e) => e,
    })?;
    watch::run(&root, &db)
}

fn log(path: Option<&Path>) -> io::Result<()> {
    let db = database_path()?;
    if !db.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "No history at {}. Start recording with `corolla history watch DIRECTORY`.",
                db.display()
            ),
        ));
    }
    let revisions = store::history(&db, path, 100)?;
    let mut out = io::stdout().lock();
    if revisions.is_empty() {
        return writeln!(out, "No recorded changes for this path.");
    }
    for revision in revisions {
        let size = |entry: &Option<store::Metadata>| {
            entry
                .as_ref()
                .map(|s| s.size.to_string())
                .unwrap_or_else(|| "—".into())
        };
        writeln!(
            out,
            "{}  {:.12}  {:8}  {} → {} B  {:?}",
            revision.date,
            revision.commit,
            revision.action,
            size(&revision.before),
            size(&revision.after),
            revision.path
        )?;
    }
    Ok(())
}

pub fn command(args: &[OsString]) -> ExitCode {
    let result = match args {
        [cmd, path] if cmd == "watch" => record(Path::new(path)),
        [cmd] if cmd == "log" => log(None),
        [cmd, path] if cmd == "log" => log(Some(Path::new(path))),
        [flag] if flag == "--help" || flag == "-h" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if args[0] == "log" && e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("corolla history: {e}");
            ExitCode::FAILURE
        }
    }
}
