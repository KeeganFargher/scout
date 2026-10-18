//! Working copies kept in step with their remotes, by running the real `git`.
//!
//! Shelling out (rather than gix or libgit2) gets every protocol, proxy and credential
//! setup users' remotes already work with, the fastest fetch there is, and no C build
//! dependency; the price is needing `git` ≥ 2.31 at runtime, checked by [`Git::new`].

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use base64::Engine;
use scout_core::catalog::Remote;

use crate::Token;

/// `GIT_CONFIG_COUNT` (how the token reaches git without a command line) needs 2.31.
const MIN_VERSION: (u32, u32) = (2, 31);

/// Variables that would point git at another repository. A server started from inside a
/// checkout (or a git hook) inherits them, and `reset --hard` would then hit that tree.
const INHERITED_GIT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CURL_VERBOSE",
];

/// Prefix of git's tracing variables. `GIT_TRACE_CURL` with `GIT_TRACE_REDACT=0` writes
/// the Authorization header to stderr, and stderr ends up in failure reports and `/status`.
const TRACE_ENV_PREFIX: &str = "GIT_TRACE";

pub struct Git {
    token: Option<Token>,
    /// `<scheme>://<host>/`: the only URL prefix the token is ever sent to.
    auth_prefix: String,
    /// Hard deadline per repo sync, across its git commands; a hung connection must not
    /// stall the sync loop.
    timeout: Duration,
}

/// Where a working copy stands after [`Git::sync`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Synced {
    /// The commit now checked out.
    pub commit: String,
    /// The checkout changed (new clone, or the remote moved).
    pub moved: bool,
}

impl Git {
    /// `auth_prefix` comes from [`crate::auth_prefix`]; `timeout` bounds every git run.
    pub fn new(token: Option<Token>, auth_prefix: String, timeout: Duration) -> anyhow::Result<Self> {
        if token.is_some() && std::env::var_os("GIT_CONFIG_COUNT").is_some() {
            // The token travels as GIT_CONFIG_COUNT config. Replacing the operator's own
            // entries would silently change how every remote is fetched.
            bail!("GIT_CONFIG_COUNT is already set in the environment; scout sync needs it to pass the token");
        }
        let git = Self { token, auth_prefix, timeout };
        let out = git.run(None, &["--version"], Instant::now() + timeout)?;
        let version = parse_version(&out).with_context(|| format!("unrecognised `git --version` output {out:?}"))?;
        if version < MIN_VERSION {
            bail!(
                "git {}.{} is too old; scout sync needs {}.{} or newer",
                version.0,
                version.1,
                MIN_VERSION.0,
                MIN_VERSION.1
            );
        }
        Ok(git)
    }

    /// Makes `dir` a checkout of the remote's branch tip: a shallow clone the first time,
    /// a shallow fetch plus hard reset afterwards. Only sync writes to `dir`, so there is
    /// never local work to merge or keep.
    ///
    /// The timeout bounds the whole call, not each git command: a hung remote then costs
    /// one timeout per cycle, not one per command.
    ///
    /// The caller must hold the mirror lock ([`lock_mirrors`]): stale git locks are cleared
    /// on that basis.
    pub fn sync(&self, remote: &Remote, dir: &Path) -> anyhow::Result<Synced> {
        validate_remote(remote)?;
        let deadline = Instant::now() + self.timeout;
        if !dir.join(".git").exists() {
            if dir.exists() {
                // Never delete a directory sync did not create.
                bail!("{} exists but is not a git working copy", dir.display());
            }
            return self.clone_into(remote, dir, deadline);
        }
        clear_stale_locks(&dir.join(".git"))?;
        let current_url = self.run(Some(dir), &["remote", "get-url", "origin"], deadline)?;
        if current_url != remote.url {
            self.run(Some(dir), &["remote", "set-url", "origin", &remote.url], deadline)?;
        }
        let refspec = remote.branch.as_deref().unwrap_or("HEAD");
        self.run(Some(dir), &["fetch", "--quiet", "--depth", "1", "--no-tags", "origin", refspec], deadline)?;
        let fetched = self.run(Some(dir), &["rev-parse", "FETCH_HEAD^{commit}"], deadline)?;
        let head = self.run(Some(dir), &["rev-parse", "HEAD"], deadline)?;
        if fetched == head {
            return Ok(Synced { commit: head, moved: false });
        }
        self.run(Some(dir), &["reset", "--quiet", "--hard", &fetched], deadline)?;
        Ok(Synced { commit: fetched, moved: true })
    }

