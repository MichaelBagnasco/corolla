//! Edit measured branches without traversing the filesystem again.
use crate::scan::{Node, Scan, can_cover};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[derive(Default, Clone)]
pub struct Changes {
    pub refreshed: Option<Arc<Scan>>,
    pub removed: Vec<PathBuf>,
}

impl Changes {
    pub fn affects(&self, root: &Path) -> bool {
        self.removed
            .iter()
            .any(|p| p.starts_with(root) || root.starts_with(p))
            || self
                .refreshed
                .as_ref()
                .is_some_and(|s| can_cover(&s.root, root) || can_cover(root, &s.root))
    }

    /// An ancestor of another mount may have a lexical overlap but no measured
    /// entry at the path. Callers holding an existing snapshot can keep it intact.
    pub fn touches(&self, scan: &Scan) -> bool {
        self.removed.iter().any(|p| scan.root.starts_with(p))
            || !omitted(scan, &self.removed).is_empty()
            || self.refreshed.as_ref().is_some_and(|s| {
                can_cover(&s.root, &scan.root)
                    || (can_cover(&scan.root, &s.root)
                        && s.root
                            .parent()
                            .and_then(|p| entry(scan, p))
                            .is_some_and(|id| scan.nodes[id].is_dir))
            })
    }
}

// Unlike an open-from-cache lookup, a patch may target a file or an unreadable directory.
fn entry(scan: &Scan, path: &Path) -> Option<usize> {
    let mut id = 0;
    for component in path.strip_prefix(&scan.root).ok()?.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        id = *scan.nodes[id]
            .children
            .iter()
            .find(|&&i| scan.nodes[i].name == name)?;
    }
    Some(id)
}

fn omitted(scan: &Scan, removed: &[PathBuf]) -> HashSet<usize> {
    if removed.len() <= 1 {
        return removed.iter().filter_map(|p| entry(scan, p)).collect();
    }
    // Index the requested components so a bulk cleanup visits each directory's
    // child list once, even when thousands of siblings have been selected.
    #[derive(Default)]
    struct Target<'a> {
        children: HashMap<&'a OsStr, usize>,
        remove: bool,
    }

    let mut targets = vec![Target::default()];
    for path in removed {
        let Ok(relative) = path.strip_prefix(&scan.root) else {
            continue;
        };
        let mut index = 0;
        for component in relative.components() {
            let name = component.as_os_str();
            if let Some(&child) = targets[index].children.get(name) {
                index = child;
            } else {
                let child = targets.len();
                targets[index].children.insert(name, child);
                targets.push(Target::default());
                index = child;
            }
        }
        targets[index].remove = true;
    }
    let mut ids = HashSet::new();
    let mut pending = vec![(0, 0)];
    while let Some((id, key)) = pending.pop() {
        let target: &Target<'_> = &targets[key];
        if target.remove {
            ids.insert(id);
        } else if !target.children.is_empty() {
            for &child in &scan.nodes[id].children {
                if let Some(&key) = target.children.get(scan.nodes[child].name.as_os_str()) {
                    pending.push((child, key));
                }
            }
        }
    }
    ids
}

fn direct_bytes(scan: &Scan, node: &Node) -> u64 {
    node.children.iter().fold(node.bytes, |bytes, &child| {
        bytes.saturating_sub(scan.nodes[child].bytes)
    })
}

// Older snapshots record a total error count and a flag per directory. Preserve the
// total when all affected error-bearing directories survive. Otherwise report the
// known lower bound for the surviving directories.
fn remaining_skips(scan: &Scan, surviving: usize) -> u64 {
    if surviving == scan.nodes.iter().filter(|n| n.incomplete).count() {
        scan.skipped
    } else {
        surviving as u64
    }
}

fn reconcile_links(nodes: &mut [Node], old: &Scan, fresh: Option<&Scan>, fresh_ids: Range<usize>) {
    let key = |n: &Node| (n.identity.0, n.identity.1);
    // A zero-byte entry can be a hard link whose counted sibling just disappeared.
    // A separately scanned subtree can also bring its own counted copy of a file.
    let mut allocation: HashMap<_, u64> = nodes
        .iter()
        .filter(|n| !n.is_dir && n.bytes == 0)
        .map(|n| (key(n), 0))
        .collect();
    for n in old.nodes.iter().filter(|n| !n.is_dir && n.bytes > 0) {
        if let Some(bytes) = allocation.get_mut(&key(n)) {
            *bytes = (*bytes).max(n.bytes);
        }
    }
    if let Some(fresh) = fresh {
        for n in fresh.nodes.iter().filter(|n| !n.is_dir) {
            allocation.insert(key(n), 0);
        }
        for n in fresh.nodes.iter().filter(|n| !n.is_dir && n.bytes > 0) {
            allocation.insert(key(n), n.bytes);
        }
    }
    // Preserve the visible owner whenever it survived, then transfer any unclaimed
    // blocks to a surviving alias. Each inode contributes at most one allocation.
    let (before, tail) = nodes.split_at_mut(fresh_ids.start);
    let (replaced, after) = tail.split_at_mut(fresh_ids.len());
    for n in before
        .iter_mut()
        .chain(after)
        .chain(replaced)
        .filter(|n| !n.is_dir && n.bytes > 0)
    {
        if let Some(bytes) = allocation.get_mut(&key(n)) {
            n.bytes = std::mem::take(bytes);
        }
    }
    for n in nodes.iter_mut().filter(|n| !n.is_dir && n.bytes == 0) {
        if let Some(bytes) = allocation.get_mut(&key(n)) {
            n.bytes = std::mem::take(bytes);
        }
    }
}

