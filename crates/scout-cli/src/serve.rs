//! `scout serve`: the sync loop plus an HTTP search endpoint over the current generation.
//!
//! A *generation* is one catalog with all its shards open. Searches clone the `Arc` of
//! the current one and finish on it even if a newer one is swapped in meanwhile; its
//! shards are unmapped when the last such search drops it. Searches run with
//! `--freshness off`: the working copies only change inside a cycle, which publishes the
//! matching index before the swap (docs/design.md, "Hosted sync").

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Context;
use scout_core::catalog::{Catalog, now_unix};
use scout_core::ports::ShardStore;
use scout_core::rank::Weights;
use scout_core::ranked::{RankOptions, search_ranked};
use scout_core::search::{
    BoundedOptions, CaseMode, Freshness, Limits, OutputMode, RepoHandle, SearchOptions, search, search_bounded,
};
use scout_index_native::{ContentCodec, LookupHome, NativeBackend, TableBudget};
use scout_source_fs::{ChangeDetection, FsContext};
use scout_store_fs::FsStore;
use scout_sync::Token;
use scout_sync::config::Config;
use serde::Serialize;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::ingest::{Budget, Content};
use crate::sync::{log_report, run_cycle};
use crate::{err_line, host};

pub struct ServeOptions {
    pub listen: String,
    /// Threads per search.
    pub threads: usize,
    /// Concurrent searches.
    pub http_workers: usize,
    /// Largest search response; a bigger one is refused with 422 instead of being built.
    pub max_output_bytes: usize,
    /// Longest a search may run; past it the answer is 503, so a few broad queries
    /// cannot occupy every HTTP worker.
    pub search_timeout: Duration,
    pub budget: Budget,
    /// How each cycle's reindexed repos store their content.
    pub content: Content,
    /// Bytes of shard tables each generation may hold on the heap (E37).
    pub table_bytes: u64,
}

struct Generation {
    id: u64,
    catalog: Catalog,
    repos: Vec<RepoHandle>,
}

/// What a generation serves: each repo's name, root and shards. Two catalogs with the
/// same fingerprint search identically, whatever else (timestamps, retired list) differs.
fn fingerprint(catalog: &Catalog) -> Vec<(&str, &str, Option<&[String]>)> {
    catalog
        .repos
        .iter()
        .map(|r| (r.name.as_str(), r.root.as_str(), r.index.as_ref().map(|i| i.shards.as_slice())))
        .collect()
}

#[derive(Clone, Serialize)]
struct LastCycle {
    finished_unix: u64,
    elapsed_ms: u128,
    reindexed: Vec<String>,
    removed: Vec<String>,
    failures: Vec<Failure>,
    /// Set when the cycle itself failed (lock, store or clock), not just some repos.
    error: Option<String>,
}

#[derive(Clone, Serialize)]
struct Failure {
    what: String,
    why: String,
}

struct State {
    current: RwLock<Arc<Generation>>,
    last_cycle: Mutex<Option<LastCycle>>,
    threads: usize,
    table_bytes: u64,
    max_output_bytes: usize,
    search_timeout: Duration,
}

