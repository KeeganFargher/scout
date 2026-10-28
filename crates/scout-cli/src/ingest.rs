//! Indexing repos and publishing the result, shared by `scout index` and sync cycles.
//!
//! Publishing order is what keeps concurrent searches correct (docs/design.md, "Hosted
//! sync"): shards under fresh generation names first, then the catalog in one atomic
//! rename, and replaced shards are only deleted a grace period later.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use scout_core::catalog::{Catalog, RepoEntry, now_unix};
use scout_core::indexer::{IndexOptions, IndexReport, index_repo, new_generation};
use scout_core::ports::{IndexBackend, RepoSource, ShardStore, WriterLock};
use scout_core::snapshots::{PACK_NAME, SnapshotPack};
use scout_index_native::{ContentCodec, FilterHome, NativeBackend};
use scout_index_sqlite::SqliteBackend;
use scout_source_fs::{ChangeDetection, FsContext, FsSource};

use crate::{err_line, host};

/// Threads and memory an indexing run may use.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub threads: usize,
    pub memory: u64,
}

impl Budget {
    /// `threads` defaults to the cores, memory to a quarter of RAM capped at 4 GiB: enough
    /// for 512 MiB shards on big machines while leaving a 2 GB box room to serve searches.
    pub fn for_host(threads: Option<usize>, memory_mb: Option<u64>) -> anyhow::Result<Self> {
        let memory = match memory_mb {
            Some(mb) => mb << 20,
            None => (host::usable_memory()? / 4).min(4 << 30),
        };
        Ok(Self { threads: threads.unwrap_or_else(host::cores), memory })
    }
}

/// How a native index stores file content (E29).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content {
    Raw,
    Lz4,
    /// lz4 when the index's content would take more than half of usable memory, else raw:
    /// see [`resolve_content`].
    Auto,
}

/// What an index run writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Native(Content),
    Sqlite,
}

/// The codec for an index whose content totals `total` bytes on a machine with `usable`
/// bytes of memory. Raw while the content takes at most half of memory: the index is the
/// content plus 15-30% of postings and tables, and it is fast only while the page cache
/// holds all of it (E21: one 1.07 GB index ran at 36-40 ms down to a 1280 MiB limit, 5.6x
/// slower at 1 GiB), next to the process and everything else on the box. Past that, the
/// pages a search misses are read again from disk on every query, which on a network
/// volume costs far more than decoding lz4 content, ~3x smaller, that the page cache can
/// hold (E28, E29). Below it, lz4 would only add a decode to every read.
fn resolve_content(content: Content, total: u64, usable: u64) -> ContentCodec {
    match content {
        Content::Lz4 => ContentCodec::Lz4,
        Content::Auto if total > usable / 2 => ContentCodec::Lz4,
        Content::Raw | Content::Auto => ContentCodec::Raw,
    }
}

/// One repo to index, and the commit its working copy is at (synced repos only).
pub struct Job {
    pub repo: RepoEntry,
    pub commit: Option<String>,
}

pub struct Outcome {
    pub name: String,
    pub result: anyhow::Result<IndexReport>,
}

/// Splits the memory budget across indexing workers. Each worker holds one shard's
/// postings in memory, roughly 5x the shard's content size, so the per-worker shard cap is
/// budget / workers / 5. On small machines we run fewer workers rather than shrinking
/// shards below 16 MiB, which would hurt query speed more than slower indexing hurts.
fn plan_indexing(threads: usize, budget: u64, repos: usize) -> (usize, u64) {
    const MIN_SHARD: u64 = 16 << 20;
    const MAX_SHARD: u64 = 512 << 20;
    const OVERHEAD: u64 = 5;
    let max_workers_for_budget = usize::try_from(budget / (MIN_SHARD * OVERHEAD)).unwrap_or(usize::MAX).max(1);
    let workers = threads.min(max_workers_for_budget).min(repos).max(1);
    let cap = (budget / workers as u64 / OVERHEAD).clamp(MIN_SHARD, MAX_SHARD);
    (workers, cap)
}

