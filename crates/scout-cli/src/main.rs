//! `scout` CLI: the composition root wiring the filesystem source, the local store and
//! an index backend into scout-core.

mod host;
mod repos;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use scout_core::catalog::{Catalog, RepoEntry};
use scout_core::indexer::{IndexOptions, index_repo};
use scout_core::ports::{IndexBackend, ShardStore};
use scout_core::search::{CaseMode, Freshness, OutputMode, RepoHandle, SearchOptions, search};
use scout_core::snapshots::SnapshotPack;
use scout_index_native::NativeBackend;
use scout_index_sqlite::SqliteBackend;
use scout_source_fs::{ChangeDetection, FsContext, FsSource};
use scout_store_fs::FsStore;

#[derive(Parser)]
#[command(name = "scout", version, about = "Index many repos once, grep them all in milliseconds.")]
struct Cli {
    /// Where the catalog and shards live. Defaults to `$SCOUT_INDEX_DIR`, else the
    /// platform data directory.
    #[arg(long, global = true)]
    index_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register repos to index.
    Add {
        paths: Vec<PathBuf>,
        /// Treat each path as a directory to search for git repos.
        #[arg(long)]
        discover: bool,
        #[arg(long, default_value_t = 4)]
        max_depth: usize,
    },
    /// Unregister repos and delete their shards.
    Remove { names: Vec<String> },
    /// List registered repos.
    List,
    /// Build (or rebuild) the index for all repos, or the named ones.
    Index {
        names: Vec<String>,
        #[arg(long, value_enum, default_value_t = Backend::Native)]
        backend: Backend,
        #[arg(long)]
        threads: Option<usize>,
        /// Indexing memory budget in MiB. Default: a quarter of physical RAM, capped at 4 GiB.
        #[arg(long)]
        memory_mb: Option<u64>,
    },
    /// Search every indexed repo (rg-compatible output).
    Search(SearchArgs),
}

#[derive(clap::Args)]
#[allow(clippy::struct_excessive_bools)] // Mirrors rg's independent boolean flags.
struct SearchArgs {
    pattern: String,
    #[arg(short = 'i', long)]
    ignore_case: bool,
    #[arg(short = 'S', long)]
    smart_case: bool,
    #[arg(short = 'F', long)]
    fixed_strings: bool,
    #[arg(short = 'w', long)]
    word_regexp: bool,
    #[arg(short = 'n', long)]
    line_number: bool,
    #[arg(short = 'l', long)]
    files_with_matches: bool,
    #[arg(short = 'c', long)]
    count: bool,
    #[arg(short = 'm', long)]
    max_count: Option<u64>,
    /// Only search these repos (by name). Repeatable.
    #[arg(long = "repo")]
    repos: Vec<String>,
    #[arg(long)]
    threads: Option<usize>,
    #[arg(long, value_enum, default_value_t = FreshnessArg::Strict)]
    freshness: FreshnessArg,
    /// How strict freshness finds what changed: the volume's change journal where there
    /// is one, or re-listing every indexed directory.
    #[arg(long, value_enum, default_value_t = DetectionArg::Journal)]
    change_detection: DetectionArg,
    /// Print timing and filter statistics to stderr.
    #[arg(long)]
    stats: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Backend {
    Native,
    Sqlite,
}

#[derive(Clone, Copy, ValueEnum)]
enum FreshnessArg {
    Strict,
    Off,
}

#[derive(Clone, Copy, ValueEnum)]
enum DetectionArg {
    Journal,
    Scan,
}

fn backend_by_name(name: &str) -> anyhow::Result<&'static dyn IndexBackend> {
    match name {
        "native" => Ok(&NativeBackend),
        "sqlite" => Ok(&SqliteBackend),
        other => bail!("unknown index backend {other:?} in catalog"),
    }
}

fn index_dir(flag: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(d) = flag {
        return Ok(d);
    }
    if let Some(d) = std::env::var_os("SCOUT_INDEX_DIR") {
        return Ok(PathBuf::from(d));
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    };
    Ok(base.context("cannot determine a data directory; pass --index-dir or set SCOUT_INDEX_DIR")?.join("scout"))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "scout: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let store = FsStore::open(&index_dir(cli.index_dir)?)?;
    match cli.command {
        Command::Add { paths, discover, max_depth } => cmd_add(&store, &paths, discover, max_depth),
        Command::Remove { names } => cmd_remove(&store, &names),
        Command::List => cmd_list(&store),
        Command::Index { names, backend, threads, memory_mb } => cmd_index(&store, &names, backend, threads, memory_mb),
        Command::Search(args) => cmd_search(&store, &args),
    }
}

