//! Hosted ingestion: which remotes to mirror ([`config`], [`github`]), and keeping a
//! working copy of each in step with its remote ([`git`]). Indexing and publishing stay in
//! the front-end, which already owns the store and backends (docs/design.md, "Hosted sync").

pub mod config;
pub mod git;
pub mod github;
mod tree;

use scout_core::catalog::{Remote, sanitize_name};

/// A credential that never prints. Error messages, logs and `{:?}` all go through
/// `Debug`, so redacting it there is what keeps the token out of every report.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Reads the token from `var`. Absent means anonymous access (public repos only);
    /// present but empty is a misconfiguration, so it fails rather than going anonymous.
    pub fn from_env(var: &str) -> anyhow::Result<Option<Self>> {
        match std::env::var(var) {
            Ok(v) if v.trim().is_empty() => anyhow::bail!("{var} is set but empty"),
            Ok(v) => Ok(Some(Self(v.trim().to_string()))),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!("{var} is not valid Unicode"),
        }
    }

    pub(crate) fn secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// One repo a cycle should mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub name: String,
    pub remote: Remote,
}

/// The URL prefix git may send the token to, derived from the API base so GitHub
/// Enterprise works too: `https://api.github.com` → `https://github.com/`, and an
/// Enterprise `https://ghe.example/api/v3` → `https://ghe.example/`.
pub fn auth_prefix(api: &str) -> anyhow::Result<String> {
    let (scheme, rest) = api.split_once("://").ok_or_else(|| anyhow::anyhow!("github_api {api:?} has no scheme"))?;
    // Lowercased so later checks (`http://` refused with a token) cannot be dodged by case.
    let scheme = scheme.to_ascii_lowercase();
    let host = rest
        .split('/')
        .next()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| anyhow::anyhow!("github_api {api:?} has no host"))?;
    let host = host.to_ascii_lowercase();
    let host = if host == "api.github.com" { "github.com".to_string() } else { host };
    Ok(format!("{scheme}://{host}/"))
}

/// One spelling per remote, so `https://github.com/A/r`, `…/a/r.git`, `…/a/r/` and
/// `git@github.com:a/r` are recognised as the same repo when a `[[repo]]` and an owner
/// listing both name it, under the same name or not. Hosts are case-insensitive; paths only
/// on GitHub, which treats owner and repo names that way (other hosts may not).
#[must_use]
pub fn normalise_url(url: &str) -> String {
    let url = url.trim_end_matches('/').trim_end_matches(".git");
    // scp-like ssh (`user@host:path`) and URLs both become `host/path`.
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    let rest = match rest.split_once('@') {
        Some((user, after)) if !user.contains('/') => after,
        _ => rest,
    };
    let (host, path) = match rest.find(['/', ':']) {
        Some(i) => (&rest[..i], rest[i + 1..].trim_start_matches('/')),
        None => (rest, ""),
    };
    let host = host.to_ascii_lowercase();
    let path = if host.contains("github") { path.to_ascii_lowercase() } else { path.to_string() };
    format!("{host}/{path}")
}

/// `<owner>-<repo>` from a remote URL's last two path segments: readable in search output
/// paths. Two remotes can still collide (`a-b/c` and `a/b-c`); [`resolve`] reports that
/// instead of guessing, and an explicit `name` resolves it.
pub fn name_from_url(url: &str) -> anyhow::Result<String> {
    let path = url.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = path.rsplit(['/', ':', '\\']).filter(|p| !p.is_empty());
    let repo = parts.next().ok_or_else(|| anyhow::anyhow!("cannot derive a repo name from {url:?}"))?;
    Ok(match parts.next() {
        Some(owner) => sanitize_name(&format!("{owner}-{repo}")),
        None => sanitize_name(repo),
    })
}

