//! End-to-end: large files are indexed as several units, and both backends must still
//! print rg-identical output while filtering on single units.

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

/// ~100 KiB of lines that contain none of the query's trigrams.
fn filler() -> String {
    "qqqq qqqq qqqq qqqq qqqq qqqq qqqq qqqq qqqq qqqq qqqq qqqq qqqq qqqq qqqq qqq\n".repeat(1300)
}

fn run(backend: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let index = tmp.path().join("index");
    std::fs::create_dir_all(&repo).unwrap();
    // The match sits in the second unit, so its line number must count the lines of the
    // first unit too.
    std::fs::write(repo.join("big.txt"), format!("alpha\n{}beta needle here\n", filler())).unwrap();
    // "needle" trigrams are split across units ("nee", "eed" | "edl", "dle"): a doc-level
    // index would make this a candidate, a unit-level one must not.
    std::fs::write(repo.join("spread.txt"), format!("need\n{}edle\n", filler())).unwrap();

    scout(&index, &["add", repo.to_str().unwrap()]);
    scout(&index, &["index", "--backend", backend]);
    let (out, err) = scout(&index, &["search", "-n", "--stats", "needle"]);
    let line = 2 + 1300;
    let expected = format!("{}{}big.txt:{line}:beta needle here\n", repo.display(), std::path::MAIN_SEPARATOR);
    assert_eq!(out, expected);
    assert_eq!(stat(&err, "candidates"), 1, "{err}");
}

#[test]
fn native_units_filter_and_keep_line_numbers() {
    run("native");
}

#[test]
fn sqlite_units_filter_and_keep_line_numbers() {
    run("sqlite");
}