    /// Clones into a sibling `.partial` directory and renames it into place, so a clone
    /// that dies halfway never leaves a `dir` that later cycles would try to fetch into.
    fn clone_into(&self, remote: &Remote, dir: &Path, deadline: Instant) -> anyhow::Result<Synced> {
        let partial = partial_path(dir)?;
        if partial.exists() {
            // Left by an earlier clone that failed; it is ours by name.
            std::fs::remove_dir_all(&partial).with_context(|| format!("removing {}", partial.display()))?;
        }
        let parent = dir.parent().with_context(|| format!("working copy {} has no parent", dir.display()))?;
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let partial_str = partial.to_str().with_context(|| format!("path is not UTF-8: {}", partial.display()))?;
        // The index holds exactly the repository's bytes, so line endings are never
        // converted (Windows git defaults to core.autocrlf=true).
        let mut args = vec!["clone", "--quiet", "--depth", "1", "--single-branch", "--no-tags"];
        args.extend(["--config", "core.autocrlf=false"]);
        if let Some(b) = &remote.branch {
            args.extend(["--branch", b]);
        }
        args.extend(["--", &remote.url, partial_str]);
        self.run(None, &args, deadline)?;
        std::fs::rename(&partial, dir).with_context(|| format!("moving clone into {}", dir.display()))?;
        let commit = self.run(Some(dir), &["rev-parse", "HEAD"], deadline)?;
        Ok(Synced { commit, moved: true })
    }

    /// Runs git and returns its trimmed stdout. Errors carry the arguments and git's
    /// stderr; the token travels only in the environment, so it is in neither.
    fn run(&self, dir: Option<&Path>, args: &[&str], deadline: Instant) -> anyhow::Result<String> {
        let mut cmd = Command::new("git");
        // A transfer slower than 1 KB/s for 60 s is a dead connection: git aborts it
        // rather than waiting out the hard deadline below.
        cmd.args(["-c", "http.lowSpeedLimit=1000", "-c", "http.lowSpeedTime=60"]);
        if let Some(d) = dir {
            cmd.arg("-C").arg(d);
        }
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for var in INHERITED_GIT_ENV {
            cmd.env_remove(var);
        }
        for (var, _) in std::env::vars_os() {
            if var.to_str().is_some_and(|v| v.starts_with(TRACE_ENV_PREFIX)) {
                cmd.env_remove(var);
            }
        }
        // Never wait on a credential prompt nobody will answer: fail instead.
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        if let Some(t) = &self.token {
            // Scoped to the token's own host so it is never sent to any other remote. Basic
            // auth with user "x-access-token" is the form GitHub documents for tokens over
            // git's smart HTTP (and what actions/checkout sends).
            let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{}", t.secret()));
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", format!("http.{}.extraheader", self.auth_prefix))
                .env("GIT_CONFIG_VALUE_0", format!("AUTHORIZATION: basic {basic}"));
        }
        let what = || format!("git {}", args.join(" "));
        crate::tree::isolate(&mut cmd);
        let mut child = cmd.spawn().context("running git (is it installed and on PATH?)")?;
        // Drained on threads so a chatty git never blocks on a full pipe while we wait.
        let stdout = drain(child.stdout.take().context("no stdout pipe")?);
        let stderr = drain(child.stderr.take().context("no stderr pipe")?);
        let status = loop {
            if let Some(st) = child.try_wait().with_context(what)? {
                break st;
            }
            if Instant::now() >= deadline {
                // The deadline is the failure to report; a failed kill is added to it.
                let killed = match crate::tree::kill(&mut child) {
                    Ok(()) => String::new(),
                    Err(e) => format!("; killing it failed: {e:#}"),
                };
                bail!("{} ran past the {}s sync deadline{killed}", what(), self.timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let out = join(stdout)?;
        let err = join(stderr)?;
        if !status.success() {
            bail!("{} failed ({status}): {}", what(), String::from_utf8_lossy(&err).trim());
        }
        Ok(String::from_utf8(out).context("git printed non-UTF-8 output")?.trim().to_string())
    }
}

/// Removes every `*.lock` git left in a working copy's `.git` (top level and `refs/`:
/// index, shallow, HEAD, config, packed-refs, `FETCH_HEAD`, `refs/remotes/origin/*`). A git
/// killed mid-fetch leaves some of them, and every later fetch or reset would fail on
/// them, so a single timeout would wedge the repo. Safe only because the mirror lock
/// keeps any other git of ours out, and a killed git's helpers died with it
/// ([`crate::tree::kill`]).
fn clear_stale_locks(git_dir: &Path) -> anyhow::Result<()> {
    clear_locks_in(git_dir, false)?;
    let refs = git_dir.join("refs");
    if refs.exists() {
        clear_locks_in(&refs, true)?;
    }
    Ok(())
}

fn clear_locks_in(dir: &Path, recurse: bool) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        let path = entry.path();
        let kind = entry.file_type().with_context(|| format!("inspecting {}", path.display()))?;
        if kind.is_dir() {
            if recurse {
                clear_locks_in(&path, true)?;
            }
        } else if path.extension().is_some_and(|e| e == "lock") {
            std::fs::remove_file(&path).with_context(|| format!("removing stale {}", path.display()))?;
        }
    }
    Ok(())
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        pipe.read_to_end(&mut buf).map(|_| buf)
    })
}

