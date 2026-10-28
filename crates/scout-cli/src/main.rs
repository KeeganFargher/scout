//! `scout` CLI: the composition root wiring the filesystem source, the local store and
//! an index backend into scout-core.

mod host;
mod ingest;
mod repos;
mod serve;
mod sync;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use scout_core::catalog::{Catalog, RepoEntry};
use scout_core::ports::{IndexBackend, ShardStore};
use scout_core::rank::Weights;
use scout_core::ranked::{RankOptions, search_ranked};
use scout_core::search::{
    BoundedOptions, CaseMode, Freshness, Limits, OutputMode, RepoHandle, SearchOptions, SearchReport, search,
    search_bounded,
};
use scout_core::snapshots::SnapshotPack;
use scout_index_native::{ContentCodec, FilterHome, NativeBackend};
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
        /// How the native backend stores file content: as is, or lz4 compressed, which takes
        /// ~3x less disk and page cache for a decode on every read (E29). `auto`: lz4 once
        /// the content of every registered repo would take more than half of this machine's
        /// usable memory, decided for the repos this run writes. Pass `lz4` to build an
        /// index here for a smaller machine.
        #[arg(long, value_enum, default_value_t = ContentArg::Auto)]
        content: ContentArg,
        #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        threads: Option<usize>,
        /// Indexing memory budget in MiB. Default: a quarter of usable RAM (physical RAM, capped
        /// by the cgroup memory limit on Linux), capped at 4 GiB. Usable RAM is read on Windows
        /// and Linux only; elsewhere (macOS, BSD) pass this.
        #[arg(long)]
        memory_mb: Option<u64>,
    },
    /// Search every indexed repo (rg-compatible output).
    Search(SearchArgs),
    /// Run one sync cycle: mirror the configured remotes, reindex what moved, publish.
    Sync {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        threads: Option<usize>,
        /// Indexing memory budget in MiB (default as for `index`).
        #[arg(long)]
        memory_mb: Option<u64>,
        /// How reindexed repos store file content (as for `index`; native only).
        #[arg(long, value_enum, default_value_t = ContentArg::Auto)]
        content: ContentArg,
    },
    /// Serve searches over HTTP while syncing every `interval_secs` of the config.
    Serve {
        #[arg(long)]
        config: PathBuf,
        /// Address to bind; port 0 picks a free one (printed on startup).
        #[arg(long, default_value = "127.0.0.1:7700")]
        listen: String,
        /// Threads per search, and for indexing. Default: the cores.
        #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        threads: Option<usize>,
        /// Concurrent searches. Default: a quarter of the cores, 1 to 8.
        #[arg(long)]
        http_workers: Option<usize>,
        /// Largest search response in MiB; bigger searches get 422 and must be narrowed.
        #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u64).range(1..))]
        max_output_mb: u64,
        /// Longest one search may run, in seconds; past it the answer is 503.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
        search_timeout_secs: u64,
        #[arg(long)]
        memory_mb: Option<u64>,
        /// How reindexed repos store file content (as for `index`; native only).
        #[arg(long, value_enum, default_value_t = ContentArg::Auto)]
        content: ContentArg,
    },
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
    /// Return at most this many matching lines globally, with completion metadata on
    /// stderr. Requires --freshness off; stops before planning later shards.
    #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..), conflicts_with_all = ["rank", "files_with_matches"])]
    max_results: Option<usize>,
    /// With --max-results: cap complete formatted output records at this many bytes.
    #[arg(long, default_value_t = 65_536, requires = "max_results", value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
    result_bytes: usize,
    /// With --max-results: cooperative processing budget in milliseconds.
    #[arg(long, default_value_t = 1000, requires = "max_results", value_parser = clap::value_parser!(u64).range(1..))]
    budget_ms: u64,
    /// Only search these repos (by name). Repeatable.
    #[arg(long = "repo")]
    repos: Vec<String>,
    #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
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
    /// Print the best matching files first instead of every match in walk order
    /// (docs/ranking.md). The files are rg's; only their order and number change.
    #[arg(long)]
    rank: bool,
    /// With --rank: how many files to print.
    #[arg(long, default_value_t = 20, requires = "rank", value_parser = clap::value_parser!(u64).range(1..))]
    top: u64,
    /// With --rank: also write every matching file's ranking features as JSON here, for
    /// the evaluation harness (bench/golden).
    #[arg(long, requires = "rank")]
    rank_features: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Backend {
    Native,
    Sqlite,
}

#[derive(Clone, Copy, ValueEnum)]
enum ContentArg {
    Auto,
    Raw,
    Lz4,
}

