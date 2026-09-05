//! Turning a raw prolly diff into findings. Pure — no database, no
//! HTML, no clock. Every behaviour worth pinning is asserted against
//! [`analyze`] in the tests at the bottom of this file.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{
    Diff, DiffResult, DupGroup, DupInfo, Entry, Inputs, Node, Side, SideResult, Status, Summary,
};

pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn depth(path: &str) -> usize {
    if path.is_empty() {
        0
    } else {
        path.matches('/').count() + 1
    }
}

// ---------------------------------------------------------------------
// correspondences
// ---------------------------------------------------------------------

/// A correspondence between one path on the left and one on the right.
///
/// Covers both "same bytes elsewhere" relations the viewer reports — a
/// move (gone from the left, present on the right) and a copy (present
/// on both) — and, with both sides pointing into the same tree, an
/// in-tree duplicate. One shape, so [`roll_up`] serves all three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub src: String,
    pub dst: String,
    pub kind: String,
    pub rolled_up: u32,
    /// True when an ancestor directory made the same journey, so this
    /// correspondence is implied rather than independently interesting.
    pub covered: bool,
}

impl Link {
    pub fn new(src: &str, dst: &str, kind: &str) -> Self {
        Link {
            src: src.to_string(),
            dst: dst.to_string(),
            kind: kind.to_string(),
            rolled_up: 0,
            covered: false,
        }
    }
}

/// The outermost accepted directory link that already implies `i`.
///
/// Anchored on `dst`: the candidate has to sit under the parent's
/// destination *and* its source has to be the parent's source plus the
/// identical relative suffix. Both halves matter — without the second,
/// a file that moved somewhere unrelated would be absorbed by whatever
/// directory happened to move above it.
fn covering(links: &[Link], accepted: &[usize], i: usize) -> Option<usize> {
    for &p in accepted {
        if p == i || links[p].dst == links[i].dst {
            continue;
        }
        let parent_dst = &links[p].dst;
        if !links[i].dst.starts_with(&format!("{parent_dst}/")) {
            continue;
        }
        let suffix = &links[i].dst[parent_dst.len()..];
        if links[i].src == format!("{}{}", links[p].src, suffix) {
            return Some(p);
        }
    }
    None
}

/// Collapse a related subtree into the single outermost directory.
///
/// Moving `docs/` to `archive/` moves every descendant with it, and
/// copying a directory copies every descendant with it; either way each
/// descendant arrives as its own link, and reporting all of them buries
/// the one fact worth reading. A link is marked `covered` when an
/// ancestor directory made exactly the same journey, and the surviving
/// ancestor carries a count of what it absorbed.
pub fn roll_up(links: &mut [Link]) {
    let mut dir_idx: Vec<usize> = (0..links.len())
        .filter(|&i| links[i].kind == "dir")
        .collect();
    dir_idx.sort_by_key(|&i| depth(&links[i].dst));

    let mut accepted: Vec<usize> = Vec::new();
    for &i in &dir_idx {
        if covering(links, &accepted, i).is_none() {
            accepted.push(i);
        }
    }

    // Decide first, mutate after: `covering` reads the whole slice, so
    // marking a link covered while still scanning would change what the
    // remaining links are compared against.
    let mut bumps: Vec<usize> = Vec::new();
    let mut covered = vec![false; links.len()];
    for (i, flag) in covered.iter_mut().enumerate() {
        if let Some(parent) = covering(links, &accepted, i) {
            bumps.push(parent);
            *flag = true;
        }
    }
    for (link, flag) in links.iter_mut().zip(covered) {
        link.covered = flag;
    }
    for parent in bumps {
        links[parent].rolled_up += 1;
    }
}

// ---------------------------------------------------------------------
// move pairing
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Move {
    pub src: Entry,
    pub dst: Entry,
}