fn err_line(msg: &str) {
    let _ = writeln!(std::io::stderr(), "{msg}");
}

fn cmd_add(store: &FsStore, paths: &[PathBuf], discover: bool, max_depth: usize) -> anyhow::Result<ExitCode> {
    if paths.is_empty() {
        bail!("no paths given");
    }
    let mut catalog = store.load_catalog()?;
    let mut roots = Vec::new();
    for p in paths {
        if discover {
            roots.extend(repos::discover(p, max_depth)?);
        } else {
            roots.push(p.clone());
        }
    }
    let mut added = 0;
    for root in &roots {
        match repos::register(&mut catalog, root)? {
            Some(name) => {
                added += 1;
                err_line(&format!("added {name}  {}", root.display()));
            }
            None => err_line(&format!("skipped (already covered) {}", root.display())),
        }
    }
    store.save_catalog(&catalog)?;
    err_line(&format!("{added} repo(s) added, {} registered", catalog.repos.len()));
    Ok(ExitCode::SUCCESS)
}

fn cmd_remove(store: &FsStore, names: &[String]) -> anyhow::Result<ExitCode> {
    let mut catalog = store.load_catalog()?;
    for name in names {
        let Some(pos) = catalog.repos.iter().position(|r| &r.name == name) else { bail!("no repo named {name:?}") };
        let entry = catalog.repos.remove(pos);
        if let Some(idx) = &entry.index {
            for name in &idx.shards {
                store.remove(name)?;
            }
        }
    }
    store.save_catalog(&catalog)?;
    let mut pack = SnapshotPack::load(store)?;
    let before = pack.entries.len();
    pack.entries.retain(|name, _| !names.contains(name));
    if pack.entries.len() != before {
        pack.save(store)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_list(store: &FsStore) -> anyhow::Result<ExitCode> {
    let catalog = store.load_catalog()?;
    let mut out = std::io::stdout().lock();
    for r in &catalog.repos {
        match &r.index {
            Some(i) => writeln!(
                out,
                "{}\t{}\t{} files\t{:.1} MiB\t{} shard(s)\t{}",
                r.name,
                r.root,
                i.files,
                i.content_bytes as f64 / 1_048_576.0,
                i.shards.len(),
                i.backend
            )?,
            None => writeln!(out, "{}\t{}\t(not indexed)", r.name, r.root)?,
        }
    }
    Ok(ExitCode::SUCCESS)
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

fn cmd_index(
    store: &FsStore,
    names: &[String],
    backend: Backend,
    threads: Option<usize>,
    memory_mb: Option<u64>,
) -> anyhow::Result<ExitCode> {
    let backend: &dyn IndexBackend = match backend {
        Backend::Native => &NativeBackend,
        Backend::Sqlite => &SqliteBackend,
    };
    let catalog = store.load_catalog()?;
    let selected: Vec<_> = if names.is_empty() {
        catalog.repos.clone()
    } else {
        names
            .iter()
            .map(|n| catalog.find(n).cloned().with_context(|| format!("no repo named {n:?}")))
            .collect::<anyhow::Result<_>>()?
    };
    if selected.is_empty() {
        bail!("no repos registered; run `scout add` first");
    }
    let threads = threads.unwrap_or_else(host::cores);
    let budget = match memory_mb {
        Some(mb) => mb << 20,
        None => (host::physical_memory()? / 4).min(4 << 30),
    };
    let (workers, cap) = plan_indexing(threads, budget, selected.len());
    let walk_threads = (threads / workers).max(1);
    err_line(&format!(
        "indexing {} repo(s): {workers} worker(s), shard cap {} MiB, budget {} MiB, backend {}",
        selected.len(),
        cap >> 20,
        budget >> 20,
        backend.name()
    ));

    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let ctx = Arc::new(FsContext::new(ChangeDetection::Journal));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(repo) = selected.get(i) else { break };
                    let t = Instant::now();
                    let r = FsSource::new(Path::new(&repo.root), walk_threads, Arc::clone(&ctx)).and_then(|src| {
                        index_repo(&repo.name, &src, backend, store, IndexOptions { shard_content_cap: cap })
                    });
                    match &r {
                        Ok(rep) => err_line(&format!(
                            "  {:<40} {:>7} files {:>8.1} MiB {:>2} shard(s) {:>6.2}s{}",
                            repo.name,
                            rep.index.files,
                            rep.index.content_bytes as f64 / 1_048_576.0,
                            rep.index.shards.len(),
                            t.elapsed().as_secs_f64(),
                            if rep.skipped.is_empty() {
                                String::new()
                            } else {
                                format!("  ({} unreadable)", rep.skipped.len())
                            }
                        )),
                        Err(e) => err_line(&format!("  {:<40} FAILED: {e}", repo.name)),
                    }
                    results.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((repo.name.clone(), r));
                }
            });
        }
    });

    let mut catalog = store.load_catalog()?;
    let mut pack = SnapshotPack::load(store)?;
    let mut failed = 0;
    for (name, r) in results.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner) {
        let Ok(rep) = r else {
            // Already reported per repo as it happened.
            failed += 1;
            continue;
        };
        for skipped in rep.skipped.iter().take(3) {
            err_line(&format!("  {name}: could not read {}: {}", skipped.path, skipped.error));
        }
        let entry = catalog.find_mut(&name).with_context(|| format!("repo {name:?} vanished from catalog"))?;
        // Shard names are deterministic, so republishing replaced same-named shards;
        // only shards beyond the new count (or from another backend) are stale.
        for old in entry.index.iter().flat_map(|i| &i.shards) {
            if !rep.index.shards.contains(old) {
                store.remove(old)?;
            }
        }
        entry.index = Some(rep.index);
        // A source without snapshots leaves none behind, so a stale one is never used.
        match rep.snapshot {
            Some(bytes) => pack.entries.insert(name, bytes),
            None => pack.entries.remove(&name),
        };
    }
    store.save_catalog(&catalog)?;
    pack.save(store)?;
    err_line(&format!("done in {:.1}s", started.elapsed().as_secs_f64()));
    if failed > 0 {
        bail!("{failed} repo(s) failed to index");
    }
    Ok(ExitCode::SUCCESS)
}

