//! End-to-end: a shard whose trigram filter rejects the query is skipped without reading
//! its dictionary and counted in `shards_filtered` (E24), its always-searched docs are
//! still verified, and the output stays what rg would print.

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

/// Two repos of one shard each; only the first holds the query's trigrams in its text. The
/// second also holds a UTF-16 file that matches: the indexer gives such a file no content
/// trigrams, only the always-search marker, so the filter rejects its shard and the file
/// must still be verified (and printed transcoded, as rg does). `filtered` is how many
/// shards the backend's filter must skip.
fn run(backend: &str, filtered: u64) {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, index) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("index"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("x.txt"), "the zebrafish swims\n").unwrap();
    std::fs::write(b.join("y.txt"), "nothing to see here\n").unwrap();
    let utf16: Vec<u8> =
        [0xFF, 0xFE].into_iter().chain("the zebrafish dances\n".encode_utf16().flat_map(u16::to_le_bytes)).collect();
    std::fs::write(b.join("z.txt"), utf16).unwrap();
    scout(&index, &["add", a.to_str().unwrap()]);
    scout(&index, &["add", b.to_str().unwrap()]);
    scout(&index, &["index", "--backend", backend]);
    let (out, err) = scout(&index, &["search", "-n", "--stats", "-F", "zebrafish"]);
    let sep = std::path::MAIN_SEPARATOR;
    assert_eq!(
        out,
        format!("{}{sep}x.txt:1:the zebrafish swims\n{}{sep}z.txt:1:the zebrafish dances\n", a.display(), b.display())
    );
    assert_eq!(stat(&err, "shards"), 2, "{err}");
    assert_eq!(stat(&err, "shards_filtered"), filtered, "{err}");
}

#[test]
fn a_native_shard_lacking_a_query_trigram_is_skipped() {
    run("native", 1);
}

#[test]
fn a_sqlite_shard_has_no_filter_to_skip_with() {
    run("sqlite", 0);
}