/// The repos to mirror this cycle: the configured ones, then each GitHub owner's listing.
/// Names must be unique, because a name is a working-copy directory and a catalog key.
#[must_use]
pub fn resolve(cfg: &config::Config, token: Option<&Token>) -> ResolveReport {
    let mut desired: Vec<Desired> = Vec::new();
    let mut failures = Vec::new();
    for r in &cfg.repo {
        match r.name.clone().map_or_else(|| name_from_url(&r.url), |n| Ok(sanitize_name(&n))) {
            Ok(name) => desired.push(Desired { name, remote: Remote { url: r.url.clone(), branch: r.branch.clone() } }),
            Err(e) => failures.push((r.url.clone(), format!("{e:#}"))),
        }
    }
    let configured = desired.len();
    let mut complete = true;
    for owner in &cfg.github {
        match github::list(&cfg.github_api, owner, token) {
            Ok(repos) => desired.extend(repos),
            Err(e) => {
                // A partial listing must never read as "those repos were deleted".
                complete = false;
                failures.push((format!("github {}", owner.owner), format!("{e:#}")));
            }
        }
    }
    // First mention wins, and `[[repo]]` entries come first: an explicit entry (with its
    // name and branch) overrides the same remote showing up again in an owner listing.
    // Mirroring one remote under two names would index it twice and double every result.
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut urls = std::collections::HashSet::new();
    let mut unique = Vec::with_capacity(desired.len());
    for (i, d) in desired.into_iter().enumerate() {
        if !urls.insert(normalise_url(&d.remote.url)) {
            // Listings repeating a configured remote are expected; two `[[repo]]` entries
            // for one remote are a config mistake, and which one wins must not be silent.
            if i < configured {
                failures.push((d.remote.url.clone(), "listed twice in [[repo]]; the first entry is used".into()));
            }
            continue;
        }
        if let Some(url) = names.get(&d.name) {
            failures.push((d.remote.url.clone(), format!("name {:?} is already used by {url}", d.name)));
            continue;
        }
        names.insert(d.name.clone(), d.remote.url.clone());
        unique.push(d);
    }
    ResolveReport { desired: unique, complete, failures }
}

pub struct ResolveReport {
    pub desired: Vec<Desired>,
    /// Every owner listing succeeded, so a synced repo missing from `desired` really is
    /// gone and may be removed.
    pub complete: bool,
    /// `(what, why)` for everything that could not be resolved.
    pub failures: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_owner_and_repo() {
        assert_eq!(name_from_url("https://github.com/BurntSushi/ripgrep.git").unwrap(), "burntsushi-ripgrep");
        assert_eq!(name_from_url("https://github.com/rust-lang/rust/").unwrap(), "rust-lang-rust");
        assert_eq!(name_from_url("git@github.com:owner/repo.git").unwrap(), "owner-repo");
        assert_eq!(name_from_url("file:///C:/tmp/remotes/alpha.git").unwrap(), "remotes-alpha");
        assert!(name_from_url("").is_err());
    }

    #[test]
    fn auth_prefix_follows_the_api_host() {
        assert_eq!(auth_prefix("https://api.github.com").unwrap(), "https://github.com/");
        assert_eq!(auth_prefix("https://ghe.example/api/v3").unwrap(), "https://ghe.example/");
        assert!(auth_prefix("api.github.com").is_err());
        assert_eq!(auth_prefix("HTTP://GHE.example/api").unwrap(), "http://ghe.example/");
        assert_eq!(normalise_url("https://github.com/Acme/R.git/"), normalise_url("https://github.com/acme/r"));
        assert_eq!(normalise_url("git@github.com:acme/r.git"), normalise_url("https://github.com/acme/r"));
        assert_eq!(normalise_url("ssh://git@github.com/acme/r"), normalise_url("https://github.com/acme/r"));
        assert_ne!(normalise_url("https://git.example/Team/R"), normalise_url("https://git.example/team/r"));
    }

    #[test]
    fn one_remote_is_mirrored_once_whatever_it_is_called() {
        let cfg: config::Config = toml::from_str(
            "mirror_dir = 'm'\ninterval_secs = 60\n\
             [[repo]]\nurl = 'https://github.com/acme/r'\nname = 'r'\n\
             [[repo]]\nurl = 'https://github.com/Acme/r.git'\n\
             [[repo]]\nurl = 'https://github.com/other/thing'\nname = 'r'\n",
        )
        .unwrap();
        let report = resolve(&cfg, None);
        assert_eq!(report.desired.len(), 1, "the second spelling is the same remote");
        assert_eq!(report.desired[0].name, "r");
        assert_eq!(report.failures.len(), 2, "the repeated entry and the taken name are both reported");
    }

    #[test]
    fn tokens_never_print() {
        let t = Token("ghp_secret".into());
        assert!(!format!("{t:?}").contains("secret"));
    }
}
