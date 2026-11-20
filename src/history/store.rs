//! A watched path is a raw Unix pathname (a BLOB primary key). The files table
//! contains the current metadata; old values and deletions live in DoltLite commits.
use super::watch::{Batch, Entry, Kind};
use serde_json::Value;
use std::{
    env,
    ffi::OsString,
    fmt::Write as _,
    fs::{self, File},
    io::{self, BufWriter, Write},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
};

const AUTHOR: &str = "'Corolla <watcher@localhost>'";
const COLUMNS: &str = "path,kind,bytes,size,mtime_ns,ctime_ns,mode,dev,ino";
const FILE_SCHEMA: &str = "(path BLOB PRIMARY KEY, kind TEXT NOT NULL, bytes INTEGER NOT NULL, size INTEGER NOT NULL, mtime_ns INTEGER NOT NULL, ctime_ns INTEGER NOT NULL, mode INTEGER NOT NULL, dev INTEGER NOT NULL, ino INTEGER NOT NULL)";

pub struct Store {
    db: PathBuf,
    root: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Metadata {
    pub kind: String,
    pub bytes: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub mode: u32,
    pub dev: u64,
    pub ino: u64,
}

#[derive(Debug)]
pub struct Revision {
    pub path: PathBuf,
    pub action: String,
    pub commit: String,
    pub date: String,
    pub before: Option<Metadata>,
    pub after: Option<Metadata>,
}

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn blob(bytes: &[u8]) -> String {
    let mut sql = String::with_capacity(3 + bytes.len() * 2);
    sql.push_str("X'");
    for b in bytes {
        write!(sql, "{b:02x}").expect("format into string");
    }
    sql.push('\'');
    sql
}

fn unhex(value: &str) -> io::Result<PathBuf> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || !bytes.len().is_multiple_of(2)
        || !bytes.iter().all(u8::is_ascii_hexdigit)
    {
        return Err(invalid("Invalid pathname in history database"));
    }
    let raw = (0..bytes.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).unwrap())
        .collect();
    Ok(PathBuf::from(OsString::from_vec(raw)))
}

fn shell(db: &Path, readonly: bool) -> Command {
    let mut command =
        Command::new(env::var_os("DOLTLITE_BIN").unwrap_or_else(|| "doltlite".into()));
    command.args(["-noinit", "-nofollow", "-bail", "-batch", "-json"]);
    if readonly {
        command.arg("-readonly");
    } else {
        command.arg("-ifexists");
    }
    command.arg("--").arg(db);
    command
}

fn executable_error(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::NotFound {
        io::Error::other(
            "DoltLite is required for file history. Install doltlite on PATH or set DOLTLITE_BIN.",
        )
    } else {
        error
    }
}

fn run(command: &mut Command) -> io::Result<Vec<u8>> {
    check_output(command.output().map_err(executable_error)?)
}

fn check_output(output: Output) -> io::Result<Vec<u8>> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let message = String::from_utf8_lossy(&output.stderr);
        Err(io::Error::other(format!(
            "DoltLite failed: {}",
            message.trim()
        )))
    }
}

fn query(db: &Path, sql: &str) -> io::Result<Vec<Value>> {
    if !fs::symlink_metadata(db)?.is_file() {
        return Err(invalid("The history database must be a regular file"));
    }
    let bytes = run(shell(db, true).arg(sql).stdin(Stdio::null()))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

pub fn root(db: &Path) -> io::Result<PathBuf> {
    let result = query(
        db,
        "SELECT hex(root) AS root FROM dolt_at_corolla_history('HEAD') WHERE id=1 AND format=1",
    )?;
    let name = result
        .first()
        .and_then(|row| row["root"].as_str())
        .ok_or_else(|| invalid("Unrecognized Corolla history database"))?;
    let path = unhex(name)?;
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(invalid("Invalid watched root in history database"));
    }
    Ok(path)
}

// Prefix ranges use raw bytes so "%" and "_" remain ordinary filename
// characters and an entry named "music2" is outside the subtree "music".
fn descendant_range(path: &Path) -> (String, String) {
    let mut prefix = path.as_os_str().as_bytes().to_vec();
    if !prefix.ends_with(b"/") {
        prefix.push(b'/');
    }
    let low = blob(&prefix);
    *prefix.last_mut().unwrap() = b'0';
    (low, blob(&prefix))
}

