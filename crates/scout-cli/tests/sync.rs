//! End-to-end hosted sync against local bare remotes (no network): `scout sync` cycles,
//! and a live `scout serve` picking up a push.

// Helpers outside #[test] fns are not covered by clippy's allow-unwrap-in-tests; a panic
// is exactly the failure report a test wants.
#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn scout_raw(index: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_scout")).arg("--index-dir").arg(index).args(args).output().unwrap()
}

fn scout(index: &Path, args: &[&str]) -> (String, String) {
    let out = scout_raw(index, args);
    assert!(
        out.status.code().is_some_and(|c| c <= 1),
        "scout {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A bare remote plus the scratch clone commits are pushed from.
struct Remote {
    url: String,
    work: PathBuf,
}

impl Remote {
    fn new(tmp: &Path, name: &str, files: &[(&str, &str)]) -> Self {
        let bare = tmp.join("remotes").join(format!("{name}.git"));
        let work = tmp.join("work").join(name);
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        git(&bare, &["init", "--quiet", "--bare", "--initial-branch=main"]);
        git(&work, &["init", "--quiet", "--initial-branch=main"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        // file:// is what makes a local clone honour --depth, the path the server uses.
        let url = format!("file:///{}", bare.to_str().unwrap().replace('\\', "/").trim_start_matches('/'));
        let r = Self { url, work };
        r.push(files, &[]);
        r
    }

    fn push(&self, write: &[(&str, &str)], delete: &[&str]) {
        for (path, content) in write {
            let p = self.work.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        for path in delete {
            std::fs::remove_file(self.work.join(path)).unwrap();
        }
        git(&self.work, &["add", "-A"]);
        git(
            &self.work,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "c",
            ],
        );
        git(&self.work, &["push", "--quiet", "origin", "main"]);
    }
}

fn write_config(tmp: &Path, repos: &[(&str, Option<&str>)]) -> PathBuf {
    write_config_with(tmp, "", repos)
}

/// `head` goes before the first table, where top-level keys must be.
fn write_config_with(tmp: &Path, head: &str, repos: &[(&str, Option<&str>)]) -> PathBuf {
    let mut toml = format!("mirror_dir = 'mirrors'\ninterval_secs = 1\n{head}");
    for (url, name) in repos {
        writeln!(toml, "[[repo]]\nurl = '{url}'").unwrap();
        if let Some(n) = name {
            writeln!(toml, "name = '{n}'").unwrap();
        }
    }
    let path = tmp.join("sync.toml");
    std::fs::write(&path, toml).unwrap();
    path
}

fn sync(index: &Path, cfg: &Path) -> String {
    let (_, err) = scout(index, &["sync", "--config", cfg.to_str().unwrap()]);
    err
}

fn search_off(index: &Path, pattern: &str) -> Vec<String> {
    let (out, _) = scout(index, &["search", "--freshness", "off", "-F", pattern]);
    let mut v: Vec<String> = out.lines().map(normalise).collect();
    v.sort();
    v
}

/// Output paths are absolute; keep `<repo dir>/<file>:<line>` so assertions read plainly.
fn normalise(line: &str) -> String {
    let line = line.replace('\\', "/");
    let marker = line.find("/mirrors/").map(|i| i + "/mirrors/".len()).or_else(|| line.find("/local/").map(|i| i + 1));
    line[marker.unwrap()..].to_string()
}

// One long scenario on purpose: each step starts from the state the previous ones built,
// which is what a server living through many cycles sees.
#[allow(clippy::too_many_lines)]
#[test]
fn sync_cycles_follow_the_remotes() {
    let tmp = tempfile::tempdir().unwrap();
    let index = tmp.path().join("index");
    let alpha = Remote::new(tmp.path(), "alpha", &[("src/a.rs", "fn needle_old() {}\n"), ("keep.txt", "stable\n")]);
    let beta = Remote::new(tmp.path(), "beta", &[("b.txt", "beta needle_old\n")]);

    // A repo added by path: sync must never touch it.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("l.txt"), "local needle_old\n").unwrap();
    scout(&index, &["add", local.to_str().unwrap()]);
    scout(&index, &["index"]);

    let cfg = write_config(tmp.path(), &[(&alpha.url, None), (&beta.url, Some("beta"))]);
    let err = sync(&index, &cfg);
    assert!(err.contains("2 reindexed, 0 removed, 0 failed, 3 repos"), "{err}");
    assert_eq!(
        search_off(&index, "needle_old"),
        ["beta/b.txt:beta needle_old", "local/l.txt:local needle_old", "remotes-alpha/src/a.rs:fn needle_old() {}",]
    );

    // Nothing moved: nothing is reindexed (change detection by commit).
    let err = sync(&index, &cfg);
    assert!(err.contains("0 reindexed, 0 removed, 0 failed"), "{err}");
    // A manual `scout index` of a synced repo keeps the commit it was built from, so the
    // next cycle still sees nothing to do.
    scout(&index, &["index", "remotes-alpha"]);
    let err = sync(&index, &cfg);
    assert!(err.contains("0 reindexed, 0 removed, 0 failed"), "{err}");

    // A push: edit, add, delete. Only alpha is reindexed, and off-mode search sees it.
    alpha.push(&[("src/a.rs", "fn needle_new() {}\n"), ("src/added.rs", "needle_new again\n")], &["keep.txt"]);
    let err = sync(&index, &cfg);
    assert!(err.contains("1 reindexed, 0 removed, 0 failed"), "{err}");
    assert_eq!(
        search_off(&index, "needle_new"),
        ["remotes-alpha/src/a.rs:fn needle_new() {}", "remotes-alpha/src/added.rs:needle_new again"]
    );
    assert_eq!(search_off(&index, "needle_old"), ["beta/b.txt:beta needle_old", "local/l.txt:local needle_old"]);
    assert!(search_off(&index, "stable").is_empty());
    // Strict search agrees: the catalog, shards and snapshots are consistent.
    let (strict, _) = scout(&index, &["search", "-F", "needle_new"]);
    assert_eq!(strict.lines().count(), 2);

    // The replaced shards wait out their grace period instead of vanishing under readers.
    let catalog: serde_json::Value =
        serde_json::from_slice(&std::fs::read(index.join("catalog.json")).unwrap()).unwrap();
    let retired = catalog["retired"].as_array().unwrap();
    assert_eq!(retired.len(), 2, "alpha's shards from the first sync and the manual index: {retired:?}");
    let retired_name = retired[0]["shard"].as_str().unwrap();
    assert!(index.join("shards").join(retired_name).exists());

    // A shard no catalog knows (a crashed run's leftover) gets retired, not left forever.
    std::fs::write(index.join("shards").join("orphan.1.0.native"), b"x").unwrap();

    // A failed owner listing (nothing listens on port 1) makes the cycle's view incomplete:
    // beta, missing from the config, may only be missing from the failed part, so it stays.
    let unlisted = write_config_with(
        tmp.path(),
        "github_api = 'http://127.0.0.1:1'\n[[github]]\nowner = 'acme'\nkind = 'org'\n",
        &[(&alpha.url, None)],
    );
    let out = scout_raw(&index, &["sync", "--config", unlisted.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("0 reindexed, 0 removed, 1 failed, 3 repos"), "{err}");
    assert!(err.contains("github acme"), "{err}");
    assert!(tmp.path().join("mirrors").join("beta").exists());

    // beta leaves the config: it leaves the catalog and its working copy is deleted; the
    // path-added repo stays.
    let cfg = write_config(tmp.path(), &[(&alpha.url, None)]);
    let err = sync(&index, &cfg);
    assert!(err.contains("0 reindexed, 1 removed, 0 failed, 2 repos"), "{err}");
    assert!(!tmp.path().join("mirrors").join("beta").exists());
    assert_eq!(search_off(&index, "needle_old"), ["local/l.txt:local needle_old"]);
    let catalog: serde_json::Value =
        serde_json::from_slice(&std::fs::read(index.join("catalog.json")).unwrap()).unwrap();
    let retired: Vec<&str> =
        catalog["retired"].as_array().unwrap().iter().map(|r| r["shard"].as_str().unwrap()).collect();
    assert!(retired.contains(&"orphan.1.0.native"), "{retired:?}");
    assert_eq!(retired.len(), 4, "alpha's two old shards, beta's shard and the orphan: {retired:?}");

    // A remote that cannot be cloned fails the cycle, never registers, and the rest syncs.
    let missing = format!("{}/nope.git", alpha.url.rsplit_once('/').unwrap().0);
    let cfg = write_config(tmp.path(), &[(&alpha.url, None), (&missing, None)]);
    let out = scout_raw(&index, &["sync", "--config", cfg.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("0 reindexed, 0 removed, 1 failed"), "{err}");
    let (list, _) = scout(&index, &["list"]);
    assert_eq!(list.lines().count(), 2, "{list}");

    // A repo name already used by a path-added repo is refused, not taken over.
    let cfg = write_config(tmp.path(), &[(&alpha.url, None), (&beta.url, Some("local"))]);
    let out = scout_raw(&index, &["sync", "--config", cfg.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("taken by a repo added by path"));

    // A different remote claiming a mirrored repo's name is refused, never reset onto it.
    let cfg = write_config(tmp.path(), &[(&beta.url, Some("remotes-alpha")), (&alpha.url, Some("alpha2"))]);
    let out = scout_raw(&index, &["sync", "--config", cfg.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("already mirrors"), "{err}");
    assert!(search_off(&index, "beta needle_old").is_empty());

    // Whatever a crashed or failed cycle left in the mirror directory is swept.
    let mirrors = tmp.path().join("mirrors");
    std::fs::create_dir_all(mirrors.join("stray").join(".git")).unwrap();
    std::fs::create_dir_all(mirrors.join("remotes-alpha.partial")).unwrap();
    std::fs::write(mirrors.join("loose.txt"), "x").unwrap();
    // A repo a user added by path inside the mirror directory is theirs, never swept.
    let theirs = mirrors.join("nested").join("mine");
    std::fs::create_dir_all(&theirs).unwrap();
    std::fs::write(theirs.join("m.txt"), "mine\n").unwrap();
    scout(&index, &["add", theirs.to_str().unwrap()]);
    scout(&index, &["index", "mine"]);
    let cfg = write_config(tmp.path(), &[(&alpha.url, None)]);
    let err = sync(&index, &cfg);
    assert!(err.contains("0 failed"), "{err}");
    let listing = |dir: &Path| {
        let mut v: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    };
    assert_eq!(listing(&mirrors), [".scout-mirrors", ".scout-sync.lock", "nested", "remotes-alpha"]);
    assert!(theirs.join("m.txt").exists());

    // A remote that is unreachable this cycle keeps its working copy and its index.
    let bare = tmp.path().join("remotes").join("alpha.git");
    let moved = tmp.path().join("remotes").join("alpha-away.git");
    std::fs::rename(&bare, &moved).unwrap();
    let out = scout_raw(&index, &["sync", "--config", cfg.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    std::fs::rename(&moved, &bare).unwrap();
    assert!(mirrors.join("remotes-alpha").join(".git").exists());
    assert!(!search_off(&index, "needle_new").is_empty());
}

#[test]
fn a_mirror_dir_with_other_content_is_never_adopted() {
    let tmp = tempfile::tempdir().unwrap();
    let index = tmp.path().join("index");
    let alpha = Remote::new(tmp.path(), "alpha", &[("a.txt", "x\n")]);
    std::fs::create_dir_all(tmp.path().join("mirrors")).unwrap();
    std::fs::write(tmp.path().join("mirrors").join("precious.txt"), "keep me").unwrap();
    let cfg = write_config(tmp.path(), &[(&alpha.url, None)]);
    let out = scout_raw(&index, &["sync", "--config", cfg.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a scout mirror directory"));
    assert!(tmp.path().join("mirrors").join("precious.txt").exists());
}

/// Drains the server's stderr on a thread (a full pipe would block it) and hands back
/// the address it reports. The child is owned by its kill guard from the moment it
/// exists, so a failed assertion never leaves a server running.
fn start_server(index: &Path, cfg: &Path, extra: &[&str]) -> (Killed, String, std::sync::mpsc::Receiver<String>) {
    let child = Command::new(env!("CARGO_BIN_EXE_scout"))
        .arg("--index-dir")
        .arg(index)
        .args(["serve", "--config", cfg.to_str().unwrap(), "--listen", "127.0.0.1:0"])
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut server = Killed(child);
    let stderr = server.0.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let line = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
        if let Some(addr) = line.strip_prefix("listening on ") {
            return (server, addr.to_string(), rx);
        }
    }
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn get(url: &str) -> (u16, String, Option<String>) {
    match ureq::get(url).call() {
        Ok(r) => {
            let generation = r.header("X-Scout-Generation").map(str::to_string);
            (r.status(), r.into_string().unwrap(), generation)
        }
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap(), None),
        Err(e) => panic!("GET {url}: {e}"),
    }
}

/// Polls `url` until `ok` accepts the body: the server syncs on its own schedule.
fn wait_for(url: &str, ok: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, body, _) = get(url);
        if status == 200 && ok(&body) {
            return body;
        }
        assert!(Instant::now() < deadline, "timed out waiting on {url}; last body: {body}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn serve_picks_up_pushes() {
    let tmp = tempfile::tempdir().unwrap();
    let index = tmp.path().join("index");
    let alpha = Remote::new(tmp.path(), "alpha", &[("a.txt", "first needle\n")]);
    let cfg = write_config(tmp.path(), &[(&alpha.url, Some("alpha"))]);

    let (_server, base, _logs) = start_server(&index, &cfg, &["--max-output-mb", "1"]);

    let body = wait_for(&format!("{base}/search?q=needle&F=1&n=1"), |b| b.contains("first needle"));
    assert!(body.replace('\\', "/").contains("/mirrors/alpha/a.txt:1:first needle"), "{body}");

    alpha.push(&[("a.txt", "second needle\n")], &[]);
    wait_for(&format!("{base}/search?q=needle&F=1"), |b| b.contains("second needle") && !b.contains("first"));

    let (status, body, generation) = get(&format!("{base}/search?q=second&repo=alpha&c=1"));
    assert_eq!(status, 200);
    assert!(body.trim_end().ends_with(":1"), "{body}");
    assert!(generation.unwrap().parse::<u64>().unwrap() >= 3, "initial, first sync, the push");

    let status: serde_json::Value = serde_json::from_str(&get(&format!("{base}/status")).1).unwrap();
    assert_eq!(status["repos"][0]["name"], "alpha");
    assert_eq!(status["repos"][0]["commit"].as_str().unwrap().len(), 40);

    assert_eq!(get(&format!("{base}/search?q=(")).0, 400, "bad pattern");
    assert_eq!(get(&format!("{base}/search?q=x&repo=nope")).0, 400, "unknown repo");
    assert_eq!(get(&format!("{base}/search?q=x&bogus=1")).0, 400, "unknown parameter");
    assert_eq!(get(&format!("{base}/search")).0, 400, "missing pattern");
    assert_eq!(get(&format!("{base}/nope")).0, 404);

    // A repo indexed from the CLI while the server runs is served too: the server
    // reconciles with the catalog on disk instead of trusting only its own cycles.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("l.txt"), "cli needle\n").unwrap();
    scout(&index, &["add", local.to_str().unwrap()]);
    scout(&index, &["index", "local"]);
    wait_for(&format!("{base}/search?q=cli+needle&F=1&repo=local"), |b| b.contains("cli needle"));

    // Ranked search: `top` bounds the files, the default prints every one of the two.
    let (status, body, _) = get(&format!("{base}/search?q=needle&F=1&l=1&rank=1&top=1"));
    assert_eq!((status, body.lines().count()), (200, 1), "{body}");
    let (status, body, _) = get(&format!("{base}/search?q=needle&F=1&l=1&rank=1"));
    assert_eq!((status, body.lines().count()), (200, 2), "{body}");
    assert_eq!(get(&format!("{base}/search?q=needle&top=1")).0, 400, "top without rank");
    assert_eq!(get(&format!("{base}/search?q=needle&rank=1&top=0")).0, 400, "top 0");

    let response = ureq::get(&format!("{base}/search?q=needle&limit=1&n=1")).call().unwrap();
    assert_eq!(response.header("X-Scout-Complete"), Some("false"));
    assert_eq!(response.header("X-Scout-Stop-Reason"), Some("match_limit"));
    assert_eq!(response.into_string().unwrap().lines().count(), 1);
    let (status, body, _) = get(&format!("{base}/search?q=needle&c=1&limit=1"));
    assert_eq!(status, 200);
    let count: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(count["count"], serde_json::json!({"kind":"lower_bound","value":1}));
    let (status, body, _) = get(&format!("{base}/search?q=needle&c=1&limit=100"));
    assert_eq!(status, 200);
    let count: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(count["count"], serde_json::json!({"kind":"exact","value":2}));
    assert_eq!(get(&format!("{base}/search?q=x&limit=1&rank=1")).0, 400);
    assert_eq!(get(&format!("{base}/search?q=x&max_bytes=100")).0, 400);

    // Output past --max-output-mb is refused, not buffered; narrowing the search works.
    let bulk = "bulk match line padding padding padding padding\n".repeat(30_000);
    alpha.push(&[("bulk.txt", &bulk)], &[]);
    wait_for(&format!("{base}/search?q=bulk&F=1&l=1"), |b| b.contains("bulk.txt"));
    let (status, body, _) = get(&format!("{base}/search?q=bulk&F=1"));
    assert_eq!(status, 422, "{}", &body[..body.len().min(200)]);
    assert!(body.contains("narrow the search"), "{body}");
    let (status, body, _) = get(&format!("{base}/search?q=bulk&F=1&rank=1"));
    assert_eq!(status, 422, "the ranked path enforces the cap too: {}", &body[..body.len().min(200)]);
}
