//! End-to-end: strict freshness sees changes made after indexing, in both change
//! detection modes, and each search hands its refreshed snapshot to the next one.

// Helpers outside #[test] fns are not covered by clippy's allow-unwrap-in-tests; a panic
// is exactly the failure report a test wants.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

fn scout(index: &Path, args: &[&str]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_scout")).arg("--index-dir").arg(index).args(args).output().unwrap();
    assert!(
        out.status.code().is_some_and(|c| c <= 1),
        "scout {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
}

fn stat(stderr: &str, key: &str) -> u64 {
    let field = stderr.split_whitespace().find_map(|w| w.strip_prefix(&format!("{key}="))).unwrap();
    field.parse().unwrap()
}

fn sorted_lines(s: &str) -> Vec<&str> {
    let mut v: Vec<&str> = s.lines().collect();
    v.sort_unstable();
    v
}

fn snapshot_dirs(index: &Path) -> Vec<String> {
    let pack = scout_core::snapshots::SnapshotPack::decode(
        &std::fs::read(index.join("shards").join("snapshots.pack")).unwrap(),
    )
    .unwrap();
    assert_eq!(pack.entries.len(), 1);
    let bytes = pack.entries.values().next().unwrap();
    let snap = scout_source_fs::snapshot::decode(bytes).unwrap().unwrap();
    snap.dirs.into_iter().map(|d| d.rel).collect()
}

/// `full_scans`: scan mode re-lists every directory by design, so it reports a full scan
/// on every search; journal mode must not. `idle_unchanged`: repos the journal proves
/// untouched since indexing skip the overlay (1 in journal mode, never in scan mode).
fn run(detection: &str, full_scans: u64, idle_unchanged: u64) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let index = tmp.path().join("index");
    std::fs::create_dir_all(repo.join("old")).unwrap();
    std::fs::write(repo.join("a.txt"), "nothing to see\n").unwrap();
    std::fs::write(repo.join("old").join("b.txt"), "needle one\n").unwrap();
    scout(&index, &["add", repo.to_str().unwrap()]);
    scout(&index, &["index"]);
    let search = |index: &Path| scout(index, &["search", "--stats", "--change-detection", detection, "needle"]);

    let (out, err) = search(&index);
    assert_eq!(sorted_lines(&out), vec![format!("{}:needle one", repo.join("old").join("b.txt").display())]);
    assert_eq!(stat(&err, "full_scans"), full_scans, "the snapshot written by indexing is used: {err}");
    assert_eq!(stat(&err, "unchanged_repos"), idle_unchanged, "{err}");

    // A same-size rewrite deep in the tree: only the file's mtime and content change.
    std::fs::write(
        repo.join("old").join("b.txt"),
        "needle uno
",
    )
    .unwrap();
    let (out, err) = search(&index);
    assert_eq!(sorted_lines(&out), vec![format!("{}:needle uno", repo.join("old").join("b.txt").display())], "{err}");
    assert_eq!(stat(&err, "unchanged_repos"), 0, "{err}");

    // Edit a file, add a new directory with a matching file, delete an old one.
    std::fs::write(repo.join("a.txt"), "now a needle lives here\n").unwrap();
    std::fs::create_dir(repo.join("new")).unwrap();
    std::fs::write(repo.join("new").join("c.txt"), "needle three\n").unwrap();
    std::fs::remove_file(repo.join("old").join("b.txt")).unwrap();
    let mut expected = vec![
        format!("{}:now a needle lives here", repo.join("a.txt").display()),
        format!("{}:needle three", repo.join("new").join("c.txt").display()),
    ];
    expected.sort_unstable();

    let (out, err) = search(&index);
    assert_eq!(sorted_lines(&out), expected, "{err}");
    assert_eq!(stat(&err, "full_scans"), full_scans, "{err}");
    assert_eq!(stat(&err, "unchanged_repos"), 0, "{err}");
    assert!(snapshot_dirs(&index).contains(&"new".to_string()), "refreshed snapshot was persisted");

    // The persisted snapshot describes the changed tree: the next search starts from it
    // and finds the same files.
    let (out, err) = search(&index);
    assert_eq!(sorted_lines(&out), expected, "{err}");
    assert_eq!(stat(&err, "full_scans"), full_scans, "{err}");
    // The persisted snapshot no longer describes the indexed listing: still no fast path.
    assert_eq!(stat(&err, "unchanged_repos"), 0, "{err}");

    // After reindexing, the index holds the current listing again.
    scout(&index, &["index"]);
    let (out, err) = search(&index);
    assert_eq!(sorted_lines(&out), expected, "{err}");
    assert_eq!(stat(&err, "unchanged_repos"), idle_unchanged, "{err}");

    // Removing the repo drops its snapshot from the pack.
    scout(&index, &["remove", "repo"]);
    let pack = scout_core::snapshots::SnapshotPack::decode(
        &std::fs::read(index.join("shards").join("snapshots.pack")).unwrap(),
    )
    .unwrap();
    assert!(pack.entries.is_empty());
}

#[test]
fn strict_freshness_with_journal() {
    run("journal", 0, 1);
}

#[test]
fn strict_freshness_with_scan() {
    run("scan", 1, 0);
}