/// `with_snapshots`: only strict freshness reads snapshots, so other modes skip the file
/// reads (about 2.5 MB across the bench corpus).
fn open_repos(
    store: &FsStore,
    catalog: &Catalog,
    only: &[String],
    walk_threads: usize,
    with_snapshots: bool,
    ctx: &Arc<FsContext>,
) -> anyhow::Result<Vec<RepoHandle>> {
    for n in only {
        if catalog.find(n).is_none() {
            bail!("no repo named {n:?}");
        }
    }
    let selected: Vec<_> = catalog.repos.iter().filter(|r| only.is_empty() || only.contains(&r.name)).collect();
    let unindexed: Vec<&str> = selected.iter().filter(|r| r.index.is_none()).map(|r| r.name.as_str()).collect();
    if !unindexed.is_empty() {
        bail!("not indexed yet (run `scout index`): {}", unindexed.join(", "));
    }
    let open_one = |r: &RepoEntry| -> anyhow::Result<RepoHandle> {
        let idx = r.index.as_ref().context("checked above")?;
        let backend = backend_by_name(&idx.backend)?;
        let shards = idx
            .shards
            .iter()
            .map(|s| Ok(backend.open(&store.local_path(s)?)?))
            .collect::<anyhow::Result<Vec<_>>>()
            .with_context(|| format!("opening shards of {}", r.name))?;
        let source = FsSource::for_search(r.root.clone(), walk_threads, Arc::clone(ctx))
            .with_context(|| format!("repo {}", r.name))?;
        Ok(RepoHandle {
            name: r.name.clone(),
            source: Box::new(source),
            shards,
            snapshot: None,
            listing_digest: idx.listing_digest,
        })
    };
    // Each open is a few file operations at tens of microseconds each on Windows; across
    // 100+ repos that is tens of milliseconds serially, so spread it over the threads.
    // Chunks keep catalog order, which the reorder buffer relies on for stable output.
    let chunk = selected.len().div_ceil(walk_threads.max(1)).max(1);
    std::thread::scope(|s| {
        // One read for every repo's snapshot (see scout_core::snapshots for why one file),
        // overlapped with the shard opens: it is a 2 MiB read plus a decode, ~2 ms that
        // would otherwise run before any open starts.
        let pack = with_snapshots.then(|| s.spawn(|| SnapshotPack::load(store)));
        let workers: Vec<_> = selected
            .chunks(chunk)
            .map(|part| s.spawn(move || part.iter().map(|r| open_one(r)).collect::<anyhow::Result<Vec<_>>>()))
            .collect();
        let mut handles = Vec::with_capacity(selected.len());
        for w in workers {
            handles.extend(w.join().map_err(|_| anyhow::anyhow!("repo open thread panicked"))??);
        }
        if let Some(pack) = pack {
            let mut pack = pack.join().map_err(|_| anyhow::anyhow!("snapshot load thread panicked"))??;
            // Moved out, not copied: the pack is not needed after this.
            for h in &mut handles {
                h.snapshot = pack.entries.remove(&h.name);
            }
        }
        Ok(handles)
    })
}