/// Match removed rows against added rows carrying the same digest.
///
/// A pair is a move: the same bytes at a different path. Pairing is
/// greedy and prefers a candidate that kept its basename, which is what
/// makes `docs/reports` pair with `archive/reports` rather than with
/// some unrelated directory holding identical content.
pub fn pair_moves(diff: &Diff) -> (Vec<Move>, Vec<Entry>, Vec<Entry>) {
    let key = |e: &Entry| (e.kind.clone(), e.digest.clone());

    let mut added_by: BTreeMap<(String, String), Vec<Entry>> = BTreeMap::new();
    for entry in diff.added.iter().filter(|e| !e.path.is_empty()) {
        added_by.entry(key(entry)).or_default().push(entry.clone());
    }

    let mut moves = Vec::new();
    let mut used_removed: BTreeSet<String> = BTreeSet::new();
    let mut used_added: BTreeSet<String> = BTreeSet::new();

    for src in diff.removed.iter().filter(|e| !e.path.is_empty()) {
        let Some(pool) = added_by.get_mut(&key(src)) else {
            continue;
        };
        if pool.is_empty() {
            continue;
        }
        pool.sort_by_key(|dst| {
            (
                basename(&dst.path) != basename(&src.path),
                (depth(&dst.path) as i64 - depth(&src.path) as i64).abs(),
                dst.path.clone(),
            )
        });
        let dst = pool.remove(0);
        used_removed.insert(src.path.clone());
        used_added.insert(dst.path.clone());
        moves.push(Move {
            src: src.clone(),
            dst,
        });
    }

    let residual_removed = diff
        .removed
        .iter()
        .filter(|e| !e.path.is_empty() && !used_removed.contains(&e.path))
        .cloned()
        .collect();
    let residual_added = diff
        .added
        .iter()
        .filter(|e| !e.path.is_empty() && !used_added.contains(&e.path))
        .cloned()
        .collect();
    (moves, residual_removed, residual_added)
}

// ---------------------------------------------------------------------
// in-tree duplicates
// ---------------------------------------------------------------------

/// Group one tree's entries by digest, keeping only the repeats.
///
/// Answers a question the left/right diff cannot: is this tree storing
/// the same bytes more than once? Directories count, because a
/// directory's digest covers its whole subtree — so a folder copied to
/// a second place inside the same tree is one finding, not one per file
/// in it.
pub fn group_duplicates(entries: &[Entry]) -> Vec<DupGroup> {
    let mut buckets: BTreeMap<(String, String), Vec<Entry>> = BTreeMap::new();
    for entry in entries.iter().filter(|e| !e.path.is_empty()) {
        buckets
            .entry((entry.kind.clone(), entry.digest.clone()))
            .or_default()
            .push(entry.clone());
    }

    let mut groups: Vec<DupGroup> = Vec::new();
    for ((kind, digest), mut members) in buckets {
        if members.len() < 2 {
            continue;
        }
        members.sort_by_key(|e| (depth(&e.path), e.path.clone()));
        groups.push(DupGroup {
            digest,
            kind,
            size: members[0].size,
            paths: members.into_iter().map(|e| e.path).collect(),
            rolled_up: 0,
        });
    }
    roll_up_duplicates(groups)
}

/// Drop duplicate groups that a duplicated parent directory implies.
fn roll_up_duplicates(mut groups: Vec<DupGroup>) -> Vec<DupGroup> {
    let mut links: Vec<Link> = Vec::new();
    let mut owner: Vec<usize> = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        let canonical = &group.paths[0];
        for other in &group.paths[1..] {
            links.push(Link::new(canonical, other, &group.kind));
            owner.push(gi);
        }
    }
    roll_up(&mut links);

    let mut survives = vec![false; groups.len()];
    let mut rolled = vec![0u32; groups.len()];
    for (link, &gi) in links.iter().zip(owner.iter()) {
        if link.covered {
            continue;
        }
        survives[gi] = true;
        rolled[gi] += link.rolled_up;
    }

    let mut out: Vec<DupGroup> = Vec::new();
    for (gi, group) in groups.drain(..).enumerate() {
        if survives[gi] {
            let mut group = group;
            group.rolled_up = rolled[gi];
            out.push(group);
        }
    }
    // Biggest win first — the reader wants the reclaimable bytes.
    out.sort_by_key(|g| (-g.wasted(), g.paths[0].clone()));
    out
}

// ---------------------------------------------------------------------
// classification
// ---------------------------------------------------------------------

/// One reportable row on one side.
#[derive(Debug, Clone)]
pub struct Finding {
    pub entry: Entry,
    pub status: Status,
    pub peer: Option<String>,
    pub note: String,
    pub rolled_up: u32,
}

impl Finding {
    fn new(
        entry: Entry,
        status: Status,
        peer: Option<String>,
        note: String,
        rolled_up: u32,
    ) -> Self {
        Finding {
            entry,
            status,
            peer,
            note,
            rolled_up,
        }
    }
}

