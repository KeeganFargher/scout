//! An explicit, incomplete-capable search for interactive callers such as MCP.
//! Planning and verification advance in stable order, without prefetching later files.

use std::cell::RefCell;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use grep_printer::StandardBuilder;
use grep_searcher::{Searcher, Sink, SinkFinish, SinkMatch};
use serde::Serialize;
use termcolor::NoColor;

use super::{
    CaseMode, Freshness, OutputMode, Reading, RepoHandle, SearchOptions, SearchStats, build_matcher, build_searcher,
    display_path_into, in_repo, plan_shard, want,
};
use crate::query::{PlanCase, analyze};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub struct BoundedOptions {
    /// Global matching lines, rather than rg's per-file `-m` limit.
    pub max_matches: usize,
    /// Complete formatted records only; an oversized next line stops the batch.
    pub max_bytes: usize,
    /// Cooperative processing budget. A blocking read or one block's matching can
    /// finish after it expires; this is not an end-to-end transport/queue deadline.
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Complete,
    MatchLimit,
    ByteLimit,
    Deadline,
}

#[derive(Debug)]
pub struct BoundedReport {
    pub stats: SearchStats,
    pub returned_matches: usize,
    pub stop: StopReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CountEstimate {
    Exact(usize),
    LowerBound(usize),
}

impl BoundedReport {
    #[must_use]
    pub fn complete(&self) -> bool {
        self.stop == StopReason::Complete
    }

    #[must_use]
    pub fn count_estimate(&self) -> CountEstimate {
        if self.complete() {
            CountEstimate::Exact(self.returned_matches)
        } else {
            CountEstimate::LowerBound(self.returned_matches)
        }
    }
}

impl StopReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::MatchLimit => "match_limit",
            Self::ByteLimit => "byte_limit",
            Self::Deadline => "deadline",
        }
    }
}

/// Returns an exact prefix of normal line output, explicitly reporting when the
/// index was not exhausted. A reached limit does not prove further matches exist.
/// Ordering is stable traversal order, not global ranking. Freshness is the published
/// index snapshot: a strict filesystem listing would itself be unbounded work.
pub fn search_bounded(
    opts: &SearchOptions,
    bounds: BoundedOptions,
    repos: &[&RepoHandle],
    out: &mut dyn Write,
) -> Result<BoundedReport> {
    let started = Instant::now();
    let mut budget = new_budget(opts, bounds, started)?;
    let counting = opts.output == OutputMode::Count;
    let bytes = RefCell::new(Vec::new());
    let mut stats = SearchStats { repos: repos.len(), ..SearchStats::default() };
    scan(opts, repos, &mut budget, &bytes, &mut stats)?;
    let bytes = bytes.into_inner();
    out.write_all(&bytes).map_err(|e| Error::io("<stdout>", e))?;
    stats.elapsed = started.elapsed();
    debug_assert!(!counting || bytes.is_empty());
    Ok(BoundedReport { stats, returned_matches: budget.matches, stop: budget.stop })
}

fn new_budget(opts: &SearchOptions, bounds: BoundedOptions, started: Instant) -> Result<Budget> {
    if bounds.max_matches == 0 || bounds.max_bytes == 0 || bounds.timeout.is_zero() || opts.threads == 0 {
        return Err(Error::Invalid("bounded search needs positive match, byte, time and thread limits".into()));
    }
    if opts.output == OutputMode::FilesWithMatches || opts.freshness != Freshness::Off {
        return Err(Error::Invalid("bounded search requires line/count output and freshness off".into()));
    }
    if opts.pattern.len() > 4096 {
        return Err(Error::Invalid("bounded search patterns must not exceed 4096 bytes".into()));
    }
    let deadline = started
        .checked_add(bounds.timeout)
        .ok_or_else(|| Error::Invalid("bounded search timeout is out of range".into()))?;
    let deadline = opts.limits.deadline.map_or(deadline, |outer| outer.min(deadline));
    let max_bytes = opts.limits.output_bytes.map_or(bounds.max_bytes, |outer| outer.min(bounds.max_bytes));
    let counting = opts.output == OutputMode::Count;
    if counting && max_bytes < 256 {
        return Err(Error::Invalid("bounded count needs at least 256 bytes for its explicit count metadata".into()));
    }
    Ok(Budget { bounds, max_bytes, deadline, matches: 0, stop: StopReason::Complete })
}

