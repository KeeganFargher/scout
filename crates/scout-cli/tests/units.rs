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

fn run(index_args: &[&str]) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let index = tmp.path().join("index");
    std::fs::create_dir_all(&repo).unwrap();
    // The match sits in the second unit, so its line number must count the lines of the
    // first unit too.
    std::fs::write(repo.join("big.txt"), format!("alpha\n{}beta nedl here\n", filler())).unwrap();
    // "nedl" has two trigrams, split across units ("ned" | "edl"): a doc-level index would
    // make this a candidate, a unit-level one must not. Both lists must be read for the
    // split to count; with every list this short the planner's decode budget (E22) always
    // reads them, and two trigrams leave no other pair it could pick instead.
    std::fs::write(repo.join("spread.txt"), format!("ned\n{}edl\n", filler())).unwrap();
    // Matches in its first and last units only, with units between them that hold none:
    // read by units (E32), the two are searched as separate runs, so the second must still
    // be numbered from the top of the file, and the file listed and counted once.
    std::fs::write(repo.join("twice.txt"), format!("alpha nedl\n{}{}omega nedl\n", filler(), filler())).unwrap();

    scout(&index, &["add", repo.to_str().unwrap()]);
    scout(&index, &[&["index"], index_args].concat());
    let path = |name: &str| format!("{}{}{name}", repo.display(), std::path::MAIN_SEPARATOR);
    let (big, twice) = (path("big.txt"), path("twice.txt"));
    let (out, err) = scout(&index, &["search", "-n", "--stats", "nedl"]);
    let last = 2 + 2 * 1300;
    assert_eq!(out, format!("{big}:1302:beta nedl here\n{twice}:1:alpha nedl\n{twice}:{last}:omega nedl\n"));
    assert_eq!(stat(&err, "candidates"), 2, "{err}");
    assert_eq!(scout(&index, &["search", "-l", "nedl"]).0, format!("{big}\n{twice}\n"));
    assert_eq!(scout(&index, &["search", "-c", "nedl"]).0, format!("{big}:1\n{twice}:2\n"));
    // `-m 1` stops at the file's first matching line, across all decoded blocks.
    let first = scout(&index, &["search", "-n", "-m", "1", "nedl"]).0;
    assert_eq!(first, format!("{big}:1302:beta nedl here\n{twice}:1:alpha nedl\n"));
}

#[test]
fn native_units_filter_and_keep_line_numbers() {
    run(&["--backend", "native"]);
}

/// The same through lz4 content (E29): both files span several compressible blocks, so a
/// candidate is decoded block by block before it is searched.
#[test]
fn native_lz4_units_filter_and_keep_line_numbers() {
    run(&["--backend", "native", "--content", "lz4"]);
}

#[test]
fn sqlite_units_filter_and_keep_line_numbers() {
    run(&["--backend", "sqlite"]);
}

fn block_semantics(index_args: &[&str]) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let index = tmp.path().join("index");
    std::fs::create_dir_all(&repo).unwrap();
    let content = format!("needle needle\n{}needle middle\n{}needle", filler(), filler());
    std::fs::write(repo.join("big.txt"), content).unwrap();
    scout(&index, &["add", repo.to_str().unwrap()]);
    scout(&index, &[&["index"], index_args].concat());
    let path = format!("{}{}big.txt", repo.display(), std::path::MAIN_SEPARATOR);
    let lines =
        [format!("{path}:1:needle needle\n"), format!("{path}:1302:needle middle\n"), format!("{path}:2603:needle\n")];
    // The short alternation branch makes the plan All. It must still decode lazily,
    // share a match budget across blocks, and emit one count/path per file.
    for pattern in ["needle", "needle|z"] {
        for limit in [0, 1, 2, 3, 9] {
            let count = limit.min(3);
            let n = limit.to_string();
            for threads in ["1", "2"] {
                let prefix = ["search", "--freshness", "off", "--threads", threads, "-m", &n];
                let actual = scout(&index, &[&prefix[..], &["-n", pattern]].concat()).0;
                assert_eq!(actual, lines[..count].concat(), "{pattern}, -m {limit}, threads={threads}");
                let actual = scout(&index, &[&prefix[..], &["-c", pattern]].concat()).0;
                let expected = if count == 0 { String::new() } else { format!("{path}:{count}\n") };
                assert_eq!(actual, expected, "{pattern}, -c -m {limit}");
                let actual = scout(&index, &[&prefix[..], &["-l", pattern]].concat()).0;
                let expected = if count == 0 { String::new() } else { format!("{path}\n") };
                assert_eq!(actual, expected, "{pattern}, -l -m {limit}");
            }
        }
        assert_eq!(scout(&index, &["search", "-n", pattern]).0, lines.concat());
        assert_eq!(scout(&index, &["search", "-c", pattern]).0, format!("{path}:3\n"));
    }
    // Keep rg's line-oriented anchor behavior: these assertions are evaluated against
    // individual lines, including the unterminated final line.
    for pattern in [r"\Aneedle", r"(?-m:^needle)"] {
        assert_eq!(scout(&index, &["search", "-n", pattern]).0, lines.concat());
    }
    for pattern in [r"needle\z", r"(?-m:needle$)"] {
        assert_eq!(scout(&index, &["search", "-n", pattern]).0, format!("{}{}", lines[0], lines[2]));
    }
}