/// Turn a raw diff into the two sides' findings.
///
/// `copies_right` maps a digest that vanished from the left to a path
/// where those bytes still live on the right; `copies_left` is the
/// mirror. Both are empty when copy detection is off, which downgrades
/// "gone but a copy remains" to a plain delete and "copy" to a plain
/// add — never the other way round.
#[allow(clippy::type_complexity)]
pub fn classify(
    diff: &Diff,
    copies_right: &BTreeMap<String, String>,
    copies_left: &BTreeMap<String, String>,
) -> (Vec<Finding>, Vec<Finding>, Summary) {
    let (moves, residual_removed, residual_added) = pair_moves(diff);

    let mut move_links: Vec<Link> = moves
        .iter()
        .map(|m| Link::new(&m.src.path, &m.dst.path, &m.src.kind))
        .collect();
    roll_up(&mut move_links);

    let kept: Vec<&Entry> = residual_removed
        .iter()
        .filter(|e| copies_right.contains_key(&e.digest))
        .collect();
    let mut kept_links: Vec<Link> = kept
        .iter()
        .map(|e| Link::new(&e.path, &copies_right[&e.digest], &e.kind))
        .collect();
    roll_up(&mut kept_links);

    let copied: Vec<&Entry> = residual_added
        .iter()
        .filter(|e| copies_left.contains_key(&e.digest))
        .collect();
    let mut copied_links: Vec<Link> = copied
        .iter()
        .map(|e| Link::new(&copies_left[&e.digest], &e.path, &e.kind))
        .collect();
    roll_up(&mut copied_links);

    let gone: Vec<&Entry> = residual_removed
        .iter()
        .filter(|e| !copies_right.contains_key(&e.digest))
        .collect();
    let fresh: Vec<&Entry> = residual_added
        .iter()
        .filter(|e| !copies_left.contains_key(&e.digest))
        .collect();

    let mut left: Vec<Finding> = Vec::new();
    let mut right: Vec<Finding> = Vec::new();

    for (mv, link) in moves.iter().zip(move_links.iter()) {
        if link.covered {
            continue;
        }
        left.push(Finding::new(
            mv.src.clone(),
            Status::MovedOut,
            Some(mv.dst.path.clone()),
            format!("moved to {}", mv.dst.path),
            link.rolled_up,
        ));
        right.push(Finding::new(
            mv.dst.clone(),
            Status::MovedIn,
            Some(mv.src.path.clone()),
            format!("moved from {}", mv.src.path),
            link.rolled_up,
        ));
    }

    for (entry, link) in kept.iter().zip(kept_links.iter()) {
        if link.covered {
            continue;
        }
        left.push(Finding::new(
            (*entry).clone(),
            Status::RemovedButCopyRemains,
            Some(link.dst.clone()),
            format!("gone from here, identical bytes still at {}", link.dst),
            link.rolled_up,
        ));
    }

    for (entry, link) in copied.iter().zip(copied_links.iter()) {
        if link.covered {
            continue;
        }
        right.push(Finding::new(
            (*entry).clone(),
            Status::AddedFromCopy,
            Some(link.src.clone()),
            format!(
                "new here, but identical bytes already existed at {}",
                link.src
            ),
            link.rolled_up,
        ));
    }

    for entry in &gone {
        let note = if entry.is_dir() {
            "directory gone — no directory on the right holds this exact subtree"
        } else {
            "deleted — these bytes are nowhere on the right"
        };
        left.push(Finding::new(
            (*entry).clone(),
            Status::Removed,
            None,
            note.to_string(),
            0,
        ));
    }

    for entry in &fresh {
        let note = if entry.is_dir() {
            "new directory — no directory on the left held this exact subtree"
        } else {
            "new content — these bytes are nowhere on the left"
        };
        right.push(Finding::new(
            (*entry).clone(),
            Status::Added,
            None,
            note.to_string(),
            0,
        ));
    }

    for (src, dst) in &diff.modified {
        if src.path.is_empty() {
            continue;
        }
        if src.is_dir() {
            // A directory's digest covers its children, so "modified"
            // here only ever means "something below me changed". That
            // is the tree doing its job, not a finding.
            left.push(Finding::new(
                src.clone(),
                Status::Structure,
                Some(src.path.clone()),
                String::new(),
                0,
            ));
            right.push(Finding::new(
                dst.clone(),
                Status::Structure,
                Some(dst.path.clone()),
                String::new(),
                0,
            ));
        } else {
            left.push(Finding::new(
                src.clone(),
                Status::Modified,
                Some(dst.path.clone()),
                "content changed".to_string(),
                0,
            ));
            right.push(Finding::new(
                dst.clone(),
                Status::Modified,
                Some(src.path.clone()),
                "content changed".to_string(),
                0,
            ));
        }
    }

    let rolled_up: u32 = move_links
        .iter()
        .chain(kept_links.iter())
        .chain(copied_links.iter())
        .map(|l| l.rolled_up)
        .sum();

    let summary = Summary {
        moves: move_links.iter().filter(|l| !l.covered).count(),
        moved_entries: moves.len(),
        rolled_up,
        removed: gone.len(),
        removed_but_copy_remains: kept_links.iter().filter(|l| !l.covered).count(),
        added: fresh.len(),
        added_from_copy: copied_links.iter().filter(|l| !l.covered).count(),
        modified: diff
            .modified
            .iter()
            .filter(|(s, _)| !s.is_dir() && !s.path.is_empty())
            .count(),
        ..Summary::default()
    };
    (left, right, summary)
}

