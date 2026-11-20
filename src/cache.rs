//! Recently viewed scans, including their already measured subtrees.
use crate::{
    patch::{self, Changes},
    scan::Scan,
    snapshots,
};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::path::{Component, Path};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;

const MAX_NODES: usize = 2_000_000;
const MAX_SCANS: usize = 8;

struct Entry {
    scan: Arc<Scan>,
    last_used: u64,
}

pub struct ScanCache {
    // Insertion order determines which overlapping snapshot is newest. Reading
    // an older snapshot only changes its eviction priority.
    entries: Vec<Entry>,
    node_count: usize,
    node_limit: usize,
    clock: u64,
}

impl Default for ScanCache {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            node_count: 0,
            node_limit: MAX_NODES,
            clock: 0,
        }
    }
}

/// Find a canonical absolute directory by walking only children actually
/// measured. A mount omitted by the scanner has no link in this tree.
pub fn locate(scan: &Scan, path: &Path) -> Option<usize> {
    let relative = path.strip_prefix(&scan.root).ok()?;
    let mut id = 0;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        id = *scan.nodes.get(id)?.children.iter().find(|&&child| {
            scan.nodes
                .get(child)
                .is_some_and(|n| n.is_dir && n.name == name)
        })?;
    }
    let node = scan.nodes.get(id)?;
    // An unreadable directory may appear as an empty leaf in a partial scan.
    // An explicit scan of that folder can report the error or try reading it again.
    if !node.is_dir || (id != 0 && node.incomplete) {
        return None;
    }
    Some(id)
}

fn same_directory(scan: &Scan, id: usize, path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| {
        let (dev, ino, _, _) = scan.nodes[id].identity;
        m.is_dir() && (m.dev(), m.ino()) == (dev, ino)
    })
}

impl ScanCache {
    #[cfg(test)]
    pub fn contains(&self, path: &Path) -> bool {
        self.entries
            .iter()
            .rev()
            .any(|e| locate(&e.scan, path).is_some())
    }

    pub fn get(&mut self, path: &Path) -> Option<(Arc<Scan>, usize)> {
        for entry in self.entries.iter_mut().rev() {
            let Some(id) = locate(&entry.scan, path) else {
                continue;
            };
            // A directory can be replaced or shadowed by another mount between
            // scans. Changes to unrelated mounts do not affect this lookup.
            if !same_directory(&entry.scan, id, path)
                || (id != 0 && !same_directory(&entry.scan, 0, &entry.scan.root))
            {
                continue;
            }
            self.clock += 1;
            entry.last_used = self.clock;
            return Some((Arc::clone(&entry.scan), id));
        }
        None
    }

    pub fn insert(&mut self, scan: Arc<Scan>) {
        if let Some(old) = self.entries.iter().position(|e| e.scan.root == scan.root) {
            self.remove(old);
        }
        let count = scan.nodes.len();
        if count == 0 {
            return;
        }
        // The active map may already exceed the budget on a large volume.
        // Keep that one map usable and evict its companions.
        while !self.entries.is_empty()
            && (self.node_count + count > self.node_limit || self.entries.len() >= MAX_SCANS)
        {
            let oldest = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .unwrap()
                .0;
            self.remove(oldest);
        }
        self.node_count += count;
        self.clock += 1;
        self.entries.push(Entry {
            scan,
            last_used: self.clock,
        });
    }

    fn apply(&mut self, changes: &Changes, active: Option<Arc<Scan>>) -> Option<Arc<Scan>> {
        let mut active_result = None;
        self.entries.retain_mut(|entry| {
            let same = active.as_ref().is_some_and(|a| Arc::ptr_eq(a, &entry.scan));
            if changes.affects(&entry.scan.root) {
                let replacement = patched(&entry.scan, changes);
                if same {
                    active_result = Some(replacement.clone());
                }
                if let Some(scan) = replacement {
                    entry.scan = scan;
                } else {
                    return false;
                }
            }
            true
        });
        self.node_count = self.entries.iter().map(|e| e.scan.nodes.len()).sum();
        if let Some(fresh) = &changes.refreshed {
            // Under storage pressure, keep the broadest patched map. It serves
            // both the refreshed folder and its siblings from a single allocation.
            let covering = self
                .entries
                .iter()
                .filter(|e| e.scan.root != fresh.root && locate(&e.scan, &fresh.root).is_some())
                .min_by_key(|e| e.scan.root.components().count());
            let protect = covering.and_then(|entry| {
                (entry.scan.nodes.len() + fresh.nodes.len() > self.node_limit)
                    .then(|| entry.scan.root.clone())
            });
            if let Some(root) = protect {
                while self.entries.len() > 1 && self.node_count > self.node_limit {
                    let oldest = self
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.scan.root != root)
                        .min_by_key(|(_, e)| e.last_used)
                        .unwrap()
                        .0;
                    self.remove(oldest);
                }
            } else {
                // Mark the covering map recently used before fitting a standalone child.
                if let Some(root) = covering.map(|entry| entry.scan.root.clone())
                    && let Some(parent) = self
                        .entries
                        .iter_mut()
                        .find(|entry| entry.scan.root == root)
                {
                    self.clock += 1;
                    parent.last_used = self.clock;
                }
                self.insert(fresh.clone());
            }
        }
        // An active inspector can outlive its cache slot after eviction.
        let active = active?;
        let result = active_result.unwrap_or_else(|| {
            if changes.affects(&active.root) {
                patched(&active, changes)
            } else {
                Some(active.clone())
            }
        });
        result.or_else(|| {
            // A remeasured ancestor can subsume the active standalone snapshot.
            changes
                .refreshed
                .as_ref()
                .filter(|fresh| locate(fresh, &active.root).is_some())
                .cloned()
        })
    }

    fn remove(&mut self, index: usize) {
        self.node_count -= self.entries.remove(index).scan.nodes.len();
    }
}