fn join(h: std::thread::JoinHandle<std::io::Result<Vec<u8>>>) -> anyhow::Result<Vec<u8>> {
    h.join().map_err(|_| anyhow::anyhow!("git output reader panicked"))?.context("reading git output")
}

/// Rejects remotes that could inject git options or leak a credential: a URL or branch
/// starting with `-` would be read as an option, and credentials written into a URL would
/// end up in the catalog, `/status` and error messages.
pub fn validate_remote(remote: &Remote) -> anyhow::Result<()> {
    if remote.url.is_empty() || remote.url.starts_with('-') {
        bail!("invalid remote URL {:?}", remote.url);
    }
    if let Some((scheme, rest)) = remote.url.split_once("://") {
        let authority = rest.split('/').next().unwrap_or(rest);
        if let Some((userinfo, host)) = authority.rsplit_once('@') {
            // Over http(s) any user part is a credential (GitHub takes a bare token as the
            // user). Elsewhere a user is normal (`ssh://git@host`), but a password is not.
            if scheme.to_ascii_lowercase().starts_with("http") || userinfo.contains(':') {
                bail!("remote URL for host {host:?} contains credentials; put the token in token_env instead");
            }
        }
    }
    // git-check-ref-format rules; ".lock" is matched exactly as git does, not as a file extension.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    if let Some(b) = &remote.branch {
        let bad = b.is_empty()
            || b.starts_with('-')
            || b.starts_with('/')
            || b.ends_with('/')
            || b.ends_with(".lock")
            || b.contains("..")
            || b.contains("@{")
            || b.chars().any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c));
        if bad {
            bail!("invalid branch name {b:?}");
        }
    }
    Ok(())
}

/// Names in a mirror directory that are scout's own bookkeeping, never working copies.
/// Repo names are sanitised to [a-z0-9_-], so neither collides with one.
pub const LOCK_NAME: &str = ".scout-sync.lock";
pub const MARKER_NAME: &str = ".scout-mirrors";

/// Held for a whole sync cycle so two sync processes never work in one mirror directory.
/// An OS lock: released if the process dies, so a crash never wedges later cycles.
///
/// The directory is scout's: each cycle deletes whatever in it is not a working copy it
/// keeps. So a directory is only adopted when it is new or empty (a marker file records
/// that); pointing `mirror_dir` at a directory with other content fails instead of
/// emptying it.
pub fn lock_mirrors(mirror_dir: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(mirror_dir).with_context(|| format!("creating {}", mirror_dir.display()))?;
    let path = mirror_dir.join(LOCK_NAME);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock().with_context(|| format!("locking {}", path.display()))?;
    // Checked under the lock, so two first runs never see each other's files.
    let marker = mirror_dir.join(MARKER_NAME);
    if !marker.exists() {
        for entry in std::fs::read_dir(mirror_dir).with_context(|| format!("listing {}", mirror_dir.display()))? {
            let name = entry.with_context(|| format!("listing {}", mirror_dir.display()))?.file_name();
            if name == LOCK_NAME {
                continue;
            }
            bail!(
                "mirror_dir {} is not empty (it has {}) and is not a scout mirror directory; scout deletes \
                 anything in it that is not a working copy, so give it a directory of its own",
                mirror_dir.display(),
                name.display()
            );
        }
        std::fs::write(&marker, b"scout sync owns this directory\n")
            .with_context(|| format!("writing {}", marker.display()))?;
    }
    Ok(file)
}