// ---------------------------------------------------------------------
// tree assembly
// ---------------------------------------------------------------------

fn with_ancestors(paths: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out = paths.clone();
    for path in paths {
        let parts: Vec<&str> = path.split('/').collect();
        for i in 1..parts.len() {
            out.insert(parts[..i].join("/"));
        }
    }
    out
}

/// Per-path duplicate detail, for every member of every group.
fn dup_annotations(groups: &[DupGroup]) -> BTreeMap<String, DupInfo> {
    let mut out = BTreeMap::new();
    for group in groups {
        for path in &group.paths {
            out.insert(
                path.clone(),
                DupInfo {
                    n: group.paths.len(),
                    peers: group.paths.iter().filter(|p| *p != path).cloned().collect(),
                    waste: group.wasted(),
                    kind: group.kind.clone(),
                    size: group.size,
                    roll: group.rolled_up,
                },
            );
        }
    }
    out
}

fn build_nodes(
    findings: &[Finding],
    full: Option<&Vec<Entry>>,
    dups: &BTreeMap<String, DupInfo>,
) -> Vec<Node> {
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();

    for finding in findings {
        if finding.entry.path.is_empty() {
            continue;
        }
        nodes.insert(
            finding.entry.path.clone(),
            Node {
                path: finding.entry.path.clone(),
                kind: finding.entry.kind.clone(),
                size: finding.entry.size,
                status: finding.status,
                peer: finding.peer.clone(),
                note: finding.note.clone(),
                rolled_up: finding.rolled_up,
                dup: None,
                diff_entries: 0,
                diff_bytes: 0,
            },
        );
    }

    if let Some(entries) = full {
        for entry in entries {
            if entry.path.is_empty() || nodes.contains_key(&entry.path) {
                continue;
            }
            nodes.insert(
                entry.path.clone(),
                Node::plain(&entry.path, &entry.kind, entry.size, Status::Unchanged),
            );
        }
    }

    for (path, info) in dups {
        let node = nodes.entry(path.clone()).or_insert_with(|| {
            // A duplicate the diff never mentioned — unchanged between
            // the two trees, but repeated inside this one. It still has
            // to appear, or the finding has nowhere to land.
            Node::plain(path, &info.kind, info.size, Status::Unchanged)
        });
        node.dup = Some(info.clone());
    }

    let present: BTreeSet<String> = nodes.keys().cloned().collect();
    for path in with_ancestors(&present) {
        if !path.is_empty() {
            nodes
                .entry(path.clone())
                .or_insert_with(|| Node::plain(&path, "dir", 0, Status::Structure));
        }
    }

    let mut out: Vec<Node> = nodes.into_values().collect();
    attach_diff_weights(&mut out);
    out
}