fn patched(scan: &Scan, changes: &Changes) -> Option<Arc<Scan>> {
    if let Some(fresh) = &changes.refreshed
        && fresh.root == scan.root
        && changes.removed.is_empty()
    {
        return Some(fresh.clone());
    }
    patch::apply(scan, changes).map(Arc::new)
}

pub struct Lookup {
    pub hit: Option<(Arc<Scan>, usize)>,
    pub generation: Option<u64>,
}

#[derive(Default)]
struct Session {
    scans: ScanCache,
    generation: Option<u64>,
    writers: Vec<JoinHandle<()>>,
}

impl Session {
    fn synchronize(&mut self, generation: Option<u64>) {
        if self.generation != generation {
            self.scans = ScanCache::default();
            self.generation = generation;
        }
    }

    fn lookup(&mut self, root: &Path, fresh: bool) -> Lookup {
        let generation = snapshots::generation();
        self.synchronize(generation);
        let hit = if fresh {
            None
        } else {
            self.scans.get(root).or_else(|| {
                let (scan, id) = snapshots::load(root)?;
                self.scans.insert(scan.clone());
                Some((scan, id))
            })
        };
        Lookup { hit, generation }
    }

    fn wait_for_writes(&mut self) {
        for writer in self.writers.drain(..) {
            let _ = writer.join();
        }
    }

    fn accept(&mut self, scan: Arc<Scan>, fresh: bool, expected: Option<u64>) {
        let current = snapshots::generation();
        self.synchronize(current);
        if expected != current {
            return;
        }
        if fresh {
            self.apply(
                &Changes {
                    refreshed: Some(scan),
                    removed: Vec::new(),
                },
                expected,
                None,
            );
        } else {
            self.scans.insert(scan.clone());
            if let Some(epoch) = expected {
                self.writers.retain(|w| !w.is_finished());
                self.writers.push(std::thread::spawn(move || {
                    snapshots::save_if_current(&scan, Some(epoch))
                }));
            }
        }
    }

    fn apply(
        &mut self,
        changes: &Changes,
        expected: Option<u64>,
        active: Option<Arc<Scan>>,
    ) -> Option<Arc<Scan>> {
        // A still-being-encoded parent belongs in this transaction as well.
        self.wait_for_writes();
        if let Some((before, after)) = snapshots::update(changes, expected) {
            self.synchronize(Some(before));
            self.generation = Some(after);
        } else {
            let current = snapshots::generation();
            self.synchronize(current);
            if expected.is_some() && expected != current {
                return active;
            }
        }
        self.scans.apply(changes, active)
    }
}

// One worker owns this window's cache. Subtree walks run independently, and only
// results accepted by the UI enter this queue. Later opens observe earlier commits.
enum Work {
    Lookup(PathBuf, bool, mpsc::Sender<Lookup>),
    Accept(Arc<Scan>, bool, Option<u64>),
    Reconcile(
        Vec<Changes>,
        Option<Arc<Scan>>,
        mpsc::Sender<Option<Arc<Scan>>>,
    ),
    Stop,
}

#[derive(Clone)]
pub struct CacheClient(mpsc::Sender<Work>);
impl CacheClient {
    pub fn lookup(&self, root: &Path, fresh: bool) -> io::Result<Lookup> {
        let (tx, rx) = mpsc::channel();
        self.0
            .send(Work::Lookup(root.into(), fresh, tx))
            .map_err(io::Error::other)?;
        rx.recv().map_err(io::Error::other)
    }

    pub fn accept(&self, scan: Arc<Scan>, fresh: bool, epoch: Option<u64>) {
        let _ = self.0.send(Work::Accept(scan, fresh, epoch));
    }

    pub fn reconcile(
        &self,
        changes: Vec<Changes>,
        active: Option<Arc<Scan>>,
    ) -> io::Result<Option<Arc<Scan>>> {
        if changes.is_empty() {
            return Ok(active);
        }
        let (tx, rx) = mpsc::channel();
        self.0
            .send(Work::Reconcile(changes, active, tx))
            .map_err(io::Error::other)?;
        rx.recv().map_err(io::Error::other)
    }
}