impl State {
    fn generation(&self) -> Arc<Generation> {
        Arc::clone(&self.current.read().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    fn record(&self, cycle: LastCycle) {
        *self.last_cycle.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cycle);
    }
}

pub fn serve(store: &FsStore, cfg: &Config, opts: &ServeOptions) -> anyhow::Result<()> {
    let token = Token::from_env(&cfg.token_env)?;
    let git = cfg.git(token.clone())?;
    // Serve what is already indexed right away; the first cycle starts immediately after.
    let first = open_generation(store, store.load_catalog()?, 1, opts.threads, opts.table_bytes)?;
    let state = Arc::new(State {
        current: RwLock::new(Arc::new(first)),
        last_cycle: Mutex::new(None),
        threads: opts.threads,
        table_bytes: opts.table_bytes,
        max_output_bytes: opts.max_output_bytes,
        search_timeout: opts.search_timeout,
    });

    let server =
        Arc::new(Server::http(&opts.listen).map_err(|e| anyhow::anyhow!("listening on {}: {e}", opts.listen))?);
    let addr = server.server_addr().to_ip().context("server is not bound to an IP address")?;
    err_line(&format!("listening on http://{addr}"));

    for _ in 0..opts.http_workers {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            loop {
                match server.recv() {
                    Ok(req) => {
                        // A panic in one request must not take this worker down with it:
                        // capacity would shrink silently with every bad request.
                        let url = req.url().to_string();
                        let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(&state, req)));
                        if handled.is_err() {
                            err_line(&format!("http: handler panicked on {url}"));
                        }
                    }
                    Err(e) => err_line(&format!("http: {e}")),
                }
            }
        });
    }

    // The sync loop runs on the calling thread for the life of the process. The served
    // generation is reconciled with the catalog on disk before and after every cycle, not
    // only when a cycle reports changes: a cycle that failed after publishing, a failed
    // open, or a `scout index` from the CLI all leave the two apart, and the next
    // reconcile closes the gap. Reconciling before the cycle also drops shards the CLI
    // replaced before the cycle's garbage collection deletes them.
    let mut next_id = 2;
    loop {
        reconcile(store, &state, &mut next_id);
        match run_cycle(store, cfg, &git, token.as_ref(), opts.budget, opts.content) {
            Ok(report) => {
                log_report(&report);
                state.record(LastCycle {
                    finished_unix: report.finished_unix,
                    elapsed_ms: report.elapsed.as_millis(),
                    reindexed: report.reindexed,
                    removed: report.removed,
                    failures: report.failures.into_iter().map(|(what, why)| Failure { what, why }).collect(),
                    error: None,
                });
            }
            Err(e) => {
                err_line(&format!("sync cycle failed: {e:#}"));
                match now_unix() {
                    Ok(t) => state.record(LastCycle {
                        finished_unix: t,
                        elapsed_ms: 0,
                        reindexed: Vec::new(),
                        removed: Vec::new(),
                        failures: Vec::new(),
                        error: Some(format!("{e:#}")),
                    }),
                    Err(clock) => err_line(&format!("not recording the failed cycle: {clock}")),
                }
            }
        }
        reconcile(store, &state, &mut next_id);
        std::thread::sleep(Duration::from_secs(cfg.interval_secs));
    }
}

/// Opens and swaps in a new generation when the catalog on disk serves something else
/// than the current one. Failures are logged and retried on the next call.
fn reconcile(store: &FsStore, state: &State, next_id: &mut u64) {
    let catalog = match store.load_catalog() {
        Ok(c) => c,
        Err(e) => return err_line(&format!("reading catalog: {e}")),
    };
    if fingerprint(&catalog) == fingerprint(&state.generation().catalog) {
        return;
    }
    match open_generation(store, catalog, *next_id, state.threads, state.table_bytes) {
        Ok(g) => {
            *state.current.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(g);
            err_line(&format!("serving generation {next_id}"));
            *next_id += 1;
        }
        Err(e) => err_line(&format!("opening generation {next_id}: {e:#}")),
    }
}

/// Opens every indexed repo of `catalog`. Unindexed repos are left out rather than
/// failing the generation: sync only registers repos after their first index, so these
/// can only be path-added repos nobody indexed yet.
/// Each generation's shards copy their tables to the heap within `table_bytes` of their
/// own: a generation opens while the one before still serves, and drops with its shards.
fn open_generation(
    store: &FsStore,
    catalog: Catalog,
    id: u64,
    threads: usize,
    table_bytes: u64,
) -> anyhow::Result<Generation> {
    let indexed: Vec<String> = catalog.repos.iter().filter(|r| r.index.is_some()).map(|r| r.name.clone()).collect();
    let repos = if indexed.is_empty() {
        Vec::new()
    } else {
        // Off-mode searches never look at the working copy, so change detection is moot.
        let ctx = Arc::new(FsContext::new(ChangeDetection::Scan));
        // Reading only: the codec is a writer's choice.
        let native = NativeBackend::new(LookupHome::Heap(Arc::new(TableBudget::new(table_bytes))), ContentCodec::Raw);
        crate::open_repos(store, &catalog, &indexed, threads, false, &ctx, &native)?
    };
    Ok(Generation { id, catalog, repos })
}

fn handle(state: &State, req: Request) {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let response = match (req.method(), path) {
        (Method::Get, "/search") => search_response(state, query),
        (Method::Get, "/status") => status_response(state),
        (Method::Get, _) => text(404, "not found\n".into()),
        _ => text(405, "only GET is supported\n".into()),
    };
    if let Err(e) = req.respond(response) {
        err_line(&format!("http: responding to {url}: {e}"));
    }
}

fn header(name: &str, value: &str) -> Header {
    // Names and values here are ASCII literals or numbers, which tiny_http always accepts.
    Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap_or_else(|()| unreachable!("invalid header {name}"))
}

fn text(status: u16, body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(body.into_bytes())
        .with_status_code(status)
        .with_header(header("Content-Type", "text/plain; charset=utf-8"))
}