fn scan(
    opts: &SearchOptions,
    repos: &[&RepoHandle],
    budget: &mut Budget,
    bytes: &RefCell<Vec<u8>>,
    stats: &mut SearchStats,
) -> Result<()> {
    let counting = opts.output == OutputMode::Count;
    // Validate the expression even for an already-expired budget or -m 0: neither
    // condition turns an invalid query into a successful empty/partial result.
    let pattern = if opts.fixed_strings { regex_syntax::escape(&opts.pattern) } else { opts.pattern.clone() };
    let case = if opts.case == CaseMode::Sensitive { PlanCase::Sensitive } else { PlanCase::Insensitive };
    let analysis = analyze(&pattern, case)?;
    let matcher = build_matcher(&pattern, opts)?;
    stats.plan = format!("{:?}", analysis.query);
    if !budget.expired() && opts.max_count != Some(0) {
        // One worker avoids speculative chunks, racing result slots and a reorder
        // buffer. A later caller can use the other CPU for another bounded request.
        let mut searcher = build_searcher(&SearchOptions { max_count: None, ..opts.clone() });
        let mut printer = StandardBuilder::new().heading(false).path(true).build(NoColor::new(Buffer(bytes)));
        let mut scratch = Vec::new();
        let mut path = String::new();
        'repos: for repo in repos {
            for shard in &repo.shards {
                if budget.expired() {
                    break 'repos;
                }
                let planned = Instant::now();
                let candidates = plan_shard(shard.as_ref(), &analysis.query, analysis.shape.as_ref(), None, stats)
                    .map_err(|e| in_repo(repo, e))?;
                stats.postings_time += planned.elapsed();
                stats.shards += 1;
                if budget.expired() {
                    break 'repos;
                }
                for candidate in &candidates.docs {
                    if budget.expired() {
                        break 'repos;
                    }
                    let meta = shard.meta(candidate.doc)?;
                    display_path_into(&mut path, repo.source.display_root(), &meta.path);
                    let reading = if analysis.whole_file { Reading::Whole } else { Reading::Units };
                    let wanted = want(shard.as_ref(), &candidates, candidate, reading)?;
                    let verify = Instant::now();
                    let reader = shard.markers().reader_mode.binary_search(&candidate.doc).is_ok();
                    let mut sink = BoundedSink {
                        inner: if counting {
                            Renderer::Count
                        } else {
                            Renderer::Lines(printer.sink_with_path(&matcher, &path))
                        },
                        bytes,
                        budget,
                        path_bytes: path.len(),
                        line_numbers: opts.line_number,
                        file_limit: opts.max_count,
                        file_matches: 0,
                        counting,
                        pending_count: counting && reader,
                    };
                    if reader || analysis.whole_file {
                        let content = shard.content(candidate.doc, &mut scratch)?;
                        if !sink.budget.expired() {
                            searcher.set_first_line_number(1);
                            if reader {
                                searcher
                                    .clone()
                                    .search_reader(&matcher, content, &mut sink)
                                    .map_err(|e| Error::io(&path, e))?;
                            } else {
                                searcher.search_slice(&matcher, content, &mut sink).map_err(|e| Error::io(&path, e))?;
                            }
                        }
                    } else {
                        // No whole-file read-ahead: the next decode happens only if
                        // the current block did not exhaust the result/time budget.
                        shard.visit_content(wanted, &mut scratch, &mut |content, first_line| {
                            if sink.budget.expired() {
                                return Ok(false);
                            }
                            searcher.set_first_line_number(first_line);
                            searcher.search_slice(&matcher, content, &mut sink).map_err(|e| Error::io(&path, e))?;
                            Ok(!sink.budget.expired() && sink.file_limit.is_none_or(|n| sink.file_matches < n))
                        })?;
                    }
                    stats.matched_files += u64::from(sink.file_matches > 0);
                    stats.verify_time += verify.elapsed();
                    if budget.stop != StopReason::Complete {
                        break 'repos;
                    }
                }
            }
        }
    }
    Ok(())
}

struct Budget {
    bounds: BoundedOptions,
    max_bytes: usize,
    deadline: Instant,
    matches: usize,
    stop: StopReason,
}

impl Budget {
    fn expired(&mut self) -> bool {
        if self.stop == StopReason::Complete && Instant::now() >= self.deadline {
            self.stop = StopReason::Deadline;
        }
        self.stop != StopReason::Complete
    }
}

struct Buffer<'a>(&'a RefCell<Vec<u8>>);

impl Write for Buffer<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BoundedSink<'a, S> {
    inner: S,
    bytes: &'a RefCell<Vec<u8>>,
    budget: &'a mut Budget,
    path_bytes: usize,
    line_numbers: bool,
    file_limit: Option<u64>,
    file_matches: u64,
    counting: bool,
    // rg suppresses a binary file's entire count when a late NUL is found. Do not
    // expose tentative reader matches as a lower bound before that decision is known.
    pending_count: bool,
}

enum Renderer<S> {
    Lines(S),
    Count,
}

