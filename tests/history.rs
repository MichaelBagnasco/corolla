use std::{
    env, fs,
    path::Path,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

const COROLLA: &str = env!("CARGO_BIN_EXE_corolla");

#[test]
fn snapshot_discovery_works_without_recording() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("unused.dolt");
    let output = Command::new(COROLLA)
        .env("COROLLA_HISTORY_DB", &db)
        .env("DOLTLITE_BIN", tmp.path().join("absent"))
        .args(["history", "snapshots", "/proc/version"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("non-Btrfs"));
    assert!(!db.exists());
}

struct Watcher(Child);

impl Watcher {
    fn start(root: &Path, db: &Path) -> Self {
        Self(
            Command::new(COROLLA)
                .env("COROLLA_HISTORY_DB", db)
                .args(["history", "watch"])
                .arg(root)
                .spawn()
                .unwrap(),
        )
    }

    fn wait_for_change(&mut self, db: &Path, path: &Path, action: &str) {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            assert_eq!(self.0.try_wait().unwrap(), None, "watcher exited early");
            let report = Command::new(COROLLA)
                .env("COROLLA_HISTORY_DB", db)
                .args(["history", "log"])
                .arg(path)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&report.stdout);
            if report.status.success()
                && stdout
                    .lines()
                    .next()
                    .is_some_and(|line| line.split_whitespace().any(|s| s == action))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "expected {action} for {path:?}, got {stdout} {}",
                String::from_utf8_lossy(&report.stderr),
            );
            thread::sleep(Duration::from_millis(75));
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires the DoltLite CLI"]
fn external_edits_persist_in_dolt_commits_across_watcher_restarts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("watched folder");
    fs::create_dir(&root).unwrap();
    let db = root.join("recorder/files.dolt");
    let baseline = root.join("existing.txt");
    fs::write(&baseline, b"baseline").unwrap();

    let mut watcher = Watcher::start(&root, &db);
    watcher.wait_for_change(&db, &baseline, "added");
    let duplicate = Command::new(COROLLA)
        .env("COROLLA_HISTORY_DB", &db)
        .args(["history", "watch"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already owns"));

    let folder = root.join("nested");
    let file = folder.join("it's new.txt");
    let renamed = folder.join("moved.txt");
    fs::create_dir(&folder).unwrap();
    fs::write(&file, b"abcd").unwrap();
    watcher.wait_for_change(&db, &file, "added");
    fs::write(&file, b"efgh").unwrap();
    watcher.wait_for_change(&db, &file, "modified");
    fs::rename(&file, &renamed).unwrap();
    watcher.wait_for_change(&db, &file, "removed");
    watcher.wait_for_change(&db, &renamed, "added");
    fs::remove_file(&renamed).unwrap();
    watcher.wait_for_change(&db, &renamed, "removed");

    drop(watcher);
    fs::remove_file(&baseline).unwrap();
    let mut restarted = Watcher::start(&root, &db);
    restarted.wait_for_change(&db, &baseline, "removed");
    restarted.wait_for_change(&db, &renamed, "removed");

    let commit_count = || {
        let output = Command::new(env::var_os("DOLTLITE_BIN").unwrap_or_else(|| "doltlite".into()))
            .args(["-readonly", "-batch", "-noheader"])
            .arg(&db)
            .arg("SELECT count(*) FROM dolt_log;")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap()
    };
    let count = commit_count();
    assert!(
        count >= 6,
        "expected persisted DoltLite commits, got {count}"
    );
    thread::sleep(Duration::from_secs(2));
    assert_eq!(commit_count(), count, "watcher recorded its own writes");
}
