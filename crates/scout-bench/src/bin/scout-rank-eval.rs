//! scout-rank-eval: the offline evaluation harness for `scout search --rank`
//! (docs/ranking.md, bench/golden/README.md).
//!
//! `dump` runs every golden query once and keeps the features scout scored. Everything
//! else re-scores those dumps with `scout_core::rank` under other weights, so tuning never
//! searches again and every number is reproducible from the dumps plus bench/golden.
//!
//! - `pool`: the top results of several systems, merged and shuffled into labelling
//!   packets that never show a score or which system found a file.
//! - `eval`: nDCG@10, MRR, noise@10 and judged@10 per system, and a paired permutation
//!   test between two of them.
//! - `tune`: coordinate ascent on the weights over every project but the held-out one,
//!   reported on the held-out project.
//! - `crossval`: `tune` with each project held out in turn; the decision number for
//!   whether tuned weights beat the defaults.
//!
//! Known-item targets (queries.tsv column 7) are the answer by construction: they and their
//! identical copies are graded 3 without a label, and a target missing from its dump is an
//! error, not a silent zero.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail, ensure};
use clap::{Parser, Subcommand};
use scout_core::rank::{self, Corpus, FileFeatures, Weights};
use serde::Deserialize;

#[derive(Parser)]
struct Args {
    /// bench/golden/queries.tsv.
    #[arg(long, default_value = "bench/golden/queries.tsv")]
    queries: PathBuf,
    /// Where `dump` writes one features file per query, and every other command reads them.
    #[arg(long)]
    dumps: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run every query once with `--rank-features` and keep the features.
    Dump {
        #[arg(long)]
        scout: PathBuf,
        #[arg(long)]
        index_dir: PathBuf,
    },
    /// Merge the top results of every system into labelling packets.
    Pool {
        /// How deep each system contributes.
        #[arg(long, default_value_t = 10)]
        depth: usize,
        /// Pairs already judged here are left out of the packets.
        #[arg(long, default_value = "bench/golden/labels.tsv")]
        labels: PathBuf,
        /// Also pool this system (e.g. tuned weights), as the system "weights".
        #[arg(long)]
        weights: Option<PathBuf>,
        /// One markdown packet per query with unjudged candidates is written here.
        #[arg(long)]
        out: PathBuf,
        /// The rg binary that prints each candidate's first matching lines.
        #[arg(long, default_value = "rg")]
        rg: String,
    },
    /// Score every system against the labels.
    Eval {
        #[arg(long, default_value = "bench/golden/labels.tsv")]
        labels: PathBuf,
        /// Evaluate these weights as the system "weights" too.
        #[arg(long)]
        weights: Option<PathBuf>,
        /// Paired permutation test of system B against system A, as "A,B". Repeatable, so
        /// one run (one output file) holds every comparison a decision needs.
        #[arg(long)]
        compare: Vec<String>,
    },
    /// Coordinate ascent on the weights; the held-out project only measures.
    Tune {
        #[arg(long, default_value = "bench/golden/labels.tsv")]
        labels: PathBuf,
        #[arg(long, default_value = "kanban-man")]
        holdout: String,
        /// Write the tuned weights here as JSON.
        #[arg(long)]
        out: PathBuf,
    },
    /// Leave-one-project-out cross-validation of the tuning: the decision number for
    /// whether tuned weights beat the defaults.
    Crossval {
        #[arg(long, default_value = "bench/golden/labels.tsv")]
        labels: PathBuf,
    },
}

/// Ten results: what an agent reads before it refines the search.
const CUTOFF: usize = 10;
/// Grade from which a file counts as an answer, for MRR (over the whole ranking, not @10).
const RELEVANT: u8 = 2;
/// Permutations for the paired test; enough for p to two decimals.
const PERMUTATIONS: usize = 10_000;

struct Query {
    id: String,
    repo: String,
    class: String,
    flags: Vec<String>,
    pattern: String,
    /// Known-item queries: the path of the one defining file.
    target: Option<String>,
}

fn parse_queries(path: &Path) -> anyhow::Result<Vec<Query>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.splitn(8, '\t').collect();
        let [id, repo, class, flags, _glob, _files, target, pattern] = cols.as_slice() else {
            bail!("{}:{}: expected 8 tab-separated columns", path.display(), i + 1)
        };
        out.push(Query {
            id: (*id).to_string(),
            repo: (*repo).to_string(),
            class: (*class).to_string(),
            flags: flags.split_whitespace().map(str::to_string).collect(),
            pattern: (*pattern).to_string(),
            target: (!target.is_empty()).then(|| (*target).to_string()),
        });
    }
    ensure!(!out.is_empty(), "{} has no queries", path.display());
    Ok(out)
}