fn subtree(path: &Path, column: &str) -> String {
    let (low, high) = descendant_range(path);
    format!(
        "({column} = {} OR ({column} >= {low} AND {column} < {high}))",
        blob(path.as_os_str().as_bytes())
    )
}

impl Store {
    pub fn open(db: &Path, watched: &Path) -> io::Result<Self> {
        let watched = watched.canonicalize()?;
        if !watched.is_dir() {
            return Err(invalid("The watched root must be a directory"));
        }
        let db = std::path::absolute(db)?;
        match fs::symlink_metadata(&db) {
            Ok(_) => {
                if root(&db)? != watched {
                    return Err(invalid(
                        "This history database belongs to a different watched root",
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let parent = db
                    .parent()
                    .ok_or_else(|| invalid("History database needs a parent directory"))?;
                // DoltLite places its own lock file alongside the database. Keep
                // initialization and its sidecars together until the commit is durable.
                let stage = tempfile::Builder::new()
                    .prefix(".corolla-init-")
                    .tempdir_in(parent)?;
                let temp = tempfile::NamedTempFile::new_in(stage.path())?;
                let sql = format!(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE corolla_history(id INTEGER PRIMARY KEY, format INTEGER NOT NULL, root BLOB NOT NULL);
                     INSERT INTO corolla_history VALUES(1,1,{});
                     CREATE TABLE files{FILE_SCHEMA};
                     SELECT dolt_commit('-Am','Initialize Corolla file history','--author',{AUTHOR});
                     COMMIT;",blob(watched.as_os_str().as_bytes()));
                run(shell(temp.path(), false).arg(sql).stdin(Stdio::null()))?;
                temp.as_file().sync_all()?;
                temp.persist_noclobber(&db)?;
                File::open(parent)?.sync_all()?;
            }
            Err(e) => return Err(e),
        }
        Ok(Self { db, root: watched })
    }

    pub fn record(&self, batch: &Batch, message: &str) -> io::Result<()> {
        let valid_path = |p: &Path| {
            p.starts_with(&self.root) && !p.components().any(|c| matches!(c, Component::ParentDir))
        };
        if batch.root != self.root
            || !batch
                .scopes
                .iter()
                .chain(&batch.preserved)
                .all(|p| valid_path(p))
            || !batch.entries.iter().all(|e| valid_path(&e.path))
        {
            return Err(invalid("Observation outside this store's watched root"));
        }

        // Pipe the baseline incrementally. It can exceed argv limits and even
        // the process's temporary directory may be inside the watched tree.
        let mut child = shell(&self.db, false)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(executable_error)?;
        let stdin = child.stdin.take().expect("piped DoltLite input");
        std::thread::scope(|scope| {
            let writer = scope.spawn(move || {
                let mut sql = BufWriter::new(stdin);
                self.write_batch(&mut sql, batch, message)?;
                sql.flush()
            });
            let output = child.wait_with_output()?;
            let written = writer
                .join()
                .map_err(|_| io::Error::other("DoltLite writer stopped"))?;
            check_output(output)?;
            written
        })
    }

    fn write_batch(&self, sql: &mut impl Write, batch: &Batch, message: &str) -> io::Result<()> {
        writeln!(sql,"BEGIN IMMEDIATE;
            CREATE TEMP TABLE guard(same_root INTEGER CHECK(same_root=1), clean_working_tree INTEGER CHECK(clean_working_tree=1));
            INSERT INTO guard VALUES(EXISTS(SELECT 1 FROM corolla_history WHERE id=1 AND format=1 AND root={}), NOT EXISTS(SELECT 1 FROM dolt_status));
            CREATE TEMP TABLE incoming{FILE_SCHEMA};
            CREATE TEMP TABLE preserved(path BLOB PRIMARY KEY, lo BLOB, hi BLOB);",blob(self.root.as_os_str().as_bytes()))?;

        for entries in batch.entries.chunks(128) {
            write!(sql, "INSERT OR REPLACE INTO incoming VALUES ")?;
            for (i, entry) in entries.iter().enumerate() {
                if i > 0 {
                    write!(sql, ",")?;
                }
                write_entry(sql, entry)?;
            }
            writeln!(sql, ";")?;
        }
        for path in &batch.preserved {
            let (lo, hi) = descendant_range(path);
            writeln!(
                sql,
                "INSERT OR IGNORE INTO preserved VALUES({},{lo},{hi});",
                blob(path.as_os_str().as_bytes())
            )?;
        }
        for path in &batch.scopes {
            writeln!(sql,"DELETE FROM files WHERE {}
                AND NOT EXISTS(SELECT 1 FROM incoming AS i WHERE i.path=files.path)
                AND NOT EXISTS(SELECT 1 FROM preserved AS p WHERE files.path=p.path OR (files.path>=p.lo AND files.path<p.hi));",subtree(path,"files.path"))?;
        }
        writeln!(sql,"INSERT INTO files({COLUMNS}) SELECT {COLUMNS} FROM incoming WHERE true
             ON CONFLICT(path) DO UPDATE SET (kind,bytes,size,mtime_ns,ctime_ns,mode,dev,ino) =
                 (excluded.kind,excluded.bytes,excluded.size,excluded.mtime_ns,excluded.ctime_ns,excluded.mode,excluded.dev,excluded.ino)
             WHERE (files.kind,files.bytes,files.size,files.mtime_ns,files.ctime_ns,files.mode,files.dev,files.ino)
                 IS NOT (excluded.kind,excluded.bytes,excluded.size,excluded.mtime_ns,excluded.ctime_ns,excluded.mode,excluded.dev,excluded.ino);
             SELECT dolt_commit('--skip-empty','-am',CAST({} AS TEXT),'--author',{AUTHOR});
             COMMIT;",blob(message.as_bytes()))?;
        Ok(())
    }
}

fn write_entry(out: &mut impl Write, entry: &Entry) -> io::Result<()> {
    let kind = match entry.kind {
        Kind::File => "file",
        Kind::Directory => "directory",
        Kind::Symlink => "symlink",
        Kind::Other => "other",
    };
    // SQLite INTEGER is signed. Preserve all 64 bits of Linux identifiers.
    write!(
        out,
        "({},'{kind}',{},{},{},{},{},{},{})",
        blob(entry.path.as_os_str().as_bytes()),
        entry.bytes as i64,
        entry.size as i64,
        entry.mtime_ns,
        entry.ctime_ns,
        entry.mode,
        entry.dev as i64,
        entry.ino as i64
    )
}

fn metadata(row: &Value, side: &str) -> io::Result<Option<Metadata>> {
    let Some(kind) = row[format!("{side}_kind")].as_str() else {
        return Ok(None);
    };
    let number = |name| {
        row[format!("{side}_{name}")]
            .as_i64()
            .ok_or_else(|| invalid("Invalid metadata in DoltLite result"))
    };
    Ok(Some(Metadata {
        kind: kind.into(),
        bytes: number("bytes")? as u64,
        size: number("size")? as u64,
        mtime_ns: number("mtime_ns")?,
        ctime_ns: number("ctime_ns")?,
        mode: number("mode")? as u32,
        dev: number("dev")? as u64,
        ino: number("ino")? as u64,
    }))
}

pub fn history(db: &Path, path: Option<&Path>, limit: usize) -> io::Result<Vec<Revision>> {
    let watched = root(db)?;
    let where_path = if let Some(p) = path {
        let mut absolute = PathBuf::new();
        for part in watched.join(p).components() {
            match part {
                Component::ParentDir => {
                    absolute.pop();
                }
                part => absolute.push(part.as_os_str()),
            }
        }
        if !absolute.starts_with(&watched) && !watched.starts_with(&absolute) {
            return Ok(Vec::new());
        }
        format!(
            " AND {}",
            subtree(&absolute, "coalesce(d.to_path,d.from_path)")
        )
    } else {
        String::new()
    };
    let columns = ["to", "from"]
        .iter()
        .flat_map(|side| {
            [
                "kind", "bytes", "size", "mtime_ns", "ctime_ns", "mode", "dev", "ino",
            ]
            .map(|c| format!("d.{side}_{c}"))
        })
        .collect::<Vec<_>>()
        .join(",");
    // dolt_log walks newest first even when several commits share a timestamp.
    // The cross joins walk only each commit's first-parent diff, stopping once
    // the requested number of matching rows has been returned.
    let sql = format!("SELECT l.commit_hash AS hash,strftime('%Y-%m-%dT%H:%M:%SZ',l.date) AS date,
              hex(coalesce(d.to_path,d.from_path)) AS path,d.diff_type AS action,{columns}
              FROM dolt_log AS l CROSS JOIN dolt_commit_ancestors AS a
              CROSS JOIN dolt_diff_files(a.parent_hash,l.commit_hash) AS d
              WHERE a.commit_hash=l.commit_hash AND a.parent_index=0 AND a.parent_hash IS NOT NULL{where_path}
              LIMIT {}",limit.min(10_000));
    query(db, &sql)?
        .iter()
        .map(|row| {
            let string = |name: &str| {
                row[name]
                    .as_str()
                    .ok_or_else(|| invalid("Invalid revision in DoltLite result"))
            };
            Ok(Revision {
                path: unhex(string("path")?)?,
                action: string("action")?.into(),
                commit: string("hash")?.into(),
                date: string("date")?.into(),
                before: metadata(row, "from")?,
                after: metadata(row, "to")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires the official doltlite CLI on PATH or in DOLTLITE_BIN"]
    fn versioned_metadata_preserves_scope_raw_paths_and_deletions() {
        let base = tempfile::tempdir().unwrap();
        let watched = base.path().join("watched");
        fs::create_dir(&watched).unwrap();
        let db = base.path().join("history.dolt");
        let store = Store::open(&db, &watched).unwrap();
        assert_eq!(
            fs::read_dir(base.path()).unwrap().count(),
            2,
            "unexpected initialization sidecar"
        );
        assert_eq!(root(&db).unwrap(), watched);
        let name = OsString::from_vec(b"x%_'\xff\n.in".to_vec());
        let sub = watched.join("music");
        let file = sub.join(&name);
        let neighbor = watched.join("music2").join(&name);
        let entry = |path: &Path, size| Entry {
            path: path.to_owned(),
            kind: Kind::File,
            size,
            bytes: 4096,
            mode: 0o100644,
            dev: 42,
            ino: u64::MAX - 120,
            mtime_ns: 100,
            ctime_ns: 200,
        };
        let first = Batch {
            root: watched.clone(),
            scopes: vec![watched.clone()],
            preserved: vec![],
            entries: vec![entry(&file, 2), entry(&neighbor, 9)],
        };
        store.record(&first, "baseline").unwrap();
        let commits = || {
            query(&db, "SELECT count(*) AS n FROM dolt_log").unwrap()[0]["n"]
                .as_u64()
                .unwrap()
        };
        let n = commits();
        store.record(&first, "still unchanged").unwrap();
        assert_eq!(commits(), n);
        let next = Batch {
            root: watched.clone(),
            scopes: vec![watched.clone()],
            preserved: vec![watched.join("music2")],
            entries: vec![entry(&file, 8)],
        };
        store.record(&next, "append").unwrap();
        store
            .record(
                &Batch {
                    root: watched.clone(),
                    scopes: vec![sub.clone()],
                    preserved: vec![],
                    entries: vec![],
                },
                "remove",
            )
            .unwrap();
        let rows = history(&db, Some(Path::new("music")), 3).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.action.as_str()).collect::<Vec<_>>(),
            ["removed", "modified", "added"]
        );
        assert!(
            rows.iter()
                .all(|r| r.path == file && r.date.ends_with('Z') && !r.commit.is_empty())
        );
        assert!(rows[0].after.is_none());
        assert_eq!(rows[0].before.as_ref().unwrap().size, 8);
        assert_eq!(rows[1].before.as_ref().unwrap().size, 2);
        assert_eq!(rows[1].after.as_ref().unwrap().ino, u64::MAX - 120);
        assert_eq!(history(&db, Some(&file), 1).unwrap().len(), 1);
        assert_eq!(history(&db, Some(&neighbor), 10).unwrap().len(), 1);
        assert!(Store::open(&db, base.path()).is_err());
    }

    #[test]
    #[ignore = "requires the official doltlite CLI on PATH or in DOLTLITE_BIN"]
    fn unrelated_databases_and_missing_paths_remain_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.dolt");
        assert!(history(&missing, None, 5).is_err());
        assert!(!missing.exists());
        let other = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        run(shell(other.path(),false).arg("CREATE TABLE other(id INTEGER PRIMARY KEY); SELECT dolt_commit('-Am','init','--author','Unit <unit@localhost>');")).unwrap();
        let bytes = fs::read(other.path()).unwrap();
        assert!(Store::open(other.path(), dir.path()).is_err());
        assert_eq!(fs::read(other.path()).unwrap(), bytes);
    }
}