pub struct CacheWorker {
    pub client: CacheClient,
    thread: Option<JoinHandle<()>>,
}
impl CacheWorker {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut session = Session::default();
            for work in rx {
                match work {
                    Work::Lookup(root, fresh, reply) => {
                        let _ = reply.send(session.lookup(&root, fresh));
                    }
                    Work::Accept(scan, fresh, epoch) => session.accept(scan, fresh, epoch),
                    Work::Reconcile(updates, mut active, reply) => {
                        for update in updates {
                            active = session.apply(&update, None, active);
                        }
                        let _ = reply.send(active);
                    }
                    Work::Stop => break,
                }
            }
            session.wait_for_writes();
        });
        Self {
            client: CacheClient(tx),
            thread: Some(worker),
        }
    }
}
impl Drop for CacheWorker {
    fn drop(&mut self) {
        let _ = self.client.0.send(Work::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Tree(PathBuf);

    impl Tree {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root =
                std::env::temp_dir().join(format!("corolla-cache-{}-{nonce}", std::process::id()));
            fs::create_dir_all(root.join("one/child")).unwrap();
            fs::create_dir(root.join("two")).unwrap();
            Self(root)
        }

        fn scan(&self, path: &str) -> Arc<Scan> {
            Arc::new(
                crate::scan::scan(&self.0.join(path), &AtomicBool::new(false), |_, _| {}).unwrap(),
            )
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn newest_subtree_wins_even_after_older_snapshot_is_read() {
        let tree = Tree::new();
        let mut cache = ScanCache::default();
        cache.insert(tree.scan("one"));
        let parent = tree.scan("");
        cache.insert(Arc::clone(&parent));
        let child = tree.0.join("one/child");
        let (found, id) = cache.get(&child).unwrap();
        assert!(Arc::ptr_eq(&parent, &found));
        assert_eq!(found.path(id), child);

        let newest = tree.scan("one");
        cache.insert(Arc::clone(&newest));
        cache.get(&tree.0.join("two")).unwrap(); // Promote parent for eviction only.
        assert!(Arc::ptr_eq(&cache.get(&child).unwrap().0, &newest));

        cache.insert(tree.scan("two"));
        cache.apply(
            &Changes {
                refreshed: None,
                removed: vec![tree.0.join("one")],
            },
            None,
        );
        assert!(!cache.contains(&child));
        assert!(cache.contains(&tree.0));
        assert!(cache.contains(&tree.0.join("two")));
    }

    #[test]
    fn refreshed_child_keeps_oversized_parent_and_siblings() {
        let tree = Tree::new();
        let mut cache = ScanCache {
            node_limit: 3,
            ..Default::default()
        };
        cache.insert(tree.scan(""));
        fs::write(tree.0.join("one/new"), "fresh").unwrap();
        cache.apply(
            &Changes {
                refreshed: Some(tree.scan("one")),
                removed: vec![],
            },
            None,
        );
        assert!(cache.contains(&tree.0.join("two")));
        let (scan, id) = cache.get(&tree.0.join("one")).unwrap();
        assert_eq!(scan.root, tree.0);
        assert_eq!(scan.nodes[id].children.len(), 2);
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn omitted_children_and_replaced_directories_do_not_hit() {
        let tree = Tree::new();
        let mut cache = ScanCache::default();
        let mut parent = tree.scan("");
        let p = Arc::get_mut(&mut parent).unwrap();
        let omit = locate(p, &tree.0.join("two")).unwrap();
        p.nodes[0].children.retain(|&id| id != omit);
        cache.insert(parent);
        assert!(!cache.contains(&tree.0.join("two")));
        assert!(cache.get(&tree.0.join("two")).is_none());
        let one = tree.0.join("one");
        fs::rename(&one, tree.0.join("old")).unwrap();
        fs::create_dir(&one).unwrap();
        assert!(cache.get(&one).is_none());
    }

    #[test]
    fn partial_scan_preserves_measured_empty_siblings() {
        let tree = Tree::new();
        let mut scan = tree.scan("");
        let scan = Arc::get_mut(&mut scan).unwrap();
        let bad = tree.0.join("one/child");
        let id = locate(scan, &bad).unwrap();
        scan.nodes[id].incomplete = true;
        scan.skipped = 1;
        assert!(locate(scan, &bad).is_none());
        assert!(locate(scan, &tree.0.join("two")).is_some());
    }

    #[test]
    fn node_budget_evicts_by_recent_use_and_keeps_one_oversize_scan() {
        let tree = Tree::new();
        let mut cache = ScanCache {
            node_limit: 3,
            ..Default::default()
        };
        cache.insert(tree.scan("one")); // 2 nodes
        cache.insert(tree.scan("two")); // 1 node
        cache.get(&tree.0.join("one")).unwrap();
        let child = tree.0.join("one/child");
        cache.insert(tree.scan("one/child"));
        assert!(cache.contains(&tree.0.join("one")));
        assert!(cache.contains(&child));
        assert!(!cache.contains(&tree.0.join("two")));
        cache.insert(tree.scan("")); // 4 nodes
        assert!(cache.get(&tree.0).is_some());
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.node_count, 4);
        cache.insert(tree.scan("two"));
        assert!(!cache.contains(&tree.0));
        assert_eq!(cache.node_count, 1);
    }
}