#[test]
fn raw_blocks_keep_file_wide_printer_semantics() {
    block_semantics(&["--backend", "native", "--content", "raw"]);
}

#[test]
fn lz4_blocks_keep_file_wide_printer_semantics() {
    block_semantics(&["--backend", "native", "--content", "lz4"]);
}

#[test]
fn sqlite_keeps_file_wide_printer_semantics() {
    block_semantics(&["--backend", "sqlite"]);
}

fn shape_semantics(index_args: &[&str]) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let index = tmp.path().join("index");
    std::fs::create_dir_all(&repo).unwrap();
    let dates = ["2026-10-27", "٢٠٢٦-١٠-٢٧", "２０２６-１０-２７"];
    for (i, date) in dates.iter().enumerate() {
        std::fs::write(repo.join(format!("{i}.txt")), format!("{}date {date}\n", filler())).unwrap();
    }
    std::fs::write(repo.join("negative.txt"), "Copyright 2012-2026\nordinary code\n").unwrap();
    let utf16: Vec<u8> =
        [0xFF, 0xFE].into_iter().chain("date 2026-10-27\n".encode_utf16().flat_map(u16::to_le_bytes)).collect();
    std::fs::write(repo.join("utf16.txt"), utf16).unwrap();
    scout(&index, &["add", repo.to_str().unwrap()]);
    scout(&index, &[&["index"], index_args].concat());
    let path = |name: &str| format!("{}{}{name}", repo.display(), std::path::MAIN_SEPARATOR);
    let expected: String = dates
        .iter()
        .enumerate()
        .map(|(i, date)| format!("{}:1301:date {date}\n", path(&format!("{i}.txt"))))
        .chain(std::iter::once(format!("{}:1:date 2026-10-27\n", path("utf16.txt"))))
        .collect();
    let pattern = r"\d{4}-\d{2}-\d{2}";
    let (actual, stats) = scout(&index, &["search", "--freshness", "off", "--stats", "-n", pattern]);
    assert_eq!(actual, expected);
    assert_eq!(stat(&stats, "candidates"), 4, "the negative file must be rejected before verification");
    let mut files = String::new();
    for file in (0..3).map(|i| path(&format!("{i}.txt"))).chain(std::iter::once(path("utf16.txt"))) {
        files.push_str(&file);
        files.push('\n');
    }
    assert_eq!(scout(&index, &["search", "-l", pattern]).0, files);
    let mut counts = String::new();
    for file in files.lines() {
        counts.push_str(file);
        counts.push_str(":1\n");
    }
    assert_eq!(scout(&index, &["search", "-c", pattern]).0, counts);
}

#[test]
fn raw_shape_filter_keeps_unicode_and_reader_matches() {
    shape_semantics(&["--content", "raw"]);
}

#[test]
fn lz4_shape_filter_keeps_unicode_and_reader_matches() {
    shape_semantics(&["--content", "lz4"]);
}

#[test]
fn sqlite_shape_filter_keeps_unicode_and_reader_matches() {
    shape_semantics(&["--backend", "sqlite"]);
}