/// Builds and stages-then-publishes shards for every job, reporting each repo as it
/// finishes. Nothing becomes visible to searches until [`commit`] saves the catalog.
/// `other_content` is the content of the registered repos this run leaves as they are,
/// which [`Content::Auto`] counts with the jobs' own.
pub fn index_repos(
    store: &dyn ShardStore,
    _held: &WriterLock,
    jobs: &[Job],
    format: Format,
    other_content: u64,
    budget: Budget,
) -> anyhow::Result<Vec<Outcome>> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let ctx = Arc::new(FsContext::new(ChangeDetection::Journal));
    let native;
    let (backend, content): (&dyn IndexBackend, String) = match format {
        Format::Sqlite => (&SqliteBackend, String::new()),
        Format::Native(content) => {
            let codec = match content {
                Content::Auto => {
                    // The budget may come from --memory-mb because usable memory cannot be
                    // read here; auto needs the real figure, so it names its own way out.
                    let usable = host::usable_memory().context("--content auto (pass --content raw or lz4)")?;
                    let total = other_content + listed_bytes(jobs, budget.threads, &ctx)?;
                    resolve_content(content, total, usable)
                }
                Content::Raw | Content::Lz4 => resolve_content(content, 0, 0),
            };
            native = NativeBackend::new(FilterHome::Map, codec);
            (&native, format!(", content {codec:?}"))
        }
    };
    let (workers, cap) = plan_indexing(budget.threads, budget.memory, jobs.len());
    let walk_threads = (budget.threads / workers).max(1);
    err_line(&format!(
        "indexing {} repo(s): {workers} worker(s), shard cap {} MiB, budget {} MiB, backend {}{content}",
        jobs.len(),
        cap >> 20,
        budget.memory >> 20,
        backend.name()
    ));
    // One CPU reads inline: a read-ahead thread only adds handoff cost there (E25).
    let read_threads = if budget.threads == 1 { 0 } else { walk_threads };
    let opts = IndexOptions { shard_content_cap: cap, generation: new_generation()?, read_threads };
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(jobs.len()));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    let t = Instant::now();
                    let result = FsSource::new(Path::new(&job.repo.root), walk_threads, Arc::clone(&ctx))
                        .and_then(|src| index_repo(&job.repo.name, &src, backend, store, opts))
                        .map(|mut rep| {
                            rep.index.commit.clone_from(&job.commit);
                            rep
                        })
                        .map_err(anyhow::Error::from);
                    report(&job.repo.name, &result, t);
                    results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(Outcome { name: job.repo.name.clone(), result });
                }
            });
        }
    });
    Ok(results.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner))
}

/// Bytes of every file the jobs' repos list now: what [`Content::Auto`] weighs, since a
/// repo's previous index (if any) may be far from what is about to be indexed. The
/// listings are dropped, not kept for the indexing that lists again: a fleet's listings
/// all held at once could cost a small box hundreds of MiB, and one more listing costs
/// about 1% of indexing chromium. A repo that cannot be listed fails the run: counting it
/// as nothing could pick the wrong codec for every repo the run writes.
fn listed_bytes(jobs: &[Job], threads: usize, ctx: &Arc<FsContext>) -> anyhow::Result<u64> {
    let mut total = 0;
    for job in jobs {
        let listing = FsSource::new(Path::new(&job.repo.root), threads, Arc::clone(ctx))
            .and_then(|s| s.list(None))
            .with_context(|| format!("listing {} to size it for --content auto", job.repo.name))?;
        total += listing.files.iter().map(|f| f.size).sum::<u64>();
    }
    Ok(total)
}

fn report(name: &str, result: &anyhow::Result<IndexReport>, started: Instant) {
    match result {
        Ok(rep) => {
            err_line(&format!(
                "  {:<40} {:>7} files {:>8.1} MiB {:>2} shard(s) {:>6.2}s{}",
                name,
                rep.index.files,
                rep.index.content_bytes as f64 / 1_048_576.0,
                rep.index.shards.len(),
                started.elapsed().as_secs_f64(),
                if rep.skipped.is_empty() { String::new() } else { format!("  ({} unreadable)", rep.skipped.len()) }
            ));
            for skipped in rep.skipped.iter().take(3) {
                err_line(&format!("  {name}: could not read {}: {}", skipped.path, skipped.error));
            }
        }
        Err(e) => err_line(&format!("  {name:<40} FAILED: {e:#}")),
    }
}

/// Records successful outcomes in `catalog` (retiring the shards they replace) and their
/// snapshots in the pack. Returns how many repos failed; a failed repo keeps its previous
/// index, which is still exactly what it was built from.
pub fn apply(catalog: &mut Catalog, pack: &mut SnapshotPack, outcomes: Vec<Outcome>) -> anyhow::Result<usize> {
    let now = now_unix()?;
    let mut failed = 0;
    for Outcome { name, result } in outcomes {
        let Ok(rep) = result else {
            // Already reported per repo as it happened.
            failed += 1;
            continue;
        };
        catalog.set_index(&name, rep.index, now).with_context(|| format!("repo {name:?} vanished from catalog"))?;
        // A source without snapshots leaves none behind, so a stale one is never used.
        match rep.snapshot {
            Some(bytes) => pack.entries.insert(name, bytes),
            None => pack.entries.remove(&name),
        };
    }
    Ok(failed)
}

