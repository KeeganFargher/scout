//! End-to-end `--rank`: the definition comes first, noise sinks, and the ranked files are
//! exactly the files the plain (rg-identical) search matches.

// Helpers outside #[test] fns are not covered by clippy's allow-unwrap-in-tests; a panic
// is exactly the failure report a test wants.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

fn scout(index: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_scout")).arg("--index-dir").arg(index).args(args).output().unwrap();
    assert!(
        out.status.code().is_some_and(|c| c <= 1),
        "scout {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// Repo-relative, `/`-separated, so assertions read the same on every OS.
fn rel(line: &str, root: &Path) -> String {
    line.strip_prefix(root.to_str().unwrap()).unwrap().trim_start_matches(['/', '\\']).replace('\\', "/")
}

#[test]
fn ranked_search_puts_the_definition_first_and_keeps_rgs_files() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let index = tmp.path().join("index");
    write(&repo, "src/widget.ts", "export function makeWidget(size: number) {\n  return { size };\n}\n");
    write(&repo, "src/app.ts", "import { makeWidget } from './widget';\nconst w = makeWidget(3);\n");
    write(&repo, "node_modules/kit/index.js", &"exports.makeWidget = makeWidget; // makeWidget\n".repeat(200));
    write(&repo, "package-lock.json", "{ \"makeWidget\": \"1.0.0\" }\n");
    write(&repo, "src/widget.test.ts", "test('makeWidget', () => makeWidget(1));\n");
    write(&repo, "docs/notes.md", "We call makeWidgets in a loop.\n");

    scout(&index, &["add", repo.to_str().unwrap()]);
    scout(&index, &["index"]);

    let ranked: Vec<String> = scout(&index, &["search", "--rank", "--top", "100", "-l", "makeWidget"])
        .lines()
        .map(|l| rel(l, &repo))
        .collect();
    assert_eq!(ranked[0], "src/widget.ts", "{ranked:?}");
    assert_eq!(ranked[1], "src/app.ts", "{ranked:?}");
    let last_two: BTreeSet<&str> = ranked[ranked.len() - 2..].iter().map(String::as_str).collect();
    assert_eq!(last_two, BTreeSet::from(["node_modules/kit/index.js", "package-lock.json"]), "{ranked:?}");

    let plain: BTreeSet<String> =
        scout(&index, &["search", "-l", "makeWidget"]).lines().map(|l| rel(l, &repo)).collect();
    assert_eq!(ranked.iter().cloned().collect::<BTreeSet<_>>(), plain, "ranking never changes which files match");
    // A set comparison alone would pass a file printed twice (say, by copy collapse).
    assert_eq!(ranked.len(), plain.len(), "every file exactly once: {ranked:?}");

    // --top bounds the files, and each file prints exactly as the plain search prints it.
    let top1 = scout(&index, &["search", "--rank", "--top", "1", "-n", "makeWidget"]);
    let plain_lines = scout(&index, &["search", "-n", "makeWidget"]);
    let widget: Vec<&str> = plain_lines.lines().filter(|l| rel(l, &repo).starts_with("src/widget.ts:")).collect();
    assert_eq!(top1.lines().collect::<Vec<_>>(), widget);
}