impl From<ContentArg> for ingest::Content {
    fn from(arg: ContentArg) -> Self {
        match arg {
            ContentArg::Auto => Self::Auto,
            ContentArg::Raw => Self::Raw,
            ContentArg::Lz4 => Self::Lz4,
        }
    }
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

/// The native backend for a process per query: its shards probe their trigram filters in
/// place (E24). Only for reading, where the codec plays no part: an index run builds its
/// own backend with the codec it picks (`ingest::Content`).
static NATIVE: NativeBackend = NativeBackend::new(FilterHome::Map, ContentCodec::Raw);
/// The native backend for `serve`, which keeps every shard's filter on the heap (E24).
static NATIVE_SERVER: NativeBackend = NativeBackend::new(FilterHome::Heap, ContentCodec::Raw);

/// `filter` says where native shards keep their trigram filters: [`FilterHome::Map`] for a
/// process per query, [`FilterHome::Heap`] for `serve`.
fn backend_by_name(name: &str, filter: FilterHome) -> anyhow::Result<&'static dyn IndexBackend> {
    match name {
        "native" => Ok(match filter {
            FilterHome::Map => &NATIVE,
            FilterHome::Heap => &NATIVE_SERVER,
        }),
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
        Command::Index { names, backend, content, threads, memory_mb } => {
            cmd_index(&store, &names, backend, content, threads, memory_mb)
        }
        Command::Search(args) => cmd_search(&store, &args),
        Command::Sync { config, threads, memory_mb, content } => {
            cmd_sync(&store, &config, threads, memory_mb, content.into())
        }
        Command::Serve {
            config,
            listen,
            threads,
            http_workers,
            max_output_mb,
            search_timeout_secs,
            memory_mb,
            content,
        } => {
            let budget = ingest::Budget::for_host(threads, memory_mb)?;
            let opts = serve::ServeOptions {
                listen,
                threads: budget.threads,
                http_workers: http_workers.unwrap_or_else(serve::default_http_workers),
                max_output_bytes: usize::try_from(max_output_mb)?
                    .checked_mul(1 << 20)
                    .context("--max-output-mb is too large")?,
                search_timeout: std::time::Duration::from_secs(search_timeout_secs),
                budget,
                content: content.into(),
            };
            serve::serve(&store, &scout_sync::config::Config::load(&config)?, &opts)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn err_line(msg: &str) {
    let _ = writeln!(std::io::stderr(), "{msg}");
}

fn cmd_add(store: &FsStore, paths: &[PathBuf], discover: bool, max_depth: usize) -> anyhow::Result<ExitCode> {
    if paths.is_empty() {
        bail!("no paths given");
    }
    let _held = store.lock_writer()?;
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
    let held = store.lock_writer()?;
    let mut catalog = store.load_catalog()?;
    let now = scout_core::catalog::now_unix()?;
    for name in names {
        catalog.remove_repo(name, now)?;
    }
    let mut pack = SnapshotPack::load(store)?;
    pack.entries.retain(|name, _| !names.contains(name));
    ingest::commit(store, &held, catalog, &pack)?;
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

fn cmd_sync(
    store: &FsStore,
    config: &Path,
    threads: Option<usize>,
    memory_mb: Option<u64>,
    content: ingest::Content,
) -> anyhow::Result<ExitCode> {
    let cfg = scout_sync::config::Config::load(config)?;
    let token = scout_sync::Token::from_env(&cfg.token_env)?;
    let git = cfg.git(token.clone())?;
    let budget = ingest::Budget::for_host(threads, memory_mb)?;
    let report = sync::run_cycle(store, &cfg, &git, token.as_ref(), budget, content)?;
    sync::log_report(&report);
    if !report.failures.is_empty() {
        bail!("{} failure(s) in this cycle", report.failures.len());
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_index(
    store: &FsStore,
    names: &[String],
    backend: Backend,
    content: ContentArg,
    threads: Option<usize>,
    memory_mb: Option<u64>,
) -> anyhow::Result<ExitCode> {
    let format = match (backend, content) {
        (Backend::Native, content) => ingest::Format::Native(content.into()),
        (Backend::Sqlite, ContentArg::Auto | ContentArg::Raw) => ingest::Format::Sqlite,
        (Backend::Sqlite, ContentArg::Lz4) => {
            bail!("--content lz4 is a native-backend format; sqlite stores content as is")
        }
    };
    let held = store.lock_writer()?;
    let catalog = store.load_catalog()?;
    let selected: Vec<RepoEntry> = if names.is_empty() {
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
    let started = Instant::now();
    // A synced repo indexed by hand is still at the commit sync left it at; keeping the
    // commit lets the next sync cycle skip it.
    let jobs: Vec<ingest::Job> = selected
        .into_iter()
        .map(|repo| {
            let commit = repo.index.as_ref().and_then(|i| i.commit.clone()).filter(|_| repo.remote.is_some());
            ingest::Job { repo, commit }
        })
        .collect();
    let other_content = catalog
        .repos
        .iter()
        .filter(|r| !jobs.iter().any(|j| j.repo.name == r.name))
        .filter_map(|r| r.index.as_ref())
        .map(|i| i.content_bytes)
        .sum();
    let budget = ingest::Budget::for_host(threads, memory_mb)?;
    let outcomes = ingest::index_repos(store, &held, &jobs, format, other_content, budget)?;
    let mut catalog = catalog;
    let mut pack = SnapshotPack::load(store)?;
    let failed = ingest::apply(&mut catalog, &mut pack, outcomes)?;
    ingest::commit(store, &held, catalog, &pack)?;
    err_line(&format!("done in {:.1}s", started.elapsed().as_secs_f64()));
    if failed > 0 {
        bail!("{failed} repo(s) failed to index");
    }
    Ok(ExitCode::SUCCESS)
}

/// `with_snapshots`: only strict freshness reads snapshots, so other modes skip the file
/// reads (about 2.5 MB across the bench corpus). `filter` is where native shards keep their
/// trigram filters (see [`backend_by_name`]).
fn open_repos(
    store: &FsStore,
    catalog: &Catalog,
    only: &[String],
    walk_threads: usize,
    with_snapshots: bool,
    ctx: &Arc<FsContext>,
    filter: FilterHome,
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
        let backend = backend_by_name(&idx.backend, filter)?;
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
/// Done under the writer lock against the catalog as it is now, not as the search saw it:
/// a repo removed (or re-added at another root) while the search ran must not get its old
/// working copy's snapshot back, or a later repo of that name would start from a listing
/// of the wrong tree. The lock is only tried: when a writer holds it, persisting is
/// skipped. The pack is a cache, so the cost is the next search replaying more of the
/// journal, where waiting could block a search behind a whole sync cycle.
fn persist_snapshots(store: &FsStore, selected: &[&RepoEntry], snapshots: Vec<(usize, Vec<u8>)>) -> anyhow::Result<()> {
    if snapshots.is_empty() {
        return Ok(());
    }
    let Some(_held) = store.try_lock_writer()? else {
        return Ok(());
    };
    let catalog = store.load_catalog()?;
    let mut pack = SnapshotPack::load(store)?;
    for (i, bytes) in snapshots {
        let searched = selected.get(i).context("snapshot for a repo that was not searched")?;
        if catalog.find(&searched.name).is_some_and(|now| now.root == searched.root) {
            pack.entries.insert(searched.name.clone(), bytes);
        }
    }
    pack.entries.retain(|name, _| catalog.find(name).is_some());
    pack.save(store)?;
    Ok(())
}

/// The evaluation harness's input: everything a ranked search scored, so it can re-score
/// under other weights without searching again.
fn write_rank_features(path: &Path, r: &scout_core::ranked::RankedReport) -> anyhow::Result<()> {
    #[derive(serde::Serialize)]
    struct Dump<'a> {
        terms: &'a [String],
        corpus: &'a scout_core::rank::Corpus,
        files: &'a [scout_core::rank::FileFeatures],
    }
    let json = serde_json::to_vec(&Dump { terms: &r.terms, corpus: &r.corpus, files: &r.files })?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))
}

fn cmd_search(store: &FsStore, a: &SearchArgs) -> anyhow::Result<ExitCode> {
    if a.max_results.is_some() && !matches!(a.freshness, FreshnessArg::Off) {
        bail!("--max-results searches the published index; use --freshness off");
    }
    let started = Instant::now();
    let threads = a.threads.unwrap_or_else(host::cores);
    let catalog = store.load_catalog()?;
    let ctx = Arc::new(FsContext::new(match a.change_detection {
        DetectionArg::Journal => ChangeDetection::Journal,
        DetectionArg::Scan => ChangeDetection::Scan,
    }));
    let repos = open_repos(
        store,
        &catalog,
        &a.repos,
        threads,
        matches!(a.freshness, FreshnessArg::Strict),
        &ctx,
        FilterHome::Map,
    )?;
    let opened = started.elapsed();
    let opts = search_options(a, threads);
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::with_capacity(1 << 16, stdout.lock());
    let handles: Vec<&RepoHandle> = repos.iter().collect();
    let searched = if let Some(max_matches) = a.max_results {
        let bounds =
            BoundedOptions { max_matches, max_bytes: a.result_bytes, timeout: Duration::from_millis(a.budget_ms) };
        run_bounded(&opts, bounds, &handles, &mut out)
    } else if a.rank {
        let rank = RankOptions { top: usize::try_from(a.top)?, weights: Weights::default() };
        search_ranked(&opts, &rank, &handles, &mut out).and_then(|r| {
            if let Some(path) = &a.rank_features {
                write_rank_features(path, &r).map_err(|e| scout_core::Error::Invalid(format!("{e:#}")))?;
            }
            Ok(SearchReport { stats: r.stats, snapshots: r.snapshots })
        })
    } else {
        search(&opts, &handles, &mut out)
    };
    let report = match searched {
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
            "scout-stats repos={} shards={} shards_filtered={} docs={} candidates={} matched_files={} dirty_files={} full_scans={} unchanged_repos={} list_ms={:.2} map_ms={:.2} postings_ms={:.2} verify_ms={:.2} open_ms={:.2} search_ms={:.2} total_ms={:.2} plan={}",
            stats.repos,
            stats.shards,
            stats.shards_filtered,
            stats.docs,
            stats.candidates,
            stats.matched_files,
            stats.dirty_files,
            stats.full_scans,
            stats.unchanged_repos,
            stats.list_time.as_secs_f64() * 1e3,
            stats.map_time.as_secs_f64() * 1e3,
            stats.postings_time.as_secs_f64() * 1e3,
            stats.verify_time.as_secs_f64() * 1e3,
            opened.as_secs_f64() * 1e3,
            stats.elapsed.as_secs_f64() * 1e3,
            started.elapsed().as_secs_f64() * 1e3,
            stats.plan
        ));
    }
    release_repos(repos, threads);
    Ok(if a.max_results.is_some() || stats.matched_files > 0 { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

fn search_options(a: &SearchArgs, threads: usize) -> SearchOptions {
    SearchOptions {
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
        // Unbounded: the CLI streams to whoever runs it.
        limits: Limits::default(),
        freshness: match a.freshness {
            FreshnessArg::Strict => Freshness::Strict,
            FreshnessArg::Off => Freshness::Off,
        },
    }
}

fn run_bounded(
    opts: &SearchOptions,
    bounds: BoundedOptions,
    repos: &[&RepoHandle],
    out: &mut dyn Write,
) -> scout_core::Result<SearchReport> {
    let r = search_bounded(opts, bounds, repos, out)?;
    let metadata = if opts.output == OutputMode::Count {
        serde_json::json!({"count": r.count_estimate(), "complete": r.complete(), "stop_reason": r.stop})
    } else {
        serde_json::json!({"complete": r.complete(), "stop_reason": r.stop,
            "returned_matches": r.returned_matches, "elapsed_ms": r.stats.elapsed.as_secs_f64() * 1000.0})
    };
    if opts.output == OutputMode::Count {
        writeln!(out, "{metadata}").map_err(|e| scout_core::Error::io("<stdout>", e))?;
    } else {
        err_line(&format!("scout-bounded {metadata}"));
    }
    Ok(SearchReport { stats: r.stats, snapshots: Vec::new() })
}

/// Drops the opened repos' shards across `threads` threads.
///
/// Unmapping shard pages the search touched costs ~0.17 ms per MiB on Windows, and the
/// kernel does it serially if left to process exit (measured: leaking the maps saves
/// nothing). Unmapping from several threads at once overlaps that work: `literal_rare`
/// went from 21 to 7 ms of teardown, `-F function` from 184 to 138 ms wall (E6). The unit is
/// a shard, taken from a shared queue, not a repo: with few large repos, per-repo release
/// left one thread unmapping chromium's ~3 GiB, ~460 ms after a full scan (E26). The rest of
/// a repo handle (name, source, snapshot) is small heap memory and drops right here.
fn release_repos(repos: Vec<RepoHandle>, threads: usize) {
    let shards: Vec<_> = repos.into_iter().flat_map(|r| r.shards).collect();
    let workers = threads.min(shards.len());
    let queue = Mutex::new(shards);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    // Pop under the lock, drop outside it, so unmaps run concurrently.
                    let next = queue.lock().unwrap_or_else(PoisonError::into_inner).pop();
                    let Some(shard) = next else { break };
                    drop(shard);
                }
            });
        }
    });
}
