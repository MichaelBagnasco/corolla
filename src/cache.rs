//! Recently viewed scans, including their already measured subtrees.
use crate::scan::Scan;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::sync::Arc;

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
    /// Fast presence hint for overview rows. Opening a row verifies its identity.
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

    /// Both a parent map and a separately scanned child can contain stale
    /// data after an operation. This also works after the path was removed.
    pub fn invalidate(&mut self, path: &Path) {
        let mut kept_nodes = 0;
        self.entries.retain(|e| {
            let keep = !e.scan.root.starts_with(path) && !path.starts_with(&e.scan.root);
            if keep {
                kept_nodes += e.scan.nodes.len();
            }
            keep
        });
        self.node_count = kept_nodes;
    }

    fn remove(&mut self, index: usize) {
        self.node_count -= self.entries.remove(index).scan.nodes.len();
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
        cache.invalidate(&tree.0.join("one"));
        assert!(!cache.contains(&child));
        assert!(!cache.contains(&tree.0));
        assert!(cache.contains(&tree.0.join("two")));
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