/// Roll each subtree's diff weight up to the directories above it, so a
/// reader can sort by "where is the action" rather than by name.
///
/// Two numbers, because they answer different questions and only one of
/// them can be summed naively:
///
/// - **entries** counts every finding in the subtree, plus what its
///   rollups absorbed. Directories included: a new directory is itself
///   a thing that changed, and counting it double-counts nothing.
/// - **bytes** counts only **maximal** findings — a finding with no
///   other finding beneath it. A directory's `size` is the recursive
///   sum of its contents, so a brand-new tree would otherwise have the
///   directory's bytes counted again for every file inside it. Taking
///   only the outermost fixes that, and still gives a rolled-up move
///   (whose interior is absent from the node set) its full weight.
///
/// `nodes` must be sorted by path, which [`build_nodes`] guarantees.
fn attach_diff_weights(nodes: &mut [Node]) {
    let findings: BTreeSet<String> = nodes
        .iter()
        .filter(|n| n.status.is_finding())
        .map(|n| n.path.clone())
        .collect();
    if findings.is_empty() {
        return;
    }

    // A finding is maximal unless one of its ancestors-of-a-finding
    // chains back to it — i.e. unless some finding sits below it.
    let mut has_finding_below: BTreeSet<&str> = BTreeSet::new();
    for path in &findings {
        let mut cur = path.as_str();
        while let Some(i) = cur.rfind('/') {
            cur = &cur[..i];
            has_finding_below.insert(cur);
        }
        // The root ("") is an ancestor of every top-level entry, but it
        // is never itself a node, so it needs no marking.
    }

    // (entries, bytes) contributed at each path.
    let mut own: Vec<(u32, i64)> = Vec::with_capacity(nodes.len());
    for node in nodes.iter() {
        if !node.status.is_finding() {
            own.push((0, 0));
            continue;
        }
        let entries = 1 + node.rolled_up;
        let bytes = if has_finding_below.contains(node.path.as_str()) {
            0
        } else {
            node.size
        };
        own.push((entries, bytes));
    }

    // Index by path so ancestors can be credited without a second scan.
    let index: BTreeMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.path.as_str(), i))
        .collect();
    let mut totals: Vec<(u32, i64)> = vec![(0, 0); nodes.len()];
    for (i, node) in nodes.iter().enumerate() {
        let (entries, bytes) = own[i];
        if entries == 0 && bytes == 0 {
            continue;
        }
        totals[i].0 += entries;
        totals[i].1 += bytes;
        let mut cur = node.path.as_str();
        while let Some(cut) = cur.rfind('/') {
            cur = &cur[..cut];
            if let Some(&a) = index.get(cur) {
                totals[a].0 += entries;
                totals[a].1 += bytes;
            }
        }
    }

    for (node, (entries, bytes)) in nodes.iter_mut().zip(totals) {
        node.diff_entries = entries;
        node.diff_bytes = bytes;
    }
}

// ---------------------------------------------------------------------
// the seam
// ---------------------------------------------------------------------

/// Turn everything that was read into everything that is concluded.
///
/// Pure: the same [`Inputs`] always produce the same [`DiffResult`], so
/// every behaviour worth testing can be tested by building an `Inputs`
/// literal.
pub fn analyze(inputs: &Inputs) -> DiffResult {
    let (left_findings, right_findings, counts) =
        classify(&inputs.diff, &inputs.copies_right, &inputs.copies_left);

    let left_dups = group_duplicates(&inputs.left.dup_candidates);
    let right_dups = group_duplicates(&inputs.right.dup_candidates);

    let left_nodes = build_nodes(
        &left_findings,
        inputs.left.full.as_ref(),
        &dup_annotations(&left_dups),
    );
    let right_nodes = build_nodes(
        &right_findings,
        inputs.right.full.as_ref(),
        &dup_annotations(&right_dups),
    );

    let summary = Summary {
        full_tree: inputs.left.full.is_some(),
        copy_detection: inputs.copy_detection,
        dup_threshold: inputs.dup_threshold,
        unified: inputs.unified,
        ..counts
    };

    DiffResult {
        left: SideResult {
            db: inputs.left.db.clone(),
            reference: inputs.left.reference.clone(),
            commit: inputs.left.commit.clone(),
            dup_wasted: left_dups.iter().map(|g| g.wasted()).sum(),
            dup_groups: left_dups,
            nodes: left_nodes,
        },
        right: SideResult {
            db: inputs.right.db.clone(),
            reference: inputs.right.reference.clone(),
            commit: inputs.right.commit.clone(),
            dup_wasted: right_dups.iter().map(|g| g.wasted()).sum(),
            dup_groups: right_dups,
            nodes: right_nodes,
        },
        summary,
    }
}

/// Convenience for callers that only want one side's view.
pub fn side_of(result: &DiffResult, side: Side) -> &SideResult {
    result.side(side)
}
