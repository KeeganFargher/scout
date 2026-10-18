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
    /// Files in the diff holding a NUL, whose expected output was re-taken from rg searching
    /// them alone (see [`reverify_nul_files`]).
    reverified_nul_files: usize,
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

/// `None` where the platform does not measure it (see `process_peak_memory`).
fn peak_memory(child: &std::process::Child) -> anyhow::Result<Option<Peak>> {
    match scout_source_fs::process_peak_memory(child) {
        Ok((working_set, private)) => Ok(Some(Peak { working_set, private })),
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => Ok(None),
        Err(e) => Err(e).context("reading the child's peak memory"),
    }
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
    let peak = peak_memory(&child)?;
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

/// The file a key belongs to: the whole key for `-l`, else everything before the last `:`
/// (`path:line`, `path: WARNING`, `path:count`; the part after it never holds a `:`).
fn key_path(key: &str, files_mode: bool) -> &str {
    if files_mode { key } else { key.rsplit_once(':').map_or(key, |(p, _)| p) }
}

/// rg's output for a file whose first NUL lies past its first 64 KiB read depends on what
/// the same search thread read before it: rg reuses each thread's line buffer, which only
/// grows. A grown buffer sees the NUL in its first fill and prints nothing; a fresh one
/// prints the matches before the fill that holds the NUL, then a warning. Which one a file
/// gets depends on the walk order and thread scheduling, so a parity diff on such a file is
/// rg disagreeing with itself. scout defines the output as rg's with a fresh buffer (each
/// reader-mode doc gets a fresh searcher, see `scout-core`'s `search::run`). So for every
/// file in the diff that holds a NUL, the expected keys are replaced with those of rg
/// searching a lone copy of it (the first and only file on its thread), with the query's
/// own flags.
///
/// Only that case is re-verified, so nothing else can hide behind it: a file without a NUL
/// or with one inside the first read (rg's output for it is deterministic) stays a failure,
/// and so does a file rg would not search at all over the same roots (ignored, hidden,
/// excluded by the query's globs), since the lone copy has no ignore files around it.
/// Returns how many files were re-verified.
fn reverify_nul_files(
    rg: &str,
    q: &Query,
    roots: &[String],
    scout: &BTreeSet<String>,
    expected: &mut BTreeSet<String>,
    files_mode: bool,
) -> anyhow::Result<usize> {
    let diff: BTreeSet<String> =
        scout.symmetric_difference(expected).map(|k| key_path(k, files_mode).to_string()).collect();
    let mut searched: Option<BTreeSet<String>> = None;
    let mut reverified = 0;
    for path in diff {
        let bytes = std::fs::read(&path).with_context(|| format!("reading {path} to re-verify it"))?;
        if bytes.iter().position(|&b| b == 0).is_none_or(|at| at < RG_FIRST_READ) {
            continue;
        }
        if searched.is_none() {
            // The query's flags choose files too (-t, -g, --hidden), but -l and -c are modes
            // and the last mode flag wins: `--files` goes after them or rg searches for the
            // first root as a pattern.
            let mut args: Vec<String> = q.flags.clone();
            args.push("--files".to_string());
            args.extend(roots.iter().cloned());
            let out = run(rg, &args)?;
            searched = Some(String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect());
        }
        if !searched.as_ref().is_some_and(|s| s.contains(&path)) {
            continue;
        }
        let (root, rel) = roots
            .iter()
            .find_map(|r| path.strip_prefix(r.as_str()).map(|rel| (r, rel.trim_start_matches(['/', '\\']))))
            .with_context(|| format!("{path} is under no root"))?;
        // The copy keeps its path below the root, so path-based flags (-t, -g 'src/**')
        // select it as they did there; ignore rules were already applied by `--files`.
        let dir = std::env::temp_dir().join(format!("scout-bench-nul-{}", std::process::id()));
        let copy = dir.join(rel);
        std::fs::create_dir_all(copy.parent().context("copy has no parent")?)?;
        let mut args: Vec<String> = ["--no-heading", "--with-filename", "--color", "never", "-n", "-j1"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        args.extend(q.flags.iter().cloned());
        args.extend(["-e".to_string(), q.pattern.clone(), dir.to_str().context("temp dir is not UTF-8")?.to_string()]);
        // The directory goes whether the copy or the search failed; the copy's or search's
        // error is the one reported, the cleanup's only when they succeeded.
        let r = std::fs::write(&copy, &bytes).map_err(anyhow::Error::from).and_then(|()| run(rg, &args));
        let cleaned = std::fs::remove_dir_all(&dir);
        let r = r?;
        cleaned?;
        let prefix = dir.to_str().context("temp dir is not UTF-8")?;
        let fresh = keys(&r.stdout, !files_mode && !q.flags.iter().any(|f| f == "-c"));
        expected.retain(|k| key_path(k, files_mode) != path);
        for k in fresh {
            let rest = k.strip_prefix(prefix).with_context(|| format!("rg printed {k:?}, not under {prefix}"))?;
            expected.insert(format!("{root}{rest}"));
        }
        reverified += 1;
    }
    Ok(reverified)
}

/// rg's first read of a file fills its default 64 KiB buffer (`grep-searcher`'s
/// `DEFAULT_BUFFER_CAPACITY`); a NUL inside it is seen whatever the buffer's history.
const RG_FIRST_READ: usize = 64 * 1024;

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
        let mut b = keys(&rg_last.stdout, line_mode);
        let files_mode = q.flags.iter().any(|f| f == "-l");
        let reverified = reverify_nul_files(&args.rg, q, &roots, &a, &mut b, files_mode)?;
        if reverified > 0 {
            let _ =
                writeln!(std::io::stderr(), "{}: re-verified {reverified} NUL file(s) against fresh-buffer rg", q.name);
        }
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
            reverified_nul_files: reverified,
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
            match (r.parity, r.reverified_nul_files) {
                (false, _) => "FAIL".to_string(),
                (true, 0) => "ok".to_string(),
                (true, n) => format!("ok ({n} NUL re-verified)"),
            },
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

#[cfg(test)]
mod tests {
    use super::{key_path, keys};

    #[test]
    fn key_path_recovers_the_file_of_every_key_shape() {
        let lines = keys(b"/r/a.txt:12:if x\n/r/a.txt: WARNING: stopped searching binary file\n", true);
        assert!(lines.iter().all(|k| key_path(k, false) == "/r/a.txt"), "{lines:?}");
        assert_eq!(key_path(r"C:\r\a.txt:3", false), r"C:\r\a.txt");
        assert_eq!(key_path("/r/a.txt:7", false), "/r/a.txt", "-c output is path:count");
        assert_eq!(key_path("/r/a:b.txt", true), "/r/a:b.txt", "-l output is the whole path");
    }
}
