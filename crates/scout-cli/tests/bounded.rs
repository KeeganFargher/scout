//! Global budgets must stop real work and must never label a partial count exact.
#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Output};

fn run(index: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_scout")).arg("--index-dir").arg(index).args(args).output().unwrap()
}

fn ok(index: &Path, args: &[&str]) -> Output {
    let r = run(index, args);
    assert!(r.status.code().is_some_and(|c| c <= 1), "{args:?}: {}", String::from_utf8_lossy(&r.stderr));
    r
}

fn metadata(output: &Output) -> serde_json::Value {
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let line = stderr.lines().find_map(|l| l.strip_prefix("scout-bounded ")).unwrap();
    serde_json::from_str(line).unwrap()
}

fn exercise(backend: &[&str]) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    let index = tmp.path().join("index");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), b"needle needle\r\nhay\nneedle\n").unwrap();
    let mut big = String::new();
    for i in 0..9000 {
        writeln!(&mut big, "needle at line {i}").unwrap();
    }
    std::fs::write(root.join("b.txt"), big).unwrap();
    let utf16: Vec<u8> =
        [0xFF, 0xFE].into_iter().chain("needle utf16\n".encode_utf16().flat_map(u16::to_le_bytes)).collect();
    std::fs::write(root.join("c.txt"), utf16).unwrap();
    std::fs::write(root.join("d.txt"), b"invalid \xff needle\nunterminated needle").unwrap();
    let mut binary = b"needle before late binary\n".to_vec();
    binary.extend("padding line\n".repeat(10_000).as_bytes());
    binary.extend_from_slice(b"\0needle after nul\n");
    std::fs::write(root.join("e.bin"), binary).unwrap();
    ok(&index, &["add", root.to_str().unwrap()]);
    let mut args = vec!["index", "--threads", "1"];
    args.extend(backend);
    ok(&index, &args);
    let prefix = ["search", "--freshness", "off", "-n"];
    for pattern in ["needle", r"needle.*line", r"\Aneedle", r"needle\z"] {
        let full = ok(&index, &[&prefix[..], &[pattern]].concat());
        let batch = ok(
            &index,
            &[&prefix[..], &["--max-results", "100000", "--result-bytes", "1048576", "--budget-ms", "5000", pattern]]
                .concat(),
        );
        assert_eq!(full.stdout, batch.stdout, "unlimited-equivalent batch: {pattern}");
        assert_eq!(metadata(&batch)["complete"], true);
    }
    let full = ok(&index, &[&prefix[..], &["needle"]].concat());
    for n in [1, 2, 19, 4000, 9004] {
        let n = n.to_string();
        let batch = ok(
            &index,
            &[&prefix[..], &["--max-results", &n, "--result-bytes", "1048576", "--budget-ms", "5000", "needle"]]
                .concat(),
        );
        assert!(full.stdout.starts_with(&batch.stdout));
        let meta = metadata(&batch);
        assert_eq!(meta["returned_matches"], n.parse::<usize>().unwrap());
        assert_eq!(meta["complete"], false);
        assert_eq!(meta["stop_reason"], "match_limit");
    }
    let clipped = ok(&index, &[&prefix[..], &["--max-results", "100", "--result-bytes", "200", "needle"]].concat());
    assert!(clipped.stdout.len() <= 200 && full.stdout.starts_with(&clipped.stdout));
    assert_eq!(metadata(&clipped)["stop_reason"], "byte_limit");
    // A binary warning is a whole record too: a budget just below the complete
    // output must omit the warning rather than emit half of it.
    let warning_cap = (full.stdout.len() - 1).to_string();
    let warning = ok(
        &index,
        &[&prefix[..], &["--max-results", "100000", "--result-bytes", &warning_cap, "--budget-ms", "5000", "needle"]]
            .concat(),
    );
    assert!(full.stdout.starts_with(&warning.stdout) && warning.stdout.len() < full.stdout.len());
    assert_eq!(metadata(&warning)["stop_reason"], "byte_limit");
    for m in ["0", "1", "2"] {
        let full = ok(&index, &[&prefix[..], &["-m", m, "needle"]].concat());
        let batch = ok(
            &index,
            &[
                &prefix[..],
                &["-m", m, "--max-results", "100000", "--result-bytes", "1048576", "--budget-ms", "5000", "needle"],
            ]
            .concat(),
        );
        assert_eq!(full.stdout, batch.stdout, "per-file limit {m}");
    }
    let counted = ok(&index, &["search", "--freshness", "off", "-c", "needle"]);
    let expected: usize = String::from_utf8(counted.stdout)
        .unwrap()
        .lines()
        .map(|line| line.rsplit(':').next().unwrap().parse::<usize>().unwrap())
        .sum();
    let exact =
        ok(&index, &["search", "--freshness", "off", "-c", "--max-results", "100000", "--budget-ms", "5000", "needle"]);
    let exact: serde_json::Value = serde_json::from_slice(&exact.stdout).unwrap();
    assert_eq!(exact["complete"], true);
    assert_eq!(exact["count"]["kind"], "exact");
    assert_eq!(exact["count"]["value"], expected);
    let partial = ok(&index, &["search", "--freshness", "off", "-c", "--max-results", "100", "needle"]);
    let partial: serde_json::Value = serde_json::from_slice(&partial.stdout).unwrap();
    assert_eq!(partial["complete"], false);
    assert_eq!(partial["count"], serde_json::json!({"kind":"lower_bound","value":100}));
}

