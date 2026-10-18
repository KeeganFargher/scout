//! One sync cycle: resolve what to mirror, bring every working copy to its remote's tip,
//! reindex the repos whose commit moved, and publish (docs/design.md, "Hosted sync").

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use scout_core::catalog::{Catalog, RepoEntry, now_unix};
use scout_core::ports::ShardStore;
use scout_core::snapshots::SnapshotPack;
use scout_store_fs::FsStore;
use scout_sync::config::Config;
use scout_sync::git::{Git, LOCK_NAME, MARKER_NAME, Synced, remove_working_copy};
use scout_sync::{Desired, Token, normalise_url};

use crate::ingest::{self, Budget, Job};
use crate::{err_line, host};

pub struct CycleReport {
    pub reindexed: Vec<String>,
    pub removed: Vec<String>,
    /// `(what, why)`: repos or owners that could not be resolved, fetched or indexed.
    /// Each keeps its previous index; the next cycle retries.
    pub failures: Vec<(String, String)>,
    /// The catalog as published at the end of the cycle.
    pub catalog: Catalog,
    pub elapsed: Duration,
    pub finished_unix: u64,
}

pub fn run_cycle(
    store: &FsStore,
    cfg: &Config,
    git: &Git,
    token: Option<&Token>,
    budget: Budget,
) -> anyhow::Result<CycleReport> {
    let started = Instant::now();
    let resolved = scout_sync::resolve(cfg, token);
    let mut failures = resolved.failures;

    // Two locks, so the slow network part never blocks catalog writers. The mirror lock
    // keeps two sync processes off the same working copies for the whole cycle; the store's
    // writer lock is only taken once fetching is done (below).
    let _mirrors = scout_sync::git::lock_mirrors(&cfg.mirror_dir)?;
    let before = store.load_catalog()?;
    let wanted: Vec<Desired> = resolved.desired.into_iter().filter(|d| claimable(&before, d, &mut failures)).collect();
    let fetched = fetch_all(git, &cfg.mirror_dir, &wanted, cfg.fetch_jobs(host::cores()));

    let held = store.lock_writer()?;
    // Reloaded: another writer may have changed the catalog while we fetched.
    let mut catalog = store.load_catalog()?;
    let mut jobs = Vec::new();
    let mut wanted_names = HashSet::new();
    for (d, result) in wanted.into_iter().zip(fetched) {
        // Another writer may have taken the name while we fetched.
        if !claimable(&catalog, &d, &mut failures) {
            continue;
        }
        wanted_names.insert(d.name.clone());
        let synced = match result {
            Ok(s) => s,
            Err(e) => {
                failures.push((d.name.clone(), format!("{e:#}")));
                continue;
            }
        };
        let root = path_string(&cfg.mirror_dir.join(&d.name))?;
        let Some(entry) = catalog.find_mut(&d.name) else {
            // Registered only once its first index succeeds (below), so a repo that fails
            // to clone or index never shows up unindexed to searches.
            jobs.push(Job {
                repo: RepoEntry { name: d.name, root, remote: Some(d.remote), index: None },
                commit: Some(synced.commit),
            });
            continue;
        };
        // The remote's spelling or branch, or mirror_dir, may have changed in the config;
        // the entry follows (`claimable` refused a different remote). Shards store
        // repo-relative paths and searches take the root from the catalog, so a moved root
        // needs no reindex.
        entry.remote = Some(d.remote);
        entry.root = root;
        if entry.index.as_ref().and_then(|i| i.commit.as_deref()) != Some(synced.commit.as_str()) {
            jobs.push(Job { repo: entry.clone(), commit: Some(synced.commit) });
        }
    }

    let mut pack = SnapshotPack::load(store)?;
    let gone = if resolved.complete { remove_gone(&mut catalog, &mut pack, &wanted_names)? } else { Vec::new() };

    let new_entries: HashMap<String, RepoEntry> = jobs
        .iter()
        .filter(|j| catalog.find(&j.repo.name).is_none())
        .map(|j| (j.repo.name.clone(), j.repo.clone()))
        .collect();
    let outcomes = ingest::index_repos(store, &held, &jobs, &crate::NATIVE, budget)?;
    let mut reindexed = Vec::new();
    for o in &outcomes {
        match &o.result {
            Ok(_) => {
                reindexed.push(o.name.clone());
                if let Some(entry) = new_entries.get(&o.name) {
                    catalog.repos.push(entry.clone());
                }
            }
            Err(e) => failures.push((o.name.clone(), format!("indexing: {e:#}"))),
        }
    }
    ingest::apply(&mut catalog, &mut pack, outcomes)?;
    let catalog = ingest::commit(store, &held, catalog, &pack)?;
    drop(held);

    // Only now that no published catalog names them may working copies go: until the
    // commit, strict CLI searches could still be walking them.
    failures.extend(sweep_mirrors(&cfg.mirror_dir, &catalog, &wanted_names)?);
    let removed = gone.into_iter().map(|e| e.name).collect();
    Ok(CycleReport { reindexed, removed, failures, catalog, elapsed: started.elapsed(), finished_unix: now_unix()? })
}

