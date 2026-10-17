//! scout-bench: times `scout search` against plain `rg` over the same repo roots and
//! checks that both return exactly the same matches.
//!
//! Every query is a separate process for both tools, which is how a user (or an agent)
//! actually invokes them, so process start-up and index opening are part of the cost.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Parser;
use scout_core::ports::ShardStore;
use scout_store_fs::FsStore;
use serde::Serialize;

#[derive(Parser)]
struct Args {
    /// Path to the scout binary under test.
    #[arg(long)]
    scout: PathBuf,
    #[arg(long)]
    index_dir: PathBuf,
    /// TSV: name<TAB>flags<TAB>pattern. Lines starting with # are comments.
    #[arg(long)]
    queries: PathBuf,
    #[arg(long, default_value_t = 5)]
    runs: usize,
    #[arg(long, default_value_t = 2)]
    rg_runs: usize,
    #[arg(long, default_value = "rg")]
    rg: String,
    /// Extra args passed to every scout search (e.g. "--freshness off").
    #[arg(long, default_value = "")]
    scout_flags: String,
    /// Label recorded with the results (experiment name).
    #[arg(long, default_value = "run")]
    label: String,
    /// Write machine-readable results here.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Only run queries whose name contains this substring.
    #[arg(long)]
    filter: Option<String>,
}

struct Query {
    name: String,
    flags: Vec<String>,
    pattern: String,
}

#[derive(Serialize)]
struct QueryResult {
    name: String,
    scout_median_ms: f64,
    scout_min_ms: f64,
    /// Tail latencies; `None` below [`TAIL_MIN_RUNS`] runs, where they would only be noise.
    scout_p90_ms: Option<f64>,
    scout_p99_ms: Option<f64>,
    rg_median_ms: f64,
    speedup: f64,
    candidates: u64,
    matched_files: u64,
    results: usize,
    parity: bool,
    missing_in_scout: usize,
    extra_in_scout: usize,
    /// Includes touched mmap pages, which are shared and evictable.
    scout_peak_ws_mb: Option<f64>,
    /// Private commit: the memory the process really needs. This is the number that must
    /// fit a 2 GB machine.
    scout_peak_private_mb: Option<f64>,
}

#[derive(Serialize)]
struct Report {
    label: String,
    repos: usize,
    indexed_files: u64,
    content_mb: f64,
    index_mb: f64,
    geomean_speedup: f64,
    total_scout_ms: f64,
    total_rg_ms: f64,
    all_parity: bool,
    queries: Vec<QueryResult>,
}

fn parse_queries(path: &Path) -> anyhow::Result<Vec<Query>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        let [name, flags, pattern] = parts.as_slice() else {
            bail!("{}:{}: expected name<TAB>flags<TAB>pattern", path.display(), i + 1)
        };
        out.push(Query {
            name: (*name).to_string(),
            flags: flags.split_whitespace().map(str::to_string).collect(),
            pattern: (*pattern).to_string(),
        });
    }
    Ok(out)
}

struct Run {
    elapsed: Duration,
    stdout: Vec<u8>,
    stderr: String,
    peak: Option<Peak>,
}

fn mib(m: Option<f64>) -> String {
    m.map_or_else(|| "?".into(), |m| format!("{m:.0}"))
}

#[derive(Clone, Copy)]
struct Peak {
    working_set: u64,
    private: u64,
}

#[cfg(windows)]
fn peak_memory(child: &std::process::Child) -> Option<Peak> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    let mut c =
        PROCESS_MEMORY_COUNTERS { cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>()).ok()?, ..Default::default() };
    // SAFETY: the child handle is valid until `child` is dropped (it has exited but is
    // not yet reaped), and `c` is a correctly sized PROCESS_MEMORY_COUNTERS.
    #[allow(unsafe_code)]
    let ok = unsafe { GetProcessMemoryInfo(child.as_raw_handle(), &raw mut c, c.cb) };
    (ok != 0).then_some(Peak { working_set: c.PeakWorkingSetSize as u64, private: c.PeakPagefileUsage as u64 })
}

/// Peak memory is only measured on Windows so far; elsewhere it is reported as unknown.
#[cfg(not(windows))]
fn peak_memory(_: &std::process::Child) -> Option<Peak> {
    None
}