#[derive(Deserialize)]
struct Dump {
    corpus: Corpus,
    files: Vec<FileFeatures>,
}

fn dump_path(dumps: &Path, q: &Query) -> PathBuf {
    dumps.join(format!("{}.json", q.id))
}

fn load_dump(dumps: &Path, q: &Query) -> anyhow::Result<Dump> {
    let p = dump_path(dumps, q);
    let bytes = std::fs::read(&p).with_context(|| format!("reading {} (run `dump` first)", p.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", p.display()))
}

/// `qid -> path -> grade`.
type Labels = HashMap<String, HashMap<String, u8>>;

/// A query's grades: its labels, plus grade 3 for a known-item target and every identical
/// copy of it. A target labelled lower than 3 contradicts the query and is an error.
fn grades(q: &Query, d: &Dump, labels: &Labels) -> anyhow::Result<HashMap<String, u8>> {
    let mut g = labels.get(&q.id).cloned().unwrap_or_default();
    let Some(target) = &q.target else { return Ok(g) };
    let hash = d
        .files
        .iter()
        .find(|f| &f.path == target)
        .with_context(|| format!("{}: known-item target {target} is not in the dump", q.id))?
        .content_hash;
    for f in d.files.iter().filter(|f| f.content_hash == hash) {
        let grade = *g.entry(f.path.clone()).or_insert(3);
        ensure!(grade == 3, "{}: {} is the known-item target (or a copy) but labelled {grade}", q.id, f.path);
    }
    Ok(g)
}

fn load_labels(path: &Path) -> anyhow::Result<Labels> {
    let mut out = Labels::new();
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        let [qid, grade, file] = cols.as_slice() else {
            bail!("{}:{}: expected qid<TAB>grade<TAB>path", path.display(), i + 1)
        };
        let grade: u8 = grade.parse().with_context(|| format!("{}:{}: grade", path.display(), i + 1))?;
        ensure!(grade <= 3, "{}:{}: grade {grade} is not 0-3", path.display(), i + 1);
        let previous = out.entry((*qid).to_string()).or_default().insert((*file).to_string(), grade);
        ensure!(previous.is_none(), "{}:{}: {qid} {file} is labelled twice", path.display(), i + 1);
    }
    Ok(out)
}

fn load_weights(path: &Path) -> anyhow::Result<Weights> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The orderings under evaluation. Each answers one question: does ranking help at all
/// (`unranked`), how much of it is just noise demotion (`noise`), how much is text
/// relevance without structure or noise (`bm25`), the scorer alone (`full`), and what
/// `scout search --rank` ships: the scorer with identical copies pushed back (`full+dedup`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum System {
    Unranked,
    Random,
    Noise,
    Bm25,
    Full,
    FullDedup,
    Weights,
}

impl System {
    const BUILT_IN: [Self; 6] = [Self::Unranked, Self::Random, Self::Noise, Self::Bm25, Self::Full, Self::FullDedup];

    fn name(self) -> &'static str {
        match self {
            Self::Unranked => "unranked",
            Self::Random => "random",
            Self::Noise => "noise",
            Self::Bm25 => "bm25",
            Self::Full => "full",
            Self::FullDedup => "full+dedup",
            Self::Weights => "weights",
        }
    }

    fn parse(name: &str) -> anyhow::Result<Self> {
        [Self::BUILT_IN.as_slice(), &[Self::Weights]]
            .concat()
            .into_iter()
            .find(|s| s.name() == name)
            .with_context(|| format!("no system {name:?}"))
    }
}

/// Text relevance only: no structural bonuses, no noise multipliers.
fn bm25_weights() -> Weights {
    Weights {
        w_def: 0.0,
        w_base: 0.0,
        w_path: 0.0,
        p_tests: 1.0,
        p_fixtures: 1.0,
        p_build: 1.0,
        p_vendored: 1.0,
        p_generated: 1.0,
        p_lockfile: 1.0,
        p_minified: 1.0,
        p_huge: 1.0,
        ..Weights::default()
    }
}

