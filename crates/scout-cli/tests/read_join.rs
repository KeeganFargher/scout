//! End-to-end: `--read-mbps` and `--read-iops` set the read join the opened repos search
//! with (E60), gp3's 32 KiB by default, and a volume with no reads is refused.

// Helpers outside #[test] fns are not covered by clippy's allow-unwrap-in-tests; a panic
// is exactly the failure report a test wants.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::{Command, Output};

fn scout(index: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_scout")).arg("--index-dir").arg(index).args(args).output().unwrap()
}

/// The `read_join` that `search --stats` reports for one indexed repo, under `flags`.
fn join(index: &Path, flags: &[&str]) -> u64 {
    let out = scout(index, &[&["search", "--stats", "-F", "zebrafish"], flags].concat());
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(0), "{err}");
    err.split_whitespace().find_map(|w| w.strip_prefix("read_join=")).unwrap().parse().unwrap()
}

#[test]
fn the_volumes_caps_set_the_read_join() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, index) = (tmp.path().join("r"), tmp.path().join("index"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("x.txt"), "the zebrafish swims\n").unwrap();
    assert!(scout(&index, &["add", repo.to_str().unwrap()]).status.success());
    assert!(scout(&index, &["index"]).status.success());
    // gp3: 41.7 KB a read; a t3.medium's EBS baseline: 21.5 KB. Global flags go either
    // side of the subcommand.
    assert_eq!(join(&index, &[]), 32 << 10);
    assert_eq!(join(&index, &["--read-mbps", "43", "--read-iops", "2000"]), 16 << 10);
    let out = Command::new(env!("CARGO_BIN_EXE_scout"))
        .args(["--read-mbps", "43", "--read-iops", "2000", "--index-dir"])
        .arg(&index)
        .args(["search", "--stats", "-F", "zebrafish"])
        .output()
        .unwrap();
    assert!(String::from_utf8(out.stderr).unwrap().contains("read_join=16384"));
    // A volume that serves no reads is refused before anything is opened.
    let out = scout(&index, &["search", "--read-iops", "0", "-F", "zebrafish"]);
    assert_eq!(out.status.code(), Some(2));
}