/// Deletes the working copy of a repo that left the configuration, or any other entry a
/// sweep found in the mirror directory. Only paths directly
/// under `mirror_dir` are accepted, so a bad name can never reach outside it.
pub fn remove_working_copy(mirror_dir: &Path, dir: &Path) -> anyhow::Result<()> {
    if dir.parent() != Some(mirror_dir) {
        bail!("refusing to delete {}: not directly under {}", dir.display(), mirror_dir.display());
    }
    // A sweep also meets stray files; symlinks are removed, never followed.
    let removed = match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(dir),
        Ok(_) => std::fs::remove_file(dir),
        Err(e) => Err(e),
    };
    match removed {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", dir.display())),
    }
}

fn partial_path(dir: &Path) -> anyhow::Result<PathBuf> {
    let name =
        dir.file_name().and_then(|n| n.to_str()).with_context(|| format!("bad working copy path {}", dir.display()))?;
    Ok(dir.with_file_name(format!("{name}.partial")))
}

fn parse_version(out: &str) -> Option<(u32, u32)> {
    let v = out.strip_prefix("git version ")?;
    let mut parts = v.split(|c: char| !c.is_ascii_digit());
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_runner(token: Option<Token>) -> Git {
        Git::new(token, "https://github.com/".into(), Duration::from_secs(120)).unwrap()
    }

    /// A fake git host that answers every request 404 and reports each request's
    /// Authorization header ("-" when absent).
    fn fake_host() -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut auth = String::from("-");
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("authorization")
                    {
                        auth = value.trim().to_string();
                    }
                }
                stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                if tx.send(auth).is_err() {
                    break;
                }
            }
        });
        (base, rx)
    }

    /// What actually goes over the wire: the token reaches its own host as basic auth,
    /// never another host, and is in no error message.
    #[test]
    fn the_token_reaches_git_scoped_to_its_host_and_nowhere_else() {
        let (base, requests) = fake_host();
        let url = format!("{base}acme/r.git");
        let token = || Some(Token("s3cret-token".into()));
        let expected = base64::engine::general_purpose::STANDARD.encode("x-access-token:s3cret-token");

        let scoped = Git::new(token(), base.clone(), Duration::from_secs(60)).unwrap();
        let err = format!("{:#}", scoped.run(None, &["ls-remote", "--", &url], soon()).unwrap_err());
        assert_eq!(requests.recv_timeout(Duration::from_secs(10)).unwrap(), format!("basic {expected}"));
        assert!(!err.contains("s3cret") && !err.contains(&expected), "{err}");

        let elsewhere = Git::new(token(), "https://github.com/".into(), Duration::from_secs(60)).unwrap();
        assert!(elsewhere.run(None, &["ls-remote", "--", &url], soon()).is_err());
        assert_eq!(requests.recv_timeout(Duration::from_secs(10)).unwrap(), "-", "no header for other hosts");
    }

    /// A remote that accepts the connection and never answers: only the deadline ends it,
    /// and git plus its http helper must be gone afterwards.
    #[test]
    fn a_hung_remote_is_killed_at_the_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/r.git", listener.local_addr().unwrap());
        let held = std::thread::spawn(move || listener.accept().map(|(s, _)| s));
        let g = Git::new(None, "https://github.com/".into(), Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        let err = format!(
            "{:#}",
            g.sync(&Remote { url, branch: None }, &tempfile::tempdir().unwrap().path().join("r")).unwrap_err()
        );
        assert!(err.contains("ran past the 2s sync deadline"), "{err}");
        assert!(!err.contains("killing it failed"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(30));
        drop(held.join().unwrap());
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn remotes_that_could_inject_options_or_leak_credentials_are_rejected() {
        let ok = |url: &str, branch: Option<&str>| {
            validate_remote(&Remote { url: url.into(), branch: branch.map(str::to_string) }).is_ok()
        };
        assert!(ok("https://github.com/a/b", Some("release/1.2")));
        assert!(ok("git@github.com:a/b.git", None));
        assert!(!ok("--upload-pack=touch /tmp/x", None));
        assert!(!ok("https://user:pat@github.com/a/b", None));
        assert!(!ok("https://pat@github.com/a/b", None));
        assert!(!ok("HTTPS://user:pat@github.com/a/b", None), "schemes are case-insensitive");
        assert!(!ok("ssh://user:pw@host/a/b", None), "a password is a credential on any scheme");
        assert!(ok("ssh://git@github.com/a/b.git", None), "a bare ssh user is not");
        assert!(!ok("https://github.com/a/b", Some("--upload-pack=x")));
        assert!(!ok("https://github.com/a/b", Some("a..b")));
        assert!(!ok("https://github.com/a/b", Some("has space")));
    }

    #[test]
    fn a_pinned_branch_is_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let (url, work) = remote(tmp.path());
        git(&work, &["checkout", "--quiet", "-b", "dev"]);
        std::fs::write(work.join("a.txt"), "dev\n").unwrap();
        commit(&work, "dev");
        git(&work, &["push", "--quiet", "origin", "dev"]);
        let g = git_runner(None);
        let dir = tmp.path().join("m").join("r");
        g.sync(&Remote { url, branch: Some("dev".into()) }, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "dev\n");
    }

    fn git(dir: &Path, args: &[&str]) {
        let st = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
        assert!(st.success(), "git {args:?}");
    }

    /// A bare remote with one commit, plus a scratch clone to push further commits from.
    fn remote(tmp: &Path) -> (String, PathBuf) {
        let bare = tmp.join("remote.git");
        let work = tmp.join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(tmp, &["init", "--quiet", "--bare", "--initial-branch=main", bare.to_str().unwrap()]);
        git(&work, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        commit(&work, "one");
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&work, &["push", "--quiet", "origin", "main"]);
        // file:// rather than a path: only then does git honour --depth for a local clone.
        (format!("file:///{}", bare.to_str().unwrap().replace('\\', "/").trim_start_matches('/')), work)
    }

    fn commit(work: &Path, msg: &str) {
        git(work, &["add", "-A"]);
        git(
            work,
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
                msg,
            ],
        );
    }

    #[test]
    fn clones_then_follows_the_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let (url, work) = remote(tmp.path());
        let g = git_runner(None);
        let dir = tmp.path().join("mirrors").join("r");
        let remote = Remote { url, branch: None };

        let first = g.sync(&remote, &dir).unwrap();
        assert!(first.moved);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert_eq!(g.run(Some(&dir), &["rev-parse", "--is-shallow-repository"], soon()).unwrap(), "true");

        let idle = g.sync(&remote, &dir).unwrap();
        assert_eq!(idle, Synced { commit: first.commit.clone(), moved: false });

        std::fs::write(work.join("a.txt"), "two\n").unwrap();
        std::fs::write(work.join("b.txt"), "new\n").unwrap();
        commit(&work, "two");
        git(&work, &["push", "--quiet", "origin", "main"]);
        // A git killed at its deadline leaves its locks; the next sync must not wedge on them.
        std::fs::write(dir.join(".git").join("index.lock"), "").unwrap();
        std::fs::write(dir.join(".git").join("shallow.lock"), "").unwrap();
        let next = g.sync(&remote, &dir).unwrap();
        assert!(next.moved && next.commit != first.commit);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "two\n");
        assert!(dir.join("b.txt").exists());
    }

    #[test]
    fn failures_are_errors_and_foreign_directories_are_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let g = git_runner(None);
        let missing = Remote { url: format!("file:///{}/nope.git", tmp.path().display()), branch: None };
        let dir = tmp.path().join("m").join("r");
        assert!(g.sync(&missing, &dir).is_err());
        assert!(!dir.exists(), "a failed clone leaves nothing to fetch into");

        let foreign = tmp.path().join("m").join("mine");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("keep.txt"), "x").unwrap();
        assert!(g.sync(&missing, &foreign).is_err());
        assert!(foreign.join("keep.txt").exists());

        assert!(remove_working_copy(&tmp.path().join("m"), &tmp.path().join("elsewhere")).is_err());
        remove_working_copy(&tmp.path().join("m"), &foreign).unwrap();
        assert!(!foreign.exists());
    }

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version("git version 2.52.0.windows.1"), Some((2, 52)));
        assert_eq!(parse_version("git version 2.43.0"), Some((2, 43)));
        assert_eq!(parse_version("nope"), None);
    }
}