fn order(system: System, qid: &str, d: &Dump, tuned: Option<&Weights>) -> anyhow::Result<Vec<usize>> {
    let by_path = |files: &[FileFeatures]| {
        let mut idx: Vec<usize> = (0..files.len()).collect();
        idx.sort_by(|&a, &b| files[a].path.cmp(&files[b].path));
        idx
    };
    Ok(match system {
        // Plain search prints repos in catalog order and files in walk order; sorted
        // paths are the deterministic stand-in (both group by repo, neither knows
        // relevance).
        System::Unranked => by_path(&d.files),
        System::Random => {
            let mut idx = by_path(&d.files);
            shuffle(&mut idx, seed(qid));
            idx
        }
        System::Noise => {
            let w = Weights::default();
            let mut idx = by_path(&d.files);
            idx.sort_by(|&a, &b| w.multiplier(d.files[b].noise).total_cmp(&w.multiplier(d.files[a].noise)));
            idx
        }
        System::Bm25 => rank::order(&d.files, &d.corpus, &bm25_weights()),
        System::Full => rank::order(&d.files, &d.corpus, &Weights::default()),
        System::FullDedup => rank::collapse_copies(&d.files, rank::order(&d.files, &d.corpus, &Weights::default())),
        System::Weights => {
            let w = tuned.context("system \"weights\" needs --weights")?;
            rank::collapse_copies(&d.files, rank::order(&d.files, &d.corpus, w))
        }
    })
}

/// splitmix64: a fixed, dependency-free generator, so the random baseline and the
/// permutation test are identical on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn seed(text: &str) -> u64 {
    // FNV-1a: stable across runs and platforms, unlike std's RandomState.
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn shuffle<T>(v: &mut [T], seed: u64) {
    let mut rng = Rng(seed);
    for i in (1..v.len()).rev() {
        v.swap(i, rng.below(i + 1));
    }
}

struct Metrics {
    ndcg: f64,
    rr: f64,
    noise: f64,
    judged: f64,
}

/// Unjudged files count as grade 0, the usual pooled-evaluation assumption; judged@10
/// says how far to trust that for a system the pool did not include.
fn metrics(d: &Dump, ranked: &[usize], grades: &HashMap<String, u8>) -> Metrics {
    let grade = |i: usize| grades.get(&d.files[i].path).copied();
    let gain = |g: u8| f64::from((1u32 << g) - 1);
    let discount = |rank: usize| 1.0 / ((rank + 2) as f64).log2();
    let top = &ranked[..ranked.len().min(CUTOFF)];
    let dcg: f64 = top.iter().enumerate().map(|(r, &i)| gain(grade(i).unwrap_or(0)) * discount(r)).sum();
    // Ideal over every label: `check_grades` already proved each names a file of the dump.
    let mut ideal: Vec<u8> = grades.values().copied().collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg: f64 = ideal.iter().take(CUTOFF).enumerate().map(|(r, &g)| gain(g) * discount(r)).sum();
    let rr = ranked.iter().position(|&i| grade(i).is_some_and(|g| g >= RELEVANT)).map_or(0.0, |r| 1.0 / (r + 1) as f64);
    let mut hashes = HashSet::new();
    let noisy = top.iter().filter(|&&i| !hashes.insert(d.files[i].content_hash) || grade(i).unwrap_or(0) == 0).count();
    let judged = top.iter().filter(|&&i| grade(i).is_some()).count();
    let n = top.len().max(1) as f64;
    Metrics { ndcg: if idcg > 0.0 { dcg / idcg } else { 0.0 }, rr, noise: noisy as f64 / n, judged: judged as f64 / n }
}

fn mean(v: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = v.fold((0.0, 0usize), |(s, n), x| (s + x, n + 1));
    if n == 0 { 0.0 } else { sum / n as f64 }
}

/// Two-sided paired permutation (sign-flip) test on per-query differences.
fn permutation_p(a: &[f64], b: &[f64]) -> f64 {
    let diffs: Vec<f64> = a.iter().zip(b).map(|(x, y)| y - x).collect();
    let observed = mean(diffs.iter().copied()).abs();
    let mut rng = Rng(seed("permutation"));
    let extreme = (0..PERMUTATIONS)
        .filter(|_| mean(diffs.iter().map(|&d| if rng.next() & 1 == 0 { d } else { -d })).abs() >= observed - 1e-12)
        .count();
    (extreme + 1) as f64 / (PERMUTATIONS + 1) as f64
}