impl<S: Sink<Error = io::Error>> Sink for Renderer<S> {
    type Error = io::Error;
    fn begin(&mut self, searcher: &Searcher) -> io::Result<bool> {
        match self {
            Self::Lines(s) => s.begin(searcher),
            Self::Count => Ok(true),
        }
    }
    fn matched(&mut self, searcher: &Searcher, mat: &SinkMatch<'_>) -> io::Result<bool> {
        match self {
            Self::Lines(s) => s.matched(searcher, mat),
            Self::Count => Ok(true),
        }
    }
    fn binary_data(&mut self, searcher: &Searcher, offset: u64) -> io::Result<bool> {
        match self {
            Self::Lines(s) => s.binary_data(searcher, offset),
            Self::Count => Ok(true),
        }
    }
    fn finish(&mut self, searcher: &Searcher, finish: &SinkFinish) -> io::Result<()> {
        match self {
            Self::Lines(s) => s.finish(searcher, finish),
            Self::Count => Ok(()),
        }
    }
}

impl<S: Sink<Error = io::Error>> Sink for BoundedSink<'_, S> {
    type Error = io::Error;

    fn begin(&mut self, searcher: &Searcher) -> io::Result<bool> {
        if self.budget.expired() {
            return Ok(false);
        }
        self.inner.begin(searcher)
    }

    fn matched(&mut self, searcher: &Searcher, mat: &SinkMatch<'_>) -> io::Result<bool> {
        if self.budget.expired() {
            return Ok(false);
        }
        let number_bytes = if self.line_numbers {
            let number =
                mat.line_number().ok_or_else(|| io::Error::other("bounded match has no requested line number"))?;
            itoa::Buffer::new().format(number).len() + 1
        } else {
            0
        };
        let before = self.bytes.borrow().len();
        let needed = if self.counting {
            0
        } else {
            self.path_bytes + 1 + number_bytes + mat.bytes().len() + usize::from(!mat.bytes().ends_with(b"\n"))
        };
        if needed > self.budget.max_bytes.saturating_sub(before) {
            self.budget.stop = StopReason::ByteLimit;
            return Ok(false);
        }
        let keep_going = self.inner.matched(searcher, mat)?;
        if self.bytes.borrow().len() > self.budget.max_bytes {
            // Preserve whole records even if printer path formatting needs extra bytes.
            self.bytes.borrow_mut().truncate(before);
            self.budget.stop = StopReason::ByteLimit;
            return Ok(false);
        }
        self.file_matches += 1;
        if self.pending_count {
            return Ok(keep_going && self.file_limit.is_none_or(|n| self.file_matches < n));
        }
        self.budget.matches += 1;
        if self.budget.matches >= self.budget.bounds.max_matches {
            self.budget.stop = StopReason::MatchLimit;
            return Ok(false);
        }
        Ok(keep_going && self.file_limit.is_none_or(|n| self.file_matches < n))
    }

    fn binary_data(&mut self, searcher: &Searcher, offset: u64) -> io::Result<bool> {
        self.inner.binary_data(searcher, offset)
    }

    fn finish(&mut self, searcher: &Searcher, finish: &SinkFinish) -> io::Result<()> {
        if self.pending_count {
            if self.budget.stop != StopReason::Complete
                || (finish.binary_byte_offset().is_some() && searcher.binary_detection().quit_byte().is_some())
            {
                self.file_matches = 0;
            } else {
                let remaining = self.budget.bounds.max_matches - self.budget.matches;
                self.budget.matches += usize::try_from(self.file_matches).map_or(remaining, |n| n.min(remaining));
                if self.budget.matches >= self.budget.bounds.max_matches {
                    self.budget.stop = StopReason::MatchLimit;
                }
            }
        }
        let before = self.bytes.borrow().len();
        self.inner.finish(searcher, finish)?;
        if self.bytes.borrow().len() > self.budget.max_bytes {
            self.bytes.borrow_mut().truncate(before);
            self.budget.stop = StopReason::ByteLimit;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Limits;

    #[test]
    fn an_expired_count_is_a_zero_lower_bound_never_an_exact_zero() {
        let opts = SearchOptions {
            pattern: "needle".into(),
            fixed_strings: true,
            case: CaseMode::Sensitive,
            word: false,
            line_number: false,
            output: OutputMode::Count,
            max_count: None,
            threads: 2,
            freshness: Freshness::Off,
            limits: Limits { output_bytes: None, deadline: Some(Instant::now()) },
        };
        let bounds = BoundedOptions { max_matches: 100, max_bytes: 65_536, timeout: Duration::from_secs(1) };
        let mut output = Vec::new();
        let report = search_bounded(&opts, bounds, &[], &mut output).unwrap();
        assert_eq!(report.stop, StopReason::Deadline);
        assert_eq!(report.count_estimate(), CountEstimate::LowerBound(0));
        assert!(!report.complete());
        assert!(output.is_empty());
        assert!(search_bounded(&opts, BoundedOptions { max_matches: 0, ..bounds }, &[], &mut output).is_err());
        let invalid = SearchOptions { pattern: "(".into(), fixed_strings: false, max_count: Some(0), ..opts };
        assert!(matches!(search_bounded(&invalid, bounds, &[], &mut output), Err(Error::Pattern(_))));
    }
}