fn run(program: &str, args: &[String]) -> anyhow::Result<Run> {
    let started = Instant::now();
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {program}"))?;
    let mut stdout = Vec::new();
    let mut stderr = String::new();
    // Drain both pipes concurrently so a chatty child can never block on a full pipe.
    std::thread::scope(|s| -> anyhow::Result<()> {
        let mut err_pipe = child.stderr.take().context("no stderr pipe")?;
        let h = s.spawn(move || {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut err_pipe, &mut buf).map(|_| buf)
        });
        let mut out_pipe = child.stdout.take().context("no stdout pipe")?;
        std::io::Read::read_to_end(&mut out_pipe, &mut stdout)?;
        stderr = h.join().map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
        Ok(())
    })?;
    let status = child.wait()?;
    let elapsed = started.elapsed();
    let peak = peak_memory(&child);
    // rg and scout both exit 1 for "no matches"; only 2+ is an error.
    if status.code().is_none_or(|c| c > 1) {
        bail!("{program} failed ({status}): {stderr}");
    }
    Ok(Run { elapsed, stdout, stderr, peak })
}

/// Normalizes output lines to comparable keys: `path:line` for match lines, the whole
/// line for `-l` / `-c` output. Windows drive letters (`C:`) are skipped when looking
/// for the path/line separator.
fn keys(out: &[u8], line_mode: bool) -> BTreeSet<String> {
    let text = String::from_utf8_lossy(out);
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            if !line_mode {
                return l.to_string();
            }
            let skip = if l.as_bytes().get(1) == Some(&b':') { 2 } else { 0 };
            let Some(p) = l[skip..].find(':').map(|i| i + skip) else {
                return l.to_string();
            };
            let rest = &l[p + 1..];
            let digits = rest.find(':').map_or(rest, |i| &rest[..i]);
            format!("{}:{digits}", &l[..p])
        })
        .collect()
}