fn cmd_dump(
    queries: &[Query],
    dumps: &Path,
    scout: &Path,
    index_dir: &Path,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dumps).with_context(|| format!("creating {}", dumps.display()))?;
    for q in queries {
        let path = dump_path(dumps, q);
        // Freshness off: the dumps must describe the indexed snapshot the labels refer
        // to, not whatever the working trees look like today.
        let status = Command::new(scout)
            .arg("--index-dir")
            .arg(index_dir)
            .args(["search", "--rank", "--top", "1", "--freshness", "off", "--rank-features"])
            .arg(&path)
            .args(&q.flags)
            .arg("--")
            .arg(&q.pattern)
            .stdout(std::process::Stdio::null())
            .status()
            .with_context(|| format!("running {}", scout.display()))?;
        ensure!(status.success(), "{}: scout exited with {status}", q.id);
        writeln!(out, "{} dumped", q.id)?;
    }
    Ok(())
}

struct PoolSpec<'a> {
    depth: usize,
    labels: &'a Labels,
    tuned: Option<&'a Weights>,
    out_dir: &'a Path,
    rg: &'a str,
}

fn cmd_pool(queries: &[Query], dumps: &Path, spec: &PoolSpec, out: &mut impl Write) -> anyhow::Result<()> {
    let PoolSpec { depth, labels, tuned, out_dir, rg } = *spec;
    // A stale packet from an earlier pool would be labelled twice or for the wrong pool.
    ensure!(
        !out_dir.exists() || std::fs::read_dir(out_dir)?.next().is_none(),
        "{} is not empty; pool into a fresh directory",
        out_dir.display()
    );
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    // Random is evaluated but not pooled: its top 10 is mostly files no other system
    // would show, which roughly doubled the labelling load for a baseline whose only job is
    // to be beaten. judged@10 shows how much of it went unjudged.
    let mut systems: Vec<System> = System::BUILT_IN.into_iter().filter(|&s| s != System::Random).collect();
    systems.extend(tuned.map(|_| System::Weights));
    let (mut total, mut packets) = (0, 0);
    for q in queries {
        let d = load_dump(dumps, q)?;
        let judged = grades(q, &d, labels)?;
        let mut pool = BTreeSet::new();
        for &s in &systems {
            for i in order(s, &q.id, &d, tuned)?.into_iter().take(depth) {
                if !judged.contains_key(&d.files[i].path) {
                    pool.insert(i);
                }
            }
        }
        if pool.is_empty() {
            continue;
        }
        let mut pool: Vec<usize> = pool.into_iter().collect();
        // Shuffled per query so position says nothing about any system.
        shuffle(&mut pool, seed(&format!("pool {}", q.id)));
        let mut md = format!(
            "# {id}\n\n- pattern: `{pattern}`\n- flags: `{flags}`\n- the agent was working in: {repo}\n- class: {class}\n\n",
            id = q.id,
            pattern = q.pattern,
            flags = q.flags.join(" "),
            repo = q.repo,
            class = q.class,
        );
        for i in pool {
            let f = &d.files[i];
            // Only the path and the matching lines: size and noise class are scorer
            // features, and showing them would bias the labels toward the scorer.
            write!(md, "## {}\n\n```\n{}```\n\n", f.path, first_lines(rg, q, &f.path)?)?;
            total += 1;
        }
        let packet = out_dir.join(format!("{}.md", q.id));
        std::fs::write(&packet, md).with_context(|| format!("writing {}", packet.display()))?;
        packets += 1;
    }
    writeln!(out, "{total} candidates to label in {packets} packets under {}", out_dir.display())?;
    Ok(())
}

/// The first matching lines of a candidate, as the labeller's evidence.
fn first_lines(rg: &str, q: &Query, path: &str) -> anyhow::Result<String> {
    let o = Command::new(rg)
        .args(["-n", "-m", "6", "--max-columns", "200", "--max-columns-preview", "--no-config"])
        .args(&q.flags)
        .arg("-e")
        .arg(&q.pattern)
        .arg("--")
        .arg(path)
        .output()
        .with_context(|| format!("running {rg}"))?;
    // 1 = no match: the file changed since it was indexed; the labeller should see that.
    ensure!(o.status.code().is_some_and(|c| c <= 1), "rg failed on {path}: {}", String::from_utf8_lossy(&o.stderr));
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    Ok(if text.is_empty() { "(no match in the file on disk any more)\n".into() } else { text })
}

struct Scored<'a> {
    query: &'a Query,
    dump: Dump,
    grades: HashMap<String, u8>,
}