fn search_response(state: &State, query: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let params: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    let generation = state.generation();
    let limits = Limits {
        output_bytes: Some(state.max_output_bytes),
        deadline: Some(std::time::Instant::now() + state.search_timeout),
    };
    let request = match parse_search(&params, state.threads, limits) {
        Ok(r) => r,
        Err(msg) => return text(400, format!("{msg}\n")),
    };
    let by_name: HashMap<&str, &RepoHandle> = generation.repos.iter().map(|r| (r.name.as_str(), r)).collect();
    let selected: Vec<&RepoHandle> = if request.repos.is_empty() {
        generation.repos.iter().collect()
    } else {
        let mut v = Vec::with_capacity(request.repos.len());
        for name in &request.repos {
            match by_name.get(name.as_str()) {
                Some(r) => v.push(*r),
                None => return text(400, format!("no indexed repo named {name:?}\n")),
            }
        }
        v
    };
    let mut out = Vec::new();
    let mut bounded = None;
    let searched = if let Some(bounds) = request.bounded {
        search_bounded(&request.opts, bounds, &selected, &mut out).map(|r| {
            if request.opts.output == OutputMode::Count {
                out = serde_json::json!({"count": r.count_estimate(), "complete": r.complete(), "stop_reason": r.stop})
                    .to_string()
                    .into_bytes();
            }
            bounded = Some((r.complete(), r.stop, r.returned_matches));
            r.stats
        })
    } else {
        match request.top {
            Some(top) => {
                let rank = RankOptions { top, weights: Weights::default() };
                search_ranked(&request.opts, &rank, &selected, &mut out).map(|r| r.stats)
            }
            None => search(&request.opts, &selected, &mut out).map(|r| r.stats),
        }
    };
    match searched {
        Ok(done) => {
            let content_type = if bounded.is_some() && request.opts.output == OutputMode::Count {
                "application/json"
            } else {
                "text/plain; charset=utf-8"
            };
            let mut response = Response::from_data(out)
                .with_header(header("Content-Type", content_type))
                .with_header(header("X-Scout-Matched-Files", &done.matched_files.to_string()))
                .with_header(header("X-Scout-Generation", &generation.id.to_string()));
            if let Some((complete, stop, matches)) = bounded {
                response.add_header(header("X-Scout-Complete", if complete { "true" } else { "false" }));
                response.add_header(header("X-Scout-Stop-Reason", stop.as_str()));
                response.add_header(header("X-Scout-Returned-Matches", &matches.to_string()));
            }
            response
        }
        Err(e @ scout_core::Error::Invalid(_)) => text(400, format!("{e}\n")),
        Err(scout_core::Error::Pattern(msg)) => text(400, format!("invalid pattern: {msg}\n")),
        Err(e @ scout_core::Error::OutputLimit(_)) => {
            text(422, format!("{e}; narrow the search (repo=, m=, l=1 or c=1)\n"))
        }
        Err(e @ scout_core::Error::Deadline) => {
            text(503, format!("{e}; narrow the search (repo=, F=1, a longer literal)\n"))
        }
        Err(e) => text(500, format!("{e}\n")),
    }
}

/// Files a ranked search returns when `top` is not given; the CLI's `--top` default.
const DEFAULT_TOP: usize = 20;

struct SearchRequest {
    opts: SearchOptions,
    repos: Vec<String>,
    /// `Some(top)` for a ranked search (`rank=1`, `top=N`, default 20).
    top: Option<usize>,
    bounded: Option<BoundedOptions>,
}

