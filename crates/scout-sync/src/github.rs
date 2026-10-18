//! Expanding a `[[github]]` owner into its repos through the REST API.

use anyhow::{Context, bail};
use scout_core::catalog::Remote;
use serde::Deserialize;

use crate::config::{OwnerKind, OwnerSpec};
use crate::{Desired, Token, name_from_url};

const PER_PAGE: usize = 100;

#[derive(Deserialize)]
struct ApiRepo {
    clone_url: String,
    fork: bool,
    archived: bool,
}

/// Every repo of `owner` the config selects, following pagination to the end. Any failed
/// page fails the whole listing: a partial list would make missing repos look deleted.
pub fn list(api: &str, owner: &OwnerSpec, token: Option<&Token>) -> anyhow::Result<Vec<Desired>> {
    // For a user, `type=all` would add repos owned by others that they merely belong to;
    // `owner` keeps the listing to what the config names.
    let (path, kind) = match owner.kind {
        OwnerKind::Org => ("orgs", "all"),
        OwnerKind::User => ("users", "owner"),
    };
    let mut out = Vec::new();
    for page in 1.. {
        let url = format!("{api}/{path}/{}/repos?per_page={PER_PAGE}&page={page}&type={kind}", owner.owner);
        let repos: Vec<ApiRepo> = get(&url, token)?.into_json().with_context(|| format!("parsing {url}"))?;
        let last = repos.len() < PER_PAGE;
        for r in repos {
            if (r.fork && !owner.include_forks) || (r.archived && !owner.include_archived) {
                continue;
            }
            // The remote's HEAD, not a pinned default_branch: a renamed default branch is
            // followed without a config change.
            out.push(Desired { name: name_from_url(&r.clone_url)?, remote: Remote { url: r.clone_url, branch: None } });
        }
        if last {
            break;
        }
    }
    Ok(out)
}

fn get(url: &str, token: Option<&Token>) -> anyhow::Result<ureq::Response> {
    // Bounded, so a stalled API cannot stall the sync loop; the next cycle retries.
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(120))
        .build();
    let mut req = agent
        .get(url)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        // GitHub rejects API requests without a User-Agent.
        .set("User-Agent", concat!("scout/", env!("CARGO_PKG_VERSION")));
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {}", t.secret()));
    }
    match req.call() {
        Ok(resp) => Ok(resp),
        Err(ureq::Error::Status(code, resp)) => {
            if matches!(code, 403 | 429) && resp.header("x-ratelimit-remaining") == Some("0") {
                // No retry here: the next cycle is the retry, and waiting inside one would
                // stall every other repo's sync.
                let reset = match resp.header("x-ratelimit-reset") {
                    Some(r) => format!("resets at unix time {r}"),
                    None => "no reset time given".to_string(),
                };
                bail!("GitHub rate limit exhausted for {url}; {reset}");
            }
            let body = match resp.into_string() {
                Ok(b) => b,
                Err(e) => format!("<error body unreadable: {e}>"),
            };
            bail!("GET {url} returned {code}: {}", body.chars().take(300).collect::<String>())
        }
        Err(e) => Err(e).with_context(|| format!("GET {url}")),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use super::*;

    /// Status, extra headers, body.
    type Canned = (u16, Vec<(&'static str, String)>, String);

    /// A one-thread fake API: answers each request with the next canned response and
    /// records the request line and Authorization header.
    fn fake(responses: Vec<Canned>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, headers, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut auth = String::from("-");
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.strip_prefix("authorization: ").or_else(|| h.strip_prefix("Authorization: ")) {
                        auth = v.trim().to_string();
                    }
                }
                seen.push(format!("{} {auth}", line.split_whitespace().nth(1).unwrap()));
                let extra = headers.iter().fold(String::new(), |acc, (k, v)| acc + k + ": " + v + "\r\n");
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            seen
        });
        (base, handle)
    }

    fn repos(n: usize, start: usize) -> String {
        let items: Vec<String> = (start..start + n)
            .map(|i| {
                format!(
                    r#"{{"clone_url":"https://github.com/acme/r{i}.git","fork":{},"archived":{}}}"#,
                    i % 7 == 1,
                    i % 7 == 2
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    fn owner() -> OwnerSpec {
        OwnerSpec { owner: "acme".into(), kind: OwnerKind::Org, include_forks: false, include_archived: false }
    }

    #[test]
    fn follows_pages_filters_and_sends_the_token() {
        let (api, server) = fake(vec![(200, vec![], repos(100, 0)), (200, vec![], repos(5, 100))]);
        let token = Token("t0ken".into());
        let got = list(&api, &owner(), Some(&token)).unwrap();
        let seen = server.join().unwrap();
        assert_eq!(seen[0], "/orgs/acme/repos?per_page=100&page=1&type=all Bearer t0ken");
        assert!(seen[1].starts_with("/orgs/acme/repos?per_page=100&page=2"));
        let excluded = (0..105).filter(|i| i % 7 == 1 || i % 7 == 2).count();
        assert_eq!(got.len(), 105 - excluded);
        assert_eq!(
            got[0],
            Desired {
                name: "acme-r0".into(),
                remote: Remote { url: "https://github.com/acme/r0.git".into(), branch: None }
            }
        );
    }

    #[test]
    fn rate_limits_and_errors_fail_the_listing() {
        let limited = vec![("x-ratelimit-remaining", "0".to_string()), ("x-ratelimit-reset", "1760000000".to_string())];
        let (api, server) = fake(vec![(403, limited, "{}".into())]);
        let err = format!("{:#}", list(&api, &owner(), None).unwrap_err());
        server.join().unwrap();
        assert!(err.contains("rate limit") && err.contains("1760000000"), "{err}");

        // A failure on a later page discards the pages already read.
        let (api, server) = fake(vec![(200, vec![], repos(100, 0)), (500, vec![], "boom".into())]);
        assert!(list(&api, &owner(), None).is_err());
        server.join().unwrap();
    }
}