/// Every query with its dump and grades. Against the frozen index every label must name a
/// file of its dump: a miss means the labels and dumps come from different indexes, and
/// metrics would silently drop it, so it is an error. Identical copies must carry the same
/// grade (the rubric grades content, not location); a disagreement is a labelling error.
/// A query with no relevant file (all grades 0) has nothing to rank and is left out,
/// reported in `log`, as the README's "nothing to rank" rule says.
fn load_scored<'a>(
    queries: &'a [Query],
    dumps: &Path,
    labels: &Labels,
    log: &mut impl Write,
) -> anyhow::Result<Vec<Scored<'a>>> {
    let mut out = Vec::new();
    let mut total = 0usize;
    for q in queries {
        let dump = load_dump(dumps, q)?;
        let grades = grades(q, &dump, labels)?;
        check_grades(&q.id, &dump, &grades)?;
        total += grades.len();
        if !grades.values().any(|&g| g > 0) {
            writeln!(log, "{} left out: no matched file is relevant", q.id)?;
            continue;
        }
        out.push(Scored { query: q, dump, grades });
    }
    writeln!(log, "{total} labels, every one a file of its dump")?;
    Ok(out)
}

/// The label invariants `load_scored` relies on: at least one label, each naming a file of
/// the dump, and identical files graded alike.
fn check_grades(qid: &str, dump: &Dump, grades: &HashMap<String, u8>) -> anyhow::Result<()> {
    ensure!(!grades.is_empty(), "{qid} has no labels");
    let paths: HashSet<&str> = dump.files.iter().map(|f| f.path.as_str()).collect();
    let missing: Vec<&String> = grades.keys().filter(|p| !paths.contains(p.as_str())).collect();
    if let Some(first) = missing.first() {
        anyhow::bail!(
            "{qid}: {} of its {} labels name no file of its dump (wrong index?), e.g. {first}",
            missing.len(),
            grades.len()
        );
    }
    let mut by_hash: HashMap<u64, (&str, u8)> = HashMap::new();
    for f in dump.files.iter().filter(|f| grades.contains_key(&f.path)) {
        let g = grades[&f.path];
        let (first, fg) = *by_hash.entry(f.content_hash).or_insert((f.path.as_str(), g));
        ensure!(fg == g, "{qid}: identical files graded {fg} and {g}: {first} and {}", f.path);
    }
    Ok(())
}

/// `p` from [`permutation_p`], written as a bound when it is the test's floor.
fn show_p(p: f64) -> String {
    if p <= 1.0 / (PERMUTATIONS + 1) as f64 + 1e-12 {
        format!("p <= {:.4}", 1.0 / PERMUTATIONS as f64)
    } else {
        format!("p = {p:.4}")
    }
}

fn per_query(set: &[&Scored], system: System, tuned: Option<&Weights>) -> anyhow::Result<Vec<Metrics>> {
    set.iter().map(|s| Ok(metrics(&s.dump, &order(system, &s.query.id, &s.dump, tuned)?, &s.grades))).collect()
}

fn cmd_eval(
    scored: &[Scored],
    tuned: Option<&Weights>,
    compare: &[String],
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let mut systems = System::BUILT_IN.to_vec();
    systems.extend(tuned.map(|_| System::Weights));
    let all: Vec<&Scored> = scored.iter().collect();
    let mut classes: BTreeMap<&str, Vec<&Scored>> = BTreeMap::new();
    for s in scored {
        classes.entry(s.query.class.as_str()).or_default().push(s);
    }
    writeln!(out, "{} queries; cutoff {CUTOFF}\n", scored.len())?;
    writeln!(out, "{:<12} {:>8} {:>8} {:>9} {:>10}", "system", "nDCG@10", "MRR", "noise@10", "judged@10")?;
    for &s in &systems {
        let m = per_query(&all, s, tuned)?;
        writeln!(
            out,
            "{:<12} {:>8.3} {:>8.3} {:>9.3} {:>10.3}",
            s.name(),
            mean(m.iter().map(|m| m.ndcg)),
            mean(m.iter().map(|m| m.rr)),
            mean(m.iter().map(|m| m.noise)),
            mean(m.iter().map(|m| m.judged)),
        )?;
    }
    writeln!(out, "\nnDCG@10 by query class")?;
    write!(out, "{:<18} {:>3}", "class", "n")?;
    for &s in &systems {
        write!(out, " {:>10}", s.name())?;
    }
    writeln!(out)?;
    for (class, set) in &classes {
        write!(out, "{class:<18} {:>3}", set.len())?;
        for &s in &systems {
            write!(out, " {:>10.3}", mean(per_query(set, s, tuned)?.iter().map(|m| m.ndcg)))?;
        }
        writeln!(out)?;
    }
    for pair in compare {
        let (a, b) = pair.split_once(',').context("--compare takes A,B")?;
        let (a, b) = (System::parse(a)?, System::parse(b)?);
        let (ma, mb) = (per_query(&all, a, tuned)?, per_query(&all, b, tuned)?);
        // nDCG@10 is the headline; noise@10 is tested too because a change can trade
        // nothing in relevance for less noise (duplicate collapse does exactly that).
        for (metric, pick) in
            [("nDCG@10", (|m: &Metrics| m.ndcg) as fn(&Metrics) -> f64), ("noise@10", |m: &Metrics| m.noise)]
        {
            let (va, vb): (Vec<f64>, Vec<f64>) = (ma.iter().map(pick).collect(), mb.iter().map(pick).collect());
            let (higher, lower) = va
                .iter()
                .zip(&vb)
                .fold((0, 0), |(h, l), (x, y)| (h + usize::from(y > &(x + 1e-9)), l + usize::from(x > &(y + 1e-9))));
            writeln!(
                out,
                "{} vs {}: {metric} {:+.3} ({higher} higher, {lower} lower), paired permutation {}",
                b.name(),
                a.name(),
                mean(vb.iter().copied()) - mean(va.iter().copied()),
                show_p(permutation_p(&va, &vb)),
            )?;
        }
    }
    Ok(())
}