/// Maps rg-style flags (`F`, `i`, `S`, `w`, `n`, `l`, `c`, `m`) and ranking (`rank`, `top`)
/// from query parameters.
/// Unknown parameters are rejected so a typo never silently searches differently.
fn parse_search(params: &[(String, String)], threads: usize, limits: Limits) -> Result<SearchRequest, String> {
    let mut pattern = None;
    let mut flags: HashMap<&str, bool> = HashMap::new();
    let mut max_count = None;
    let mut repos = Vec::new();
    let mut top = None;
    let (mut limit, mut max_bytes, mut timeout_ms) = (None, None, None);
    for (k, v) in params {
        match k.as_str() {
            "limit" | "max_bytes" | "timeout_ms" => {
                let n = v.parse::<usize>().map_err(|_| format!("{k} must be a positive integer"))?;
                if n == 0 {
                    return Err(format!("{k} must be positive"));
                }
                match k.as_str() {
                    "limit" => limit = Some(n),
                    "max_bytes" => max_bytes = Some(n),
                    _ => timeout_ms = Some(n),
                }
            }
            "q" => pattern = Some(v.clone()),
            "F" | "i" | "S" | "w" | "n" | "l" | "c" | "rank" => {
                let on = match v.as_str() {
                    "1" | "true" => true,
                    "0" | "false" => false,
                    other => return Err(format!("flag {k} must be 1/0/true/false, got {other:?}")),
                };
                flags.insert(k.as_str(), on);
            }
            "m" => max_count = Some(v.parse::<u64>().map_err(|_| format!("m must be a number, got {v:?}"))?),
            "repo" => repos.push(v.clone()),
            "top" => {
                let n = v.parse::<usize>().map_err(|_| format!("top must be a number, got {v:?}"))?;
                if n == 0 {
                    return Err("top must be at least 1".into());
                }
                top = Some(n);
            }
            other => return Err(format!("unknown parameter {other:?}")),
        }
    }
    let pattern = pattern.ok_or("missing q (the pattern)")?;
    let on = |f: &str| flags.get(f).copied().unwrap_or(false);
    if top.is_some() && !on("rank") {
        return Err("top only applies to rank=1".into());
    }
    let top = on("rank").then(|| top.unwrap_or(DEFAULT_TOP));
    if limit.is_none() && (max_bytes.is_some() || timeout_ms.is_some()) {
        return Err("max_bytes and timeout_ms require limit".into());
    }
    if limit.is_some() && (top.is_some() || on("l")) {
        return Err("limit supports unranked lines or an explicitly bounded count".into());
    }
    let bounded = limit.map(|max_matches| BoundedOptions {
        max_matches,
        max_bytes: max_bytes.unwrap_or(65_536),
        timeout: Duration::from_millis(timeout_ms.unwrap_or(1000) as u64),
    });
    if on("c") && bounded.is_some_and(|b| b.max_bytes < 256) {
        return Err("bounded count needs at least 256 bytes for count metadata".into());
    }
    Ok(SearchRequest {
        opts: SearchOptions {
            pattern,
            fixed_strings: on("F"),
            case: if on("i") {
                CaseMode::Insensitive
            } else if on("S") {
                CaseMode::Smart
            } else {
                CaseMode::Sensitive
            },
            word: on("w"),
            line_number: on("n"),
            output: if on("l") {
                OutputMode::FilesWithMatches
            } else if on("c") {
                OutputMode::Count
            } else {
                OutputMode::Lines
            },
            max_count,
            threads,
            freshness: Freshness::Off,
            limits,
        },
        repos,
        top,
        bounded,
    })
}

#[derive(Serialize)]
struct Status<'a> {
    generation: u64,
    repos: Vec<RepoStatus<'a>>,
    last_cycle: Option<LastCycle>,
}

#[derive(Serialize)]
struct RepoStatus<'a> {
    name: &'a str,
    remote: Option<&'a str>,
    commit: Option<&'a str>,
    files: Option<u64>,
    indexed_at_unix: Option<u64>,
}

fn status_response(state: &State) -> Response<std::io::Cursor<Vec<u8>>> {
    let generation = state.generation();
    let last_cycle = state.last_cycle.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let status = Status {
        generation: generation.id,
        repos: generation
            .catalog
            .repos
            .iter()
            .map(|r| RepoStatus {
                name: &r.name,
                remote: r.remote.as_ref().map(|m| m.url.as_str()),
                commit: r.index.as_ref().and_then(|i| i.commit.as_deref()),
                files: r.index.as_ref().map(|i| i.files),
                indexed_at_unix: r.index.as_ref().map(|i| i.indexed_at_unix),
            })
            .collect(),
        last_cycle,
    };
    match serde_json::to_vec_pretty(&status) {
        Ok(body) => Response::from_data(body).with_header(header("Content-Type", "application/json")),
        Err(e) => text(500, format!("serializing status: {e}\n")),
    }
}

/// Default concurrent searches: half the cores, 1 to 4. Each search already fans out over
/// every core (E16), but most agent queries are short and leave cores idle, so a second
/// concurrent search per few cores keeps a short query from queueing behind a broad one.
/// Measured under a mixed load (E17): at 4 cores 2 searches beat 1 (+10% throughput at 4
/// clients, +22% and p99 down 20–30% at 16); at 1–2 cores a second search only moved
/// latency from the selective queries to the broad ones; at 8 cores 2 and 4 were equal and
/// 8 was worse. The cap is the largest value measured as best: past it each search still
/// gets every core, and more concurrency only splits them further.
#[must_use]
pub fn default_http_workers() -> usize {
    (host::cores() / 2).clamp(1, 4)
}
