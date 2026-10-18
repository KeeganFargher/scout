//! The sync configuration file (TOML). Unknown keys are errors: a typo in a key would
//! otherwise silently fall back to a default and mirror the wrong thing.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// One working copy per repo, at `<mirror_dir>/<name>`.
    pub mirror_dir: PathBuf,
    pub interval_secs: u64,
    /// The env var holding the GitHub token: the file names the variable, never the
    /// secret, so the config can be committed.
    #[serde(default = "default_token_env")]
    pub token_env: String,
    /// Overridable so tests (and GitHub Enterprise) can point at another API.
    #[serde(default = "default_github_api")]
    pub github_api: String,
    /// Concurrent fetches; default `clamp(2 × cores, 4, 16)` (see [`Config::fetch_jobs`]).
    pub fetch_jobs: Option<usize>,
    /// Hard deadline for one git process (a clone of a big repo included).
    #[serde(default = "default_git_timeout")]
    pub git_timeout_secs: u64,
    #[serde(default)]
    pub repo: Vec<RepoSpec>,
    #[serde(default)]
    pub github: Vec<OwnerSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoSpec {
    pub url: String,
    pub name: Option<String>,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerSpec {
    pub owner: String,
    pub kind: OwnerKind,
    #[serde(default)]
    pub include_forks: bool,
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OwnerKind {
    Org,
    User,
}

fn default_token_env() -> String {
    "GITHUB_TOKEN".into()
}

fn default_git_timeout() -> u64 {
    900
}

fn default_github_api() -> String {
    "https://api.github.com".into()
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        anyhow::ensure!(cfg.interval_secs > 0, "interval_secs must be > 0");
        anyhow::ensure!(cfg.fetch_jobs != Some(0), "fetch_jobs must be > 0");
        anyhow::ensure!(cfg.git_timeout_secs > 0, "git_timeout_secs must be > 0");
        for r in &cfg.repo {
            let remote = scout_core::catalog::Remote { url: r.url.clone(), branch: r.branch.clone() };
            crate::git::validate_remote(&remote).with_context(|| format!("[[repo]] {}", r.url))?;
        }
        anyhow::ensure!(
            !cfg.repo.is_empty() || !cfg.github.is_empty(),
            "nothing to mirror: add [[repo]] or [[github]]"
        );
        // Relative to the config file, so a config and its mirrors can move together.
        if cfg.mirror_dir.is_relative() {
            let base = path.parent().with_context(|| format!("config path {} has no parent", path.display()))?;
            cfg.mirror_dir = std::path::absolute(base.join(&cfg.mirror_dir))
                .with_context(|| format!("resolving mirror_dir {}", cfg.mirror_dir.display()))?;
        }
        cfg.github_api = cfg.github_api.trim_end_matches('/').to_string();
        Ok(cfg)
    }

    /// The git runner this config asks for: token scoped to the API host, its timeout.
    pub fn git(&self, token: Option<crate::Token>) -> anyhow::Result<crate::git::Git> {
        let prefix = crate::auth_prefix(&self.github_api)?;
        if token.is_some() && prefix.starts_with("http://") && !is_loopback(&prefix) {
            // The API and git would both send the token in cleartext.
            anyhow::bail!("github_api {:?} is plain http; a token is only sent over https", self.github_api);
        }
        crate::git::Git::new(token, prefix, std::time::Duration::from_secs(self.git_timeout_secs))
    }

    /// Fetches wait on the network, not the CPU, so twice the cores keeps a small box busy;
    /// 16 stays polite to one git host.
    #[must_use]
    pub fn fetch_jobs(&self, cores: usize) -> usize {
        self.fetch_jobs.unwrap_or_else(|| (cores * 2).clamp(4, 16))
    }
}

/// Plain http to this machine never crosses a network, so tests and local proxies may use it.
fn is_loopback(prefix: &str) -> bool {
    prefix
        .trim_start_matches("http://")
        .split(['/', ':'])
        .next()
        .is_some_and(|h| matches!(h, "127.0.0.1" | "localhost"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> anyhow::Result<Config> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("sync.toml");
        std::fs::write(&path, text)?;
        Config::load(&path)
    }

    #[test]
    fn parses_repos_and_owners_and_rejects_typos() {
        let cfg = load(
            "mirror_dir = 'm'\ninterval_secs = 60\n[[repo]]\nurl = 'https://github.com/a/b'\nbranch = 'dev'\n\
             [[github]]\nowner = 'rust-lang'\nkind = 'org'\n",
        )
        .unwrap();
        assert!(cfg.mirror_dir.is_absolute() && cfg.mirror_dir.ends_with("m"));
        assert_eq!(cfg.token_env, "GITHUB_TOKEN");
        assert_eq!(cfg.repo[0].branch.as_deref(), Some("dev"));
        assert_eq!(cfg.github[0].kind, OwnerKind::Org);
        assert!(!cfg.github[0].include_forks);
        assert_eq!(cfg.fetch_jobs(1), 4);
        assert_eq!(cfg.fetch_jobs(30), 16);

        assert!(load("mirror_dir = 'm'\ninterval_secs = 60\n[[repo]]\nurl = 'x'\nbrnach = 'dev'\n").is_err());
        assert!(load("mirror_dir = 'm'\ninterval_secs = 0\n[[repo]]\nurl = 'x'\n").is_err());
        assert!(load("mirror_dir = 'm'\ninterval_secs = 60\n").is_err());
    }
}