/// A tunable coordinate ascent moves: its name, where it lives in [`Weights`], and its lowest and highest value.
type Knob = (&'static str, fn(&mut Weights) -> &mut f64, f64, f64);

fn knobs() -> Vec<Knob> {
    vec![
        ("w_sub", |w| &mut w.w_sub, 0.0, 1.0),
        ("w_def", |w| &mut w.w_def, 0.0, 10.0),
        ("w_base", |w| &mut w.w_base, 0.0, 10.0),
        ("w_path", |w| &mut w.w_path, 0.0, 10.0),
        ("k1", |w| &mut w.k1, 0.1, 5.0),
        ("b", |w| &mut w.b, 0.0, 1.0),
        ("l_cap", |w| &mut w.l_cap, 1.0, 100.0),
        ("p_tests", |w| &mut w.p_tests, 0.01, 1.0),
        ("p_fixtures", |w| &mut w.p_fixtures, 0.01, 1.0),
        ("p_build", |w| &mut w.p_build, 0.01, 1.0),
        ("p_vendored", |w| &mut w.p_vendored, 0.01, 1.0),
        ("p_generated", |w| &mut w.p_generated, 0.01, 1.0),
        ("p_lockfile", |w| &mut w.p_lockfile, 0.01, 1.0),
        ("p_minified", |w| &mut w.p_minified, 0.01, 1.0),
        ("p_huge", |w| &mut w.p_huge, 0.01, 1.0),
    ]
}

fn ndcgs(set: &[&Scored], system: System, w: Option<&Weights>) -> anyhow::Result<Vec<f64>> {
    Ok(per_query(set, system, w)?.iter().map(|m| m.ndcg).collect())
}

/// Coordinate ascent from the defaults on `train`; returns the weights and the train
/// nDCG@10 before and after. Each accepted step is logged.
fn ascend(train: &[&Scored], log: &mut impl Write) -> anyhow::Result<(Weights, f64, f64)> {
    // w_word stays 1: the score's scale is arbitrary, so one weight is fixed and the rest
    // are relative to it.
    let objective =
        |w: &Weights| -> anyhow::Result<f64> { Ok(mean(ndcgs(train, System::Weights, Some(w))?.into_iter())) };
    let mut best = Weights::default();
    let mut best_score = objective(&best)?;
    let start = best_score;
    for round in 1..=10 {
        let before = best_score;
        for (name, field, lo, hi) in knobs() {
            let current = *field(&mut best);
            let default = *field(&mut Weights::default());
            let candidates = [0.0, 0.25, 0.5, 0.8, 1.25, 2.0, 4.0].map(|f| current * f);
            // Multiplying alone could never move a knob that reached 0 again.
            for value in candidates.into_iter().chain([default]) {
                let mut w = best;
                *field(&mut w) = value.clamp(lo, hi);
                let s = objective(&w)?;
                // Strictly better by a margin: ties keep the default, which is the prior.
                if s > best_score + 1e-4 {
                    writeln!(log, "round {round}: {name} {current:.3} -> {:.3}, train nDCG@10 {s:.4}", *field(&mut w))?;
                    best = w;
                    best_score = s;
                }
            }
        }
        if best_score <= before {
            break;
        }
    }
    Ok((best, start, best_score))
}

/// Leave-one-project-out: every query is scored by weights tuned without its project, so
/// the pooled comparison has all queries' power and none of their bias. The baseline is
/// full+dedup: tuned weights are applied with copies collapsed, as `--rank` ships.
fn cmd_crossval(scored: &[Scored], out: &mut impl Write) -> anyhow::Result<()> {
    let projects: BTreeSet<&str> = scored.iter().map(|s| s.query.repo.as_str()).collect();
    let (mut defaults, mut tuned) = (Vec::new(), Vec::new());
    for project in projects {
        let (test, train): (Vec<&Scored>, Vec<&Scored>) = scored.iter().partition(|s| s.query.repo == project);
        let (w, _, _) = ascend(&train, &mut std::io::sink())?;
        let (d, t) = (ndcgs(&test, System::FullDedup, None)?, ndcgs(&test, System::Weights, Some(&w))?);
        writeln!(
            out,
            "{project:<18} {:>3} queries: nDCG@10 {:.4} -> {:.4}",
            test.len(),
            mean(d.iter().copied()),
            mean(t.iter().copied())
        )?;
        defaults.extend(d);
        tuned.extend(t);
    }
    writeln!(
        out,
        "all held out ({} queries): nDCG@10 {:.4} -> {:.4}, paired permutation {}",
        defaults.len(),
        mean(defaults.iter().copied()),
        mean(tuned.iter().copied()),
        show_p(permutation_p(&defaults, &tuned)),
    )?;
    Ok(())
}

fn cmd_tune(scored: &[Scored], holdout: &str, path: &Path, out: &mut impl Write) -> anyhow::Result<()> {
    let (test, train): (Vec<&Scored>, Vec<&Scored>) = scored.iter().partition(|s| s.query.repo == holdout);
    ensure!(!test.is_empty(), "no queries from the held-out project {holdout:?}");
    let (best, start, best_score) = ascend(&train, out)?;
    let default_test = ndcgs(&test, System::FullDedup, None)?;
    let tuned_test = ndcgs(&test, System::Weights, Some(&best))?;
    writeln!(out, "\ntrain ({} queries): nDCG@10 {start:.4} -> {best_score:.4}", train.len())?;
    writeln!(
        out,
        "held out {holdout} ({} queries): nDCG@10 {:.4} -> {:.4}, paired permutation {}",
        test.len(),
        mean(default_test.iter().copied()),
        mean(tuned_test.iter().copied()),
        show_p(permutation_p(&default_test, &tuned_test)),
    )?;
    std::fs::write(path, serde_json::to_string_pretty(&best)?)
        .with_context(|| format!("writing {}", path.display()))?;
    writeln!(out, "tuned weights written to {}", path.display())?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let queries = parse_queries(&args.queries)?;
    let mut out = std::io::stdout().lock();
    match &args.command {
        Cmd::Dump { scout, index_dir } => cmd_dump(&queries, &args.dumps, scout, index_dir, &mut out),
        Cmd::Pool { depth, labels, weights, out: dir, rg } => {
            ensure!(*depth > 0, "--depth must be at least 1");
            let tuned = weights.as_deref().map(load_weights).transpose()?;
            let labels = load_labels(labels)?;
            let spec = PoolSpec { depth: *depth, labels: &labels, tuned: tuned.as_ref(), out_dir: dir, rg };
            cmd_pool(&queries, &args.dumps, &spec, &mut out)
        }
        Cmd::Eval { labels, weights, compare } => {
            let tuned = weights.as_deref().map(load_weights).transpose()?;
            let scored = load_scored(&queries, &args.dumps, &load_labels(labels)?, &mut out)?;
            cmd_eval(&scored, tuned.as_ref(), compare, &mut out)
        }
        Cmd::Tune { labels, holdout, out: path } => {
            let scored = load_scored(&queries, &args.dumps, &load_labels(labels)?, &mut out)?;
            cmd_tune(&scored, holdout, path, &mut out)
        }
        Cmd::Crossval { labels } => {
            let scored = load_scored(&queries, &args.dumps, &load_labels(labels)?, &mut out)?;
            cmd_crossval(&scored, &mut out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scout_core::rank::{Noise, TermHits};

    fn file(path: &str, hash: u64) -> FileFeatures {
        FileFeatures {
            path: path.into(),
            bytes: 100,
            noise: Noise::None,
            content_hash: hash,
            terms: vec![TermHits::default()],
        }
    }

    fn dump(files: Vec<FileFeatures>) -> Dump {
        let corpus = Corpus::of(&files, 1, files.len() as u64);
        Dump { corpus, files }
    }

    #[test]
    fn ndcg_is_one_for_the_ideal_order_and_unjudged_files_count_as_zero() {
        let d = dump(vec![file("a", 1), file("b", 2), file("c", 3)]);
        let grades = HashMap::from([("a".to_string(), 3), ("b".to_string(), 1)]);
        let ideal = metrics(&d, &[0, 1, 2], &grades);
        assert!((ideal.ndcg - 1.0).abs() < 1e-12);
        assert!((ideal.rr - 1.0).abs() < 1e-12);
        let worse = metrics(&d, &[2, 1, 0], &grades);
        assert!(worse.ndcg < ideal.ndcg);
        assert!((worse.rr - 1.0 / 3.0).abs() < 1e-12);
        assert!((worse.judged - 2.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn a_copy_counts_as_noise_and_dedup_moves_it_back() {
        let d = dump(vec![file("a", 7), file("fork/a", 7), file("b", 8)]);
        let grades = HashMap::from([("a".into(), 3), ("fork/a".into(), 3), ("b".into(), 2)]);
        assert!((metrics(&d, &[0, 1, 2], &grades).noise - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(rank::collapse_copies(&d.files, vec![0, 1, 2]), vec![0, 2, 1]);
    }

    #[test]
    fn identical_systems_are_not_significant_and_a_consistent_win_is() {
        let a = vec![0.5; 30];
        assert!(permutation_p(&a, &a) > 0.99);
        let b: Vec<f64> = a.iter().map(|x| x + 0.1).collect();
        assert!(permutation_p(&a, &b) < 0.001);
    }

    fn known_item(target: &str) -> Query {
        Query {
            id: "q1".into(),
            repo: "r".into(),
            class: "known-item".into(),
            flags: vec![],
            pattern: "Foo".into(),
            target: Some(target.into()),
        }
    }

    #[test]
    fn a_known_item_target_and_its_copies_are_the_answer() {
        let d = dump(vec![file("src/foo.rs", 7), file("fork/src/foo.rs", 7), file("src/use.rs", 8)]);
        let labels = Labels::from([("q1".into(), HashMap::from([("src/use.rs".into(), 1)]))]);
        let g = grades(&known_item("src/foo.rs"), &d, &labels).unwrap();
        assert_eq!((g["src/foo.rs"], g["fork/src/foo.rs"], g["src/use.rs"]), (3, 3, 1));

        assert!(grades(&known_item("src/gone.rs"), &d, &labels).is_err(), "a target missing from the dump");
        let contradicting = Labels::from([("q1".into(), HashMap::from([("fork/src/foo.rs".into(), 1)]))]);
        assert!(grades(&known_item("src/foo.rs"), &d, &contradicting).is_err(), "a copy labelled below 3");
    }

    #[test]
    fn labels_must_name_dump_files_and_copies_must_agree() {
        let d = dump(vec![file("a", 7), file("fork/a", 7), file("b", 8)]);
        let ok = HashMap::from([("a".into(), 2), ("fork/a".into(), 2), ("b".into(), 0)]);
        check_grades("q1", &d, &ok).unwrap();

        let missing = HashMap::from([("a".into(), 2), ("gone".into(), 1)]);
        let e = check_grades("q1", &d, &missing).unwrap_err().to_string();
        assert!(e.contains("gone"), "{e}");

        let disagreeing = HashMap::from([("a".into(), 3), ("fork/a".into(), 1)]);
        let e = check_grades("q1", &d, &disagreeing).unwrap_err().to_string();
        assert!(e.contains("identical files graded"), "{e}");

        assert!(check_grades("q1", &d, &HashMap::new()).is_err(), "no labels");
    }

    #[test]
    fn the_random_baseline_is_stable() {
        let mut x: Vec<usize> = (0..50).collect();
        let mut y = x.clone();
        shuffle(&mut x, seed("q001"));
        shuffle(&mut y, seed("q001"));
        assert_eq!(x, y);
        assert_ne!(x, (0..50).collect::<Vec<_>>());
    }
}