/// Saves the catalog, then the pack, then deletes what expired. The catalog goes first:
/// once it is saved no new reader can pick up a retired shard.
pub fn commit(
    store: &dyn ShardStore,
    held: &WriterLock,
    mut catalog: Catalog,
    pack: &SnapshotPack,
) -> anyhow::Result<Catalog> {
    store.save_catalog(&catalog)?;
    pack.save(store)?;
    collect_garbage(store, held, &mut catalog, now_unix()?)?;
    Ok(catalog)
}

/// Retires shards that no catalog names (a run that crashed between publishing shards
/// and saving the catalog leaves those), and deletes retired shards past their grace.
fn collect_garbage(store: &dyn ShardStore, _held: &WriterLock, catalog: &mut Catalog, now: u64) -> anyhow::Result<()> {
    let known: std::collections::HashSet<&str> =
        catalog.live_shards().chain(catalog.retired.iter().map(|r| r.shard.as_str())).collect();
    let orphans: Vec<String> =
        store.list()?.into_iter().filter(|n| n != PACK_NAME && !known.contains(n.as_str())).collect();
    let expired = catalog.take_expired(now);
    if orphans.is_empty() && expired.is_empty() {
        return Ok(());
    }
    catalog.retired.extend(orphans.into_iter().map(|shard| scout_core::catalog::RetiredShard { shard, at_unix: now }));
    // Save before deleting: a crash in between leaves files the next sweep re-retires,
    // never a catalog naming deleted shards.
    store.save_catalog(catalog)?;
    for shard in &expired {
        store.remove(shard)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use scout_core::catalog::{RETIRE_GRACE_SECS, RepoEntry, RepoIndex};
    use scout_store_fs::FsStore;

    use super::*;

    fn publish(store: &FsStore, name: &str) {
        let staged = store.staging_path(name).unwrap();
        std::fs::write(&staged, b"x").unwrap();
        store.publish(&staged, name).unwrap();
    }

    fn index(shard: &str) -> RepoIndex {
        RepoIndex {
            backend: "native".into(),
            shards: vec![shard.into()],
            listing_digest: None,
            commit: None,
            files: 0,
            content_bytes: 0,
            binary_files: 0,
            indexed_at_unix: 0,
        }
    }

    #[test]
    fn auto_content_compresses_past_half_of_memory() {
        let gib = 1u64 << 30;
        assert_eq!(resolve_content(Content::Auto, 2 * gib, 4 * gib), ContentCodec::Raw);
        assert_eq!(resolve_content(Content::Auto, 2 * gib + 1, 4 * gib), ContentCodec::Lz4);
        // Explicit choices ignore the sizes.
        assert_eq!(resolve_content(Content::Raw, 100 * gib, gib), ContentCodec::Raw);
        assert_eq!(resolve_content(Content::Lz4, 0, 100 * gib), ContentCodec::Lz4);
    }

    /// Replaced shards stay on disk through the grace period and are deleted after it;
    /// a shard no catalog names is retired, then deleted the same way.
    #[test]
    fn retired_and_orphaned_shards_are_deleted_after_the_grace_period() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::open(dir.path()).unwrap();
        let held = store.lock_writer().unwrap();
        let mut catalog = Catalog::default();
        catalog.repos.push(RepoEntry { name: "r".into(), root: "/r".into(), remote: None, index: None });
        publish(&store, "r.1.0.native");
        catalog.set_index("r", index("r.1.0.native"), 1000).unwrap();
        publish(&store, "r.2.0.native");
        catalog.set_index("r", index("r.2.0.native"), 1000).unwrap();
        publish(&store, "orphan.1.0.native");
        let exists = |n: &str| store.local_path(n).unwrap().exists();

        collect_garbage(&store, &held, &mut catalog, 1000).unwrap();
        assert!(exists("r.1.0.native") && exists("orphan.1.0.native"), "inside the grace period");
        assert_eq!(catalog.retired.len(), 2);

        collect_garbage(&store, &held, &mut catalog, 1000 + RETIRE_GRACE_SECS).unwrap();
        assert!(!exists("r.1.0.native") && !exists("orphan.1.0.native"));
        assert!(exists("r.2.0.native"), "the live shard is never touched");
        assert!(catalog.retired.is_empty());
        assert!(store.load_catalog().unwrap().retired.is_empty(), "the catalog on disk agrees");
    }
}