pub fn apply(scan: &Scan, changes: &Changes) -> Option<Scan> {
    if changes
        .removed
        .iter()
        .any(|path| scan.root.starts_with(path))
    {
        return None;
    }

    let fresh = changes.refreshed.as_deref();
    if let Some(f) = fresh {
        if f.root == scan.root {
            if changes.removed.is_empty() {
                return Some(f.clone());
            }
            return apply(
                f,
                &Changes {
                    refreshed: None,
                    removed: changes.removed.clone(),
                },
            );
        }
        if can_cover(&f.root, &scan.root) {
            return None; // The newer ancestor now answers subtree requests.
        }
    }

    let mut skip_old = omitted(scan, &changes.removed);
    let mut graft = None;
    if let Some(f) = fresh
        && can_cover(&scan.root, &f.root)
        && !changes.removed.iter().any(|p| f.root.starts_with(p))
        && let Some(parent) = f.root.parent().and_then(|p| entry(scan, p))
        && scan.nodes[parent].is_dir
    {
        graft = Some((parent, f));
        if let Some(id) = entry(scan, &f.root) {
            skip_old.insert(id);
        }
    }

    if skip_old.is_empty() && graft.is_none() {
        return Some(scan.clone());
    }

    let skip_fresh = graft
        .map(|(_, f)| omitted(f, &changes.removed))
        .unwrap_or_default();
    let mut incomplete = [0; 2];
    let mut fresh_ids = 0..0;
    let mut nodes: Vec<Node> = Vec::with_capacity(scan.nodes.len());
    // Iterative preorder preserves the codec's parent-before-child invariant and
    // avoids a call stack proportional to directory depth.
    let mut pending = vec![(false, 0, None)];
    while let Some((new, source_id, parent)) = pending.pop() {
        let (source, skip) = if new {
            (graft.unwrap().1, &skip_fresh)
        } else {
            (scan, &skip_old)
        };
        if skip.contains(&source_id) {
            continue;
        }
        let original = &source.nodes[source_id];
        let id = nodes.len();
        if new {
            if source_id == 0 {
                fresh_ids.start = id;
            }
            fresh_ids.end = id + 1;
        }
        nodes.push(Node {
            name: original.name.clone(),
            bytes: direct_bytes(source, original),
            is_dir: original.is_dir,
            incomplete: original.incomplete,
            identity: original.identity,
            children: Vec::with_capacity(original.children.len()),
            parent,
        });
        if let Some(parent) = parent {
            nodes[parent].children.push(id);
        }
        incomplete[usize::from(new)] += usize::from(original.incomplete);
        if !new && graft.is_some_and(|(parent_id, _)| source_id == parent_id) {
            pending.push((true, 0, Some(id)));
        }
        pending.extend(
            original
                .children
                .iter()
                .rev()
                .map(|&child| (new, child, Some(id))),
        );
    }

    if nodes.len() < nodes.capacity() / 2 {
        nodes.shrink_to_fit();
    }
    let source = graft.map(|(_, f)| f);
    reconcile_links(&mut nodes, scan, source, fresh_ids);
    for id in (0..nodes.len()).rev() {
        if nodes[id].children.is_empty() {
            continue;
        }
        let mut children = std::mem::take(&mut nodes[id].children);
        children.sort_unstable_by(|&a, &b| {
            nodes[b]
                .bytes
                .cmp(&nodes[a].bytes)
                .then_with(|| nodes[a].name.cmp(&nodes[b].name))
        });
        nodes[id].bytes = children.iter().fold(nodes[id].bytes, |bytes, &child| {
            bytes.saturating_add(nodes[child].bytes)
        });
        nodes[id].children = children;
    }
    let skipped = remaining_skips(scan, incomplete[0])
        + source.map_or(0, |f| remaining_skips(f, incomplete[1]));
    Some(Scan {
        root: scan.root.clone(),
        nodes,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::scan;
    use std::{fs, sync::atomic::AtomicBool};
    use tempfile::tempdir;

    fn measure(path: &Path) -> Scan {
        scan(path, &AtomicBool::new(false), |_, _| {}).unwrap()
    }

    fn structurally_valid(scan: &Scan) {
        let mut seen = vec![false; scan.nodes.len()];
        seen[0] = true;
        for (id, node) in scan.nodes.iter().enumerate() {
            for &child in &node.children {
                assert!(child > id && !seen[child]);
                seen[child] = true;
                assert_eq!(scan.nodes[child].parent, Some(id));
            }
            assert!(
                node.children
                    .windows(2)
                    .all(|p| scan.nodes[p[0]].bytes >= scan.nodes[p[1]].bytes)
            );
            assert!(
                node.bytes
                    >= node
                        .children
                        .iter()
                        .map(|&c| scan.nodes[c].bytes)
                        .sum::<u64>()
            );
        }
        assert!(seen.into_iter().all(|reachable| reachable));
    }

    #[test]
    fn refresh_grafts_new_measurements_preserving_other_branches_and_directory_blocks() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let branch = root.join("branch");
        let sibling = root.join("sibling");
        fs::create_dir(&branch).unwrap();
        fs::create_dir(&sibling).unwrap();
        fs::write(sibling.join("keep"), vec![42; 16384]).unwrap();
        let old = measure(root);
        let overhead = direct_bytes(&old, &old.nodes[0]);
        let sibling_id = entry(&old, &sibling).unwrap();
        fs::write(branch.join("new.bin"), vec![1; 98304]).unwrap();
        // Another branch is intentionally stale until separately refreshed.
        fs::write(sibling.join("not measured"), vec![1; 8192]).unwrap();
        let fresh = Arc::new(measure(&branch));
        let changes = Changes {
            refreshed: Some(fresh.clone()),
            removed: vec![],
        };
        assert!(changes.affects(root));
        assert!(!changes.affects(&sibling));
        let updated = apply(&old, &changes).unwrap();
        structurally_valid(&updated);
        assert_eq!(
            updated.nodes[0].bytes,
            overhead + old.nodes[sibling_id].bytes + fresh.nodes[0].bytes
        );
        assert_eq!(updated.nodes[updated.nodes[0].children[0]].name, "branch");
        assert!(entry(&updated, &sibling.join("not measured")).is_none());
        assert!(entry(&updated, &branch.join("new.bin")).is_some());
        assert_eq!(
            apply(&fresh, &changes).unwrap().nodes.len(),
            fresh.nodes.len()
        );
        let old_child = measure(&sibling);
        assert!(
            apply(
                &old_child,
                &Changes {
                    refreshed: Some(Arc::new(updated)),
                    removed: vec![]
                }
            )
            .is_none()
        );
    }

    #[test]
    fn removing_owners_and_duplicate_targets_transfers_blocks_to_surviving_aliases() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let folder = root.join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("nested"), vec![99; 65536]).unwrap();
        let a = root.join("first");
        let b = root.join("second");
        fs::write(&a, vec![1; 32768]).unwrap();
        fs::hard_link(&a, &b).unwrap();
        let old = measure(root);
        let ownership = old.nodes[entry(&old, &a).unwrap()].bytes;
        let (owner, alias) = if ownership > 0 { (a, b) } else { (b, a) };
        let alias_bytes = old.nodes[entry(&old, &owner).unwrap()].bytes;
        let gone = old.nodes[entry(&old, &folder).unwrap()].bytes;
        let changes = Changes {
            refreshed: None,
            removed: vec![
                owner.clone(),
                folder.clone(),
                folder.join("nested"),
                root.join("never existed"),
            ],
        };
        let updated = apply(&old, &changes).unwrap();
        structurally_valid(&updated);
        assert_eq!(updated.nodes.len(), 2);
        assert_eq!(updated.nodes[0].bytes, old.nodes[0].bytes - gone);
        assert_eq!(
            updated.nodes[entry(&updated, &alias).unwrap()].bytes,
            alias_bytes
        );
        assert!(entry(&updated, &owner).is_none());
        assert!(apply(&measure(&folder), &changes).is_none());
    }

    #[test]
    fn independently_refreshed_hardlinks_count_once_after_grafting() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let branch = root.join("subdir");
        fs::create_dir(&branch).unwrap();
        let a = root.join("a");
        fs::write(&a, vec![42; 32768]).unwrap();
        let old = measure(root);
        fs::hard_link(&a, branch.join("b")).unwrap();
        let update = apply(
            &old,
            &Changes {
                refreshed: Some(Arc::new(measure(&branch))),
                removed: vec![],
            },
        )
        .unwrap();
        structurally_valid(&update);
        let bytes = |p: &Path| update.nodes[entry(&update, p).unwrap()].bytes;
        assert_eq!(
            bytes(&a) + bytes(&branch.join("b")),
            old.nodes[entry(&old, &a).unwrap()].bytes
        );
    }

    #[test]
    fn missing_directory_can_be_grafted_only_beneath_a_recorded_parent() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let old = measure(root);
        let branch = root.join("new");
        let child = branch.join("child");
        fs::create_dir_all(&child).unwrap();
        let changes = |p: &Path| Changes {
            refreshed: Some(Arc::new(measure(p))),
            removed: vec![],
        };
        assert_eq!(apply(&old, &changes(&child)).unwrap().nodes.len(), 1);
        let added = apply(&old, &changes(&branch)).unwrap();
        assert!(entry(&added, &child).is_some());
        let missing = Changes {
            removed: vec![root.join("gone")],
            refreshed: None,
        };
        assert!(missing.affects(root));
        assert!(!missing.touches(&old));
    }
}