fn stat(stderr: &str, key: &str) -> u64 {
    stderr
        .split_whitespace()
        .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// A p99 needs enough samples for the 99th to be a sample at all: from 100 runs it is the
/// second-slowest. Fewer runs report no tails rather than a misleading number.
const TAIL_MIN_RUNS: usize = 100;

/// Nearest-rank percentile of sorted samples.
fn percentile(sorted: &[f64], p: usize) -> f64 {
    let rank = (p * sorted.len()).div_ceil(100);
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn tail(v: &[f64], p: usize) -> Option<f64> {
    if v.len() < TAIL_MIN_RUNS {
        return None;
    }
    let mut sorted = v.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(percentile(&sorted, p))
}

fn opt_ms(v: Option<f64>) -> String {
    v.map_or_else(|| "-".to_string(), |v| format!("{v:.1}"))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn dir_size(dir: &Path) -> anyhow::Result<u64> {
    let mut total = 0;
    for e in std::fs::read_dir(dir)? {
        total += e?.metadata()?.len();
    }
    Ok(total)
}

#[allow(clippy::too_many_lines)] // Linear benchmark script; splitting it hides the flow.
fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    anyhow::ensure!(args.runs >= 1 && args.rg_runs >= 1, "runs must be >= 1");
    let store = FsStore::open(&args.index_dir)?;
    let catalog = store.load_catalog()?;
    let roots: Vec<String> = catalog.repos.iter().filter(|r| r.index.is_some()).map(|r| r.root.clone()).collect();
    anyhow::ensure!(!roots.is_empty(), "no indexed repos in {}", args.index_dir.display());
    let indexed_files: u64 = catalog.repos.iter().filter_map(|r| r.index.as_ref()).map(|i| i.files).sum();
    let content: u64 = catalog.repos.iter().filter_map(|r| r.index.as_ref()).map(|i| i.content_bytes).sum();
    let index_bytes = dir_size(&args.index_dir.join("shards"))?;

    let mut queries = parse_queries(&args.queries)?;
    if let Some(f) = &args.filter {
        queries.retain(|q| q.name.contains(f.as_str()));
    }
    let scout = args.scout.to_str().context("scout path is not UTF-8")?.to_string();
    let index_dir = args.index_dir.to_str().context("index dir is not UTF-8")?.to_string();
    let extra: Vec<String> = args.scout_flags.split_whitespace().map(str::to_string).collect();

    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "## {}  ({} repos, {indexed_files} files, {:.0} MiB content, {:.0} MiB index)\n",
        args.label,
        roots.len(),
        content as f64 / 1_048_576.0,
        index_bytes as f64 / 1_048_576.0
    )?;
    writeln!(
        out,
        "| query | scout ms | p90 | p99 | rg ms | speedup | candidates | matched files | results | parity | peak WS MiB | peak private MiB |"
    )?;
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|---:|---:|:-:|---:|---:|")?;

    let mut results = Vec::new();
    for q in &queries {
        let line_mode = !q.flags.iter().any(|f| f == "-l" || f == "-c");
        let mut scout_args =
            vec!["--index-dir".to_string(), index_dir.clone(), "search".into(), "-n".into(), "--stats".into()];
        scout_args.extend(extra.iter().cloned());
        scout_args.extend(q.flags.iter().cloned());
        scout_args.push("--".into());
        scout_args.push(q.pattern.clone());

        let mut rg_args: Vec<String> =
            ["--no-heading", "--with-filename", "--color", "never", "-n"].iter().map(|s| (*s).to_string()).collect();
        rg_args.extend(q.flags.iter().cloned());
        rg_args.push("-e".into());
        rg_args.push(q.pattern.clone());
        rg_args.extend(roots.iter().cloned());

        run(&scout, &scout_args)?; // warm-up: page in shards, like a long-lived workload
        let mut times = Vec::new();
        let mut last = None;
        for _ in 0..args.runs {
            let r = run(&scout, &scout_args)?;
            times.push(ms(r.elapsed));
            last = Some(r);
        }
        let last = last.context("no scout run")?;
        let mut rg_times = Vec::new();
        let mut rg_last = None;
        for _ in 0..args.rg_runs {
            let r = run(&args.rg, &rg_args)?;
            rg_times.push(ms(r.elapsed));
            rg_last = Some(r);
        }
        let rg_last = rg_last.context("no rg run")?;

        let a = keys(&last.stdout, line_mode);
        let b = keys(&rg_last.stdout, line_mode);
        let missing: Vec<_> = b.difference(&a).collect();
        let extra_found: Vec<_> = a.difference(&b).collect();
        let parity = missing.is_empty() && extra_found.is_empty();
        if !parity {
            let _ = writeln!(
                std::io::stderr(),
                "PARITY FAIL {}: {} missing, {} extra",
                q.name,
                missing.len(),
                extra_found.len()
            );
            for m in missing.iter().take(5) {
                let _ = writeln!(std::io::stderr(), "  missing: {m}");
            }
            for e in extra_found.iter().take(5) {
                let _ = writeln!(std::io::stderr(), "  extra:   {e}");
            }
        }
        let scout_median = median(times.clone());
        let rg_median = median(rg_times);
        let r = QueryResult {
            name: q.name.clone(),
            scout_median_ms: scout_median,
            scout_min_ms: times.iter().copied().fold(f64::INFINITY, f64::min),
            scout_p90_ms: tail(&times, 90),
            scout_p99_ms: tail(&times, 99),
            rg_median_ms: rg_median,
            speedup: rg_median / scout_median,
            candidates: stat(&last.stderr, "candidates"),
            matched_files: stat(&last.stderr, "matched_files"),
            results: b.len(),
            parity,
            missing_in_scout: missing.len(),
            extra_in_scout: extra_found.len(),
            scout_peak_ws_mb: last.peak.map(|p| p.working_set as f64 / 1_048_576.0),
            scout_peak_private_mb: last.peak.map(|p| p.private as f64 / 1_048_576.0),
        };
        writeln!(
            out,
            "| {} | {:.1} | {} | {} | {:.1} | {:.1}x | {} | {} | {} | {} | {} | {} |",
            r.name,
            r.scout_median_ms,
            opt_ms(r.scout_p90_ms),
            opt_ms(r.scout_p99_ms),
            r.rg_median_ms,
            r.speedup,
            r.candidates,
            r.matched_files,
            r.results,
            if r.parity { "ok" } else { "FAIL" },
            mib(r.scout_peak_ws_mb),
            mib(r.scout_peak_private_mb)
        )?;
        out.flush()?;
        results.push(r);
    }

    let geomean = (results.iter().map(|r| r.speedup.ln()).sum::<f64>() / results.len() as f64).exp();
    let report = Report {
        label: args.label.clone(),
        repos: roots.len(),
        indexed_files,
        content_mb: content as f64 / 1_048_576.0,
        index_mb: index_bytes as f64 / 1_048_576.0,
        geomean_speedup: geomean,
        total_scout_ms: results.iter().map(|r| r.scout_median_ms).sum(),
        total_rg_ms: results.iter().map(|r| r.rg_median_ms).sum(),
        all_parity: results.iter().all(|r| r.parity),
        queries: results,
    };
    writeln!(
        out,
        "\ngeomean speedup {:.1}x | total scout {:.0} ms vs rg {:.0} ms | parity {}",
        report.geomean_speedup,
        report.total_scout_ms,
        report.total_rg_ms,
        if report.all_parity { "ALL OK" } else { "FAILURES" }
    )?;
    if let Some(p) = &args.json {
        std::fs::write(p, serde_json::to_vec_pretty(&report)?).with_context(|| format!("writing {}", p.display()))?;
    }
    if !report.all_parity {
        bail!("parity failures; results are not trustworthy");
    }
    Ok(())
}