/// Stores the snapshots a strict search refreshed, so the next search starts from them.
/// `selected` is in the same order as the repos passed to `search`.
///
/// The pack is re-read rather than reusing the copy read at open, so that entries another
/// process persisted meanwhile (for repos this search did not change) are kept.
fn persist_snapshots(store: &FsStore, selected: &[&RepoEntry], snapshots: Vec<(usize, Vec<u8>)>) -> anyhow::Result<()> {
    if snapshots.is_empty() {
        return Ok(());
    }
    let mut pack = SnapshotPack::load(store)?;
    for (i, bytes) in snapshots {
        let repo = selected.get(i).context("snapshot for a repo that was not searched")?;
        pack.entries.insert(repo.name.clone(), bytes);
    }
    pack.save(store)?;
    Ok(())
}

fn cmd_search(store: &FsStore, a: &SearchArgs) -> anyhow::Result<ExitCode> {
    let started = Instant::now();
    let threads = a.threads.unwrap_or_else(host::cores);
    let catalog = store.load_catalog()?;
    let ctx = Arc::new(FsContext::new(match a.change_detection {
        DetectionArg::Journal => ChangeDetection::Journal,
        DetectionArg::Scan => ChangeDetection::Scan,
    }));
    let repos = open_repos(store, &catalog, &a.repos, threads, matches!(a.freshness, FreshnessArg::Strict), &ctx)?;
    let opened = started.elapsed();
    let opts = SearchOptions {
        pattern: a.pattern.clone(),
        fixed_strings: a.fixed_strings,
        case: if a.ignore_case {
            CaseMode::Insensitive
        } else if a.smart_case {
            CaseMode::Smart
        } else {
            CaseMode::Sensitive
        },
        word: a.word_regexp,
        line_number: a.line_number,
        output: if a.files_with_matches {
            OutputMode::FilesWithMatches
        } else if a.count {
            OutputMode::Count
        } else {
            OutputMode::Lines
        },
        max_count: a.max_count,
        threads,
        freshness: match a.freshness {
            FreshnessArg::Strict => Freshness::Strict,
            FreshnessArg::Off => Freshness::Off,
        },
    };
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::with_capacity(1 << 16, stdout.lock());
    let report = match search(&opts, &repos, &mut out) {
        Ok(s) => s,
        // `scout ... | head` closing the pipe is a normal way to stop a search.
        Err(scout_core::Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::BrokenPipe => {
            return Ok(ExitCode::SUCCESS);
        }
        Err(e) => return Err(e.into()),
    };
    match out.flush() {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(ExitCode::SUCCESS),
        r => r?,
    }
    let stats = report.stats;
    let selected: Vec<&RepoEntry> =
        catalog.repos.iter().filter(|r| a.repos.is_empty() || a.repos.contains(&r.name)).collect();
    persist_snapshots(store, &selected, report.snapshots)?;
    if a.stats {
        err_line(&format!(
            "scout-stats repos={} shards={} docs={} candidates={} matched_files={} dirty_files={} full_scans={} unchanged_repos={} list_ms={:.2} map_ms={:.2} open_ms={:.2} search_ms={:.2} total_ms={:.2} plan={}",
            stats.repos,
            stats.shards,
            stats.docs,
            stats.candidates,
            stats.matched_files,
            stats.dirty_files,
            stats.full_scans,
            stats.unchanged_repos,
            stats.list_time.as_secs_f64() * 1e3,
            stats.map_time.as_secs_f64() * 1e3,
            opened.as_secs_f64() * 1e3,
            stats.elapsed.as_secs_f64() * 1e3,
            started.elapsed().as_secs_f64() * 1e3,
            stats.plan
        ));
    }
    release_repos(repos, threads);
    Ok(if stats.matched_files > 0 { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

/// Drops the opened repos across `threads` threads.
///
/// Unmapping shard pages the search touched costs ~0.17 ms per MiB on Windows, and the
/// kernel does it serially if left to process exit (measured: leaking the maps saves
/// nothing). Unmapping from several threads at once overlaps that work: `literal_rare`
/// went from 21 to 7 ms of teardown, `-F function` from 184 to 138 ms wall.
fn release_repos(mut repos: Vec<RepoHandle>, threads: usize) {
    let chunk = repos.len().div_ceil(threads.max(1)).max(1);
    std::thread::scope(|s| {
        while !repos.is_empty() {
            let part: Vec<RepoHandle> = repos.drain(..chunk.min(repos.len())).collect();
            s.spawn(move || drop(part));
        }
    });
}