/// False (and a failure recorded) when `d`'s name belongs to something else: a repo added
/// by path, or a mirror of a different remote. Taking it over would reset that working
/// copy to an unrelated repo and replace its index; the operator decides instead.
fn claimable(catalog: &Catalog, d: &Desired, failures: &mut Vec<(String, String)>) -> bool {
    let Some(entry) = catalog.find(&d.name) else { return true };
    let why = match &entry.remote {
        None => format!("name {:?} is taken by a repo added by path", d.name),
        Some(r) if normalise_url(&r.url) == normalise_url(&d.remote.url) => return true,
        Some(r) => format!("name {:?} already mirrors {}; `scout remove {}` first to reuse it", d.name, r.url, d.name),
    };
    failures.push((d.remote.url.clone(), why));
    false
}

/// Deletes everything in `mirror_dir` except what any catalog repo is rooted in and repos
/// wanted this cycle (a fresh clone whose first index failed is kept for the retry).
/// That covers removed repos, `.partial` clones and anything a failed cycle left behind:
/// the directory converges on what the catalog says, whatever happened before. It is
/// scout's own ([`scout_sync::git::lock_mirrors`] only adopts a new or empty one).
fn sweep_mirrors(
    mirror_dir: &Path,
    catalog: &Catalog,
    wanted: &HashSet<String>,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut keep: HashSet<std::ffi::OsString> = wanted.iter().map(std::ffi::OsString::from).collect();
    // Every repo the catalog roots inside mirror_dir is kept, path-added ones included
    // (`scout add <mirror_dir>/x`, or deeper): the sweep must never delete what a user
    // registered, only what sync itself left behind.
    for r in &catalog.repos {
        if let Ok(inside) = Path::new(&r.root).strip_prefix(mirror_dir)
            && let Some(std::path::Component::Normal(top)) = inside.components().next()
        {
            keep.insert(top.to_os_string());
        }
    }
    keep.insert(LOCK_NAME.into());
    keep.insert(MARKER_NAME.into());
    let mut failures = Vec::new();
    let entries =
        std::fs::read_dir(mirror_dir).map_err(|e| anyhow::anyhow!("listing {}: {e}", mirror_dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| anyhow::anyhow!("listing {}: {e}", mirror_dir.display()))?;
        if keep.contains(&entry.file_name()) {
            continue;
        }
        if let Err(e) = remove_working_copy(mirror_dir, &entry.path()) {
            // Out of the index either way; a leftover only costs disk until the next sweep.
            failures.push((entry.path().display().to_string(), format!("removing: {e:#}")));
        }
    }
    Ok(failures)
}

/// Syncs every working copy on `jobs` threads; results are in `wanted`'s order.
fn fetch_all(git: &Git, mirror_dir: &Path, wanted: &[Desired], jobs: usize) -> Vec<anyhow::Result<Synced>> {
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(wanted.len()));
    std::thread::scope(|s| {
        for _ in 0..jobs.min(wanted.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(d) = wanted.get(i) else { break };
                    let r = git.sync(&d.remote, &mirror_dir.join(&d.name));
                    results.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((i, r));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
    results.sort_unstable_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, r)| r).collect()
}

/// Takes synced repos no longer wanted out of the catalog (shards retired) and the pack.
/// Repos added by path have no remote and are never touched. Their working copies are
/// deleted by the caller once the catalog without them is published.
fn remove_gone(
    catalog: &mut Catalog,
    pack: &mut SnapshotPack,
    wanted: &HashSet<String>,
) -> anyhow::Result<Vec<RepoEntry>> {
    let gone: Vec<String> = catalog
        .repos
        .iter()
        .filter(|r| r.remote.is_some() && !wanted.contains(&r.name))
        .map(|r| r.name.clone())
        .collect();
    let now = now_unix()?;
    let mut removed = Vec::with_capacity(gone.len());
    for name in &gone {
        removed.push(catalog.remove_repo(name, now)?);
        pack.entries.remove(name);
    }
    Ok(removed)
}

fn path_string(p: &Path) -> anyhow::Result<String> {
    Ok(p.to_str().ok_or_else(|| anyhow::anyhow!("path is not UTF-8: {}", p.display()))?.to_string())
}

/// Prints a cycle's outcome, one line per failure.
pub fn log_report(r: &CycleReport) {
    err_line(&format!(
        "sync: {} reindexed, {} removed, {} failed, {} repos in catalog, {:.1}s",
        r.reindexed.len(),
        r.removed.len(),
        r.failures.len(),
        r.catalog.repos.len(),
        r.elapsed.as_secs_f64()
    ));
    for (what, why) in &r.failures {
        err_line(&format!("  {what}: {why}"));
    }
}