#[test]
fn native_raw_bounded_prefix_and_counts() {
    exercise(&["--content", "raw"]);
}
#[test]
fn native_lz4_bounded_prefix_and_counts() {
    exercise(&["--content", "lz4"]);
}
#[test]
fn sqlite_bounded_prefix_and_counts() {
    exercise(&["--backend", "sqlite"]);
}

#[test]
fn a_giant_line_is_not_buffered_for_a_small_preview_but_can_be_counted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    let index = tmp.path().join("index");
    std::fs::create_dir(&root).unwrap();
    let mut text = b"needle ".to_vec();
    text.extend(std::iter::repeat_n(b'x', 20_000_000));
    std::fs::write(root.join("huge.txt"), text).unwrap();
    ok(&index, &["add", root.to_str().unwrap()]);
    ok(&index, &["index", "--content", "lz4"]);
    let preview = ok(&index, &["search", "--freshness", "off", "--max-results", "100", "needle"]);
    assert!(preview.stdout.is_empty());
    assert_eq!(metadata(&preview)["stop_reason"], "byte_limit");
    let count =
        ok(&index, &["search", "--freshness", "off", "-c", "--max-results", "100", "--budget-ms", "5000", "needle"]);
    let count: serde_json::Value = serde_json::from_slice(&count.stdout).unwrap();
    assert_eq!(count["count"], serde_json::json!({"kind":"exact","value":1}));
}

#[test]
fn stopping_does_not_plan_a_corrupt_later_shard() {
    let tmp = tempfile::tempdir().unwrap();
    let index = tmp.path().join("index");
    for name in ["a", "b"] {
        let root = tmp.path().join(name);
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("hit.txt"), b"needle\n").unwrap();
        ok(&index, &["add", root.to_str().unwrap()]);
    }
    ok(&index, &["index"]);
    let catalog: serde_json::Value =
        serde_json::from_slice(&std::fs::read(index.join("catalog.json")).unwrap()).unwrap();
    let shard = catalog["repos"].as_array().unwrap().iter().find(|r| r["name"] == "b").unwrap()["index"]["shards"][0]
        .as_str()
        .unwrap();
    let path = index.join("shards").join(shard);
    let mut bytes = std::fs::read(&path).unwrap();
    let units = u64::from_le_bytes(bytes[64..72].try_into().unwrap()) as usize;
    bytes[units..units + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(path, bytes).unwrap();
    let batch = ok(&index, &["search", "--freshness", "off", "--max-results", "1", "needle"]);
    assert_eq!(metadata(&batch)["returned_matches"], 1);
    assert!(!run(&index, &["search", "--freshness", "off", "needle"]).status.success());
}
