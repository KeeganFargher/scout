//! Regex → trigram query planning.
//!
//! Follows Russ Cox, "Regular Expression Matching with a Trigram Index" (2012): walk
//! the regex HIR computing, per node, the set of strings it can match exactly (when
//! small), the possible prefixes and suffixes, and a boolean trigram query that every
//! matching document must satisfy.
//!
//! **Soundness is the only hard requirement**: if the regex can match a document, the
//! query must accept it. Every approximation below therefore widens (towards
//! [`Query::All`]), never narrows. Precision is a performance concern measured by the
//! benchmark, soundness is a correctness concern guarded by tests and the rg parity
//! gate.

use std::collections::BTreeSet;

use regex_syntax::hir::{Class, Hir, HirKind};

use crate::trigram::{Trigram, fold};
use crate::{Error, Result};

/// Boolean query over trigram posting lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// Matches every document (no constraint could be derived).
    All,
    Tri(Trigram),
    And(Vec<Query>),
    Or(Vec<Query>),
}

impl Query {
    fn and(parts: Vec<Query>) -> Query {
        let mut out: Vec<Query> = Vec::new();
        for p in parts {
            match p {
                Query::All => {}
                Query::And(inner) => out.extend(inner),
                other => out.push(other),
            }
        }
        dedup(&mut out);
        match out.len() {
            0 => Query::All,
            1 => out.remove(0),
            _ => Query::And(out),
        }
    }

    fn or(parts: Vec<Query>) -> Query {
        let mut out: Vec<Query> = Vec::new();
        for p in parts {
            match p {
                Query::All => return Query::All,
                Query::Or(inner) => out.extend(inner),
                other => out.push(other),
            }
        }
        dedup(&mut out);
        match out.len() {
            // An empty OR means "no alternative can match". Treat it as All: sound,
            // and only reachable for regexes that can never match anyway.
            0 => Query::All,
            1 => out.remove(0),
            _ => Query::Or(out),
        }
    }
}

fn dedup(qs: &mut Vec<Query>) {
    let mut seen: Vec<Query> = Vec::with_capacity(qs.len());
    qs.retain(|q| {
        if seen.contains(q) {
            false
        } else {
            seen.push(q.clone());
            true
        }
    });
}

/// How the pattern's case sensitivity must be treated by the planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanCase {
    Sensitive,
    /// Used for `-i` *and* smart case: planning insensitively is always sound (the index
    /// is ASCII-folded anyway), so the planner never has to re-implement rg's smart-case
    /// heuristic.
    Insensitive,
}

/// Plans a trigram query for `pattern`, parsed with the same flags rg gives its matcher.
pub fn plan(pattern: &str, case: PlanCase) -> Result<Query> {
    analyze(pattern, case).map(|analysis| analysis.query)
}

pub(crate) struct Analysis {
    pub query: Query,
    pub whole_file: bool,
    /// Only an exact, nonempty literal with no line terminator can use byte search.
    pub literal: Option<Vec<u8>>,
    pub shape: Option<crate::shape::ShapeQuery>,
}

/// Planning and whether the matcher has haystack anchors. Keep those on the existing
/// whole-file search path: their exact behavior depends on the searcher's line strategy.
pub(crate) fn analyze(pattern: &str, case: PlanCase) -> Result<Analysis> {
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(case == PlanCase::Insensitive)
        .multi_line(true)
        .unicode(true)
        .utf8(false)
        .octal(false)
        .dot_matches_new_line(false)
        .build()
        .parse(pattern)
        .map_err(|e| Error::Pattern(e.to_string()))?;
    let whole_file = hir.properties().look_set().contains_anchor_haystack();
    let literal = match hir.kind() {
        HirKind::Literal(literal) if !literal.0.is_empty() && !literal.0.contains(&b'\n') => Some(literal.0.to_vec()),
        _ => None,
    };
    let query = info(&hir).into_query();
    let shape = if query == Query::All { crate::shape::ShapeQuery::from_hir(&hir) } else { None };
    Ok(Analysis { query, whole_file, literal, shape })
}

/// Upper bound on set sizes before we give up exactness. Keeps planning O(pattern) and
/// stops case-insensitive or class-heavy patterns from exploding combinatorially.
const MAX_SET: usize = 64;
/// Largest character class expanded into literal alternatives.
const MAX_CLASS: u32 = 16;

type Set = BTreeSet<Vec<u8>>;

#[derive(Debug, Clone)]
struct Info {
    emptyable: bool,
    /// Every string the node can match, when known and small.
    exact: Option<Set>,
    /// Possible prefixes / suffixes of a match. Only meaningful when `exact` is `None`.
    prefix: Set,
    suffix: Set,
    /// Trigram constraint every match satisfies.
    matchq: Query,
}

fn single(s: &[u8]) -> Set {
    std::iter::once(s.to_vec()).collect()
}

impl Info {
    fn anything(emptyable: bool) -> Self {
        Self { emptyable, exact: None, prefix: single(b""), suffix: single(b""), matchq: Query::All }
    }

    fn exact(set: Set) -> Self {
        let emptyable = set.contains(&Vec::new());
        Self { emptyable, exact: Some(set), prefix: Set::new(), suffix: Set::new(), matchq: Query::All }
    }

    fn prefixes(&self) -> &Set {
        self.exact.as_ref().unwrap_or(&self.prefix)
    }

    fn suffixes(&self) -> &Set {
        self.exact.as_ref().unwrap_or(&self.suffix)
    }

    /// Drops the exact set, first recording everything it implied into `matchq`,
    /// `prefix` and `suffix` so no information is lost.
    fn materialize(&mut self) {
        let Some(exact) = self.exact.take() else {
            return;
        };
        self.matchq = Query::and(vec![std::mem::replace(&mut self.matchq, Query::All), or_trigrams(&exact)]);
        self.prefix = exact.clone();
        self.suffix = exact;
        self.trim();
    }

    /// Keeps prefix/suffix sets small. Their full trigrams are first recorded in
    /// `matchq`; what remains useful afterwards is only the 2 bytes at the open end that
    /// can combine with a neighbour's bytes into a cross-boundary trigram.
    fn trim(&mut self) {
        let too_big = self.prefix.len() > MAX_SET
            || self.suffix.len() > MAX_SET
            || self.prefix.iter().chain(&self.suffix).any(|s| s.len() > 2);
        if !too_big {
            return;
        }
        self.matchq = Query::and(vec![
            std::mem::replace(&mut self.matchq, Query::All),
            or_trigrams(&self.prefix),
            or_trigrams(&self.suffix),
        ]);
        self.prefix = self.prefix.iter().map(|s| s[..s.len().min(2)].to_vec()).collect();
        self.suffix = self.suffix.iter().map(|s| s[s.len().saturating_sub(2)..].to_vec()).collect();
        if self.prefix.len() > MAX_SET {
            self.prefix = single(b"");
        }
        if self.suffix.len() > MAX_SET {
            self.suffix = single(b"");
        }
    }

    fn into_query(mut self) -> Query {
        if let Some(exact) = &self.exact {
            return Query::and(vec![self.matchq, or_trigrams(exact)]);
        }
        self.matchq = Query::and(vec![
            std::mem::replace(&mut self.matchq, Query::All),
            or_trigrams(&self.prefix),
            or_trigrams(&self.suffix),
        ]);
        self.matchq
    }
}

/// OR over strings, each string an AND of its trigrams, or one tagged pair for a
/// two-byte literal. A one-byte alternative leaves the OR unconstrained.
fn or_trigrams(set: &Set) -> Query {
    let mut alts = Vec::with_capacity(set.len());
    for s in set {
        if s.len() == 2 {
            alts.push(Query::Tri(Trigram::from_pair(s[0], s[1])));
            continue;
        }
        if s.len() < 2 {
            return Query::All;
        }
        alts.push(Query::and(s.windows(3).map(|w| Query::Tri(Trigram::from_bytes(w[0], w[1], w[2]))).collect()));
    }
    Query::or(alts)
}

fn cross(a: &Set, b: &Set) -> Option<Set> {
    if a.len().saturating_mul(b.len()) > MAX_SET {
        return None;
    }
    Some(a.iter().flat_map(|x| b.iter().map(move |y| [x.as_slice(), y].concat())).collect())
}

fn fold_bytes(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().copied().map(fold).collect()
}

fn info(hir: &Hir) -> Info {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::exact(single(b"")),
        HirKind::Literal(lit) => Info::exact(single(&fold_bytes(&lit.0))),
        HirKind::Class(class) => class_info(class),
        HirKind::Capture(cap) => info(&cap.sub),
        HirKind::Repetition(rep) => {
            let sub = info(&rep.sub);
            match (rep.min, rep.max) {
                (1, Some(1)) => sub,
                (0, Some(1)) => match sub.exact {
                    Some(mut e) => {
                        e.insert(Vec::new());
                        Info::exact(e)
                    }
                    None => Info::anything(true),
                },
                (0, _) => Info::anything(true),
                _ => {
                    // One or more repetitions: every match contains at least one match
                    // of `sub`, so `sub`'s constraints hold; its exact set does not.
                    let mut s = sub;
                    s.materialize();
                    s
                }
            }
        }
        HirKind::Concat(subs) => subs.iter().map(info).reduce(concat).unwrap_or_else(|| Info::exact(single(b""))),
        HirKind::Alternation(subs) => {
            subs.iter().map(info).reduce(alternate).unwrap_or_else(|| Info::exact(single(b"")))
        }
    }
}

fn class_info(class: &Class) -> Info {
    let mut set = Set::new();
    match class {
        Class::Unicode(c) => {
            let size: u32 = c.ranges().iter().map(|r| u32::from(r.end()) - u32::from(r.start()) + 1).sum();
            if size > MAX_CLASS {
                return Info::anything(false);
            }
            for r in c.ranges() {
                for ch in r.start()..=r.end() {
                    let mut buf = [0u8; 4];
                    set.insert(fold_bytes(ch.encode_utf8(&mut buf).as_bytes()));
                }
            }
        }
        Class::Bytes(c) => {
            let size: u32 = c.ranges().iter().map(|r| u32::from(r.end()) - u32::from(r.start()) + 1).sum();
            if size > MAX_CLASS {
                return Info::anything(false);
            }
            for r in c.ranges() {
                for b in r.start()..=r.end() {
                    set.insert(vec![fold(b)]);
                }
            }
        }
    }
    Info::exact(set)
}

fn concat(mut x: Info, mut y: Info) -> Info {
    if let (Some(ex), Some(ey)) = (&x.exact, &y.exact)
        && let Some(e) = cross(ex, ey)
    {
        let mut out = Info::exact(e);
        out.matchq = Query::and(vec![x.matchq, y.matchq]);
        if out.exact.as_ref().is_some_and(|s| s.iter().any(|s| s.len() > 64)) {
            out.materialize();
        }
        return out;
    }
    // Boundary trigrams spanning x's end and y's start.
    let boundary = cross(x.suffixes(), y.prefixes()).map_or(Query::All, |s| or_trigrams(&s));
    let prefix = match &x.exact {
        Some(ex) => cross(ex, y.prefixes()),
        None if x.emptyable => Some(x.prefix.union(y.prefixes()).cloned().collect()),
        None => Some(x.prefix.clone()),
    };
    let suffix = match &y.exact {
        Some(ey) => cross(x.suffixes(), ey),
        None if y.emptyable => Some(y.suffix.union(x.suffixes()).cloned().collect()),
        None => Some(y.suffix.clone()),
    };
    let emptyable = x.emptyable && y.emptyable;
    x.materialize();
    y.materialize();
    let mut out = Info {
        emptyable,
        exact: None,
        // A cross product that overflowed degrades to the trivial (always sound) set.
        prefix: prefix.unwrap_or_else(|| single(b"")),
        suffix: suffix.unwrap_or_else(|| single(b"")),
        matchq: Query::and(vec![x.matchq, y.matchq, boundary]),
    };
    out.trim();
    out
}

fn alternate(mut x: Info, mut y: Info) -> Info {
    if let (Some(ex), Some(ey)) = (&x.exact, &y.exact)
        && ex.len() + ey.len() <= MAX_SET
        && x.matchq == Query::All
        && y.matchq == Query::All
    {
        return Info::exact(ex.union(ey).cloned().collect());
    }
    x.materialize();
    y.materialize();
    let mut out = Info {
        emptyable: x.emptyable || y.emptyable,
        exact: None,
        prefix: x.prefix.union(&y.prefix).cloned().collect(),
        suffix: x.suffix.union(&y.suffix).cloned().collect(),
        // Each side's prefix/suffix constraints were already recorded in its matchq by
        // materialize/trim, so the OR of the two matchqs keeps them.
        matchq: Query::or(vec![
            Query::and(vec![x.matchq, or_trigrams(&x.prefix), or_trigrams(&x.suffix)]),
            Query::and(vec![y.matchq, or_trigrams(&y.prefix), or_trigrams(&y.suffix)]),
        ]),
    };
    out.trim();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Query {
        let b = s.as_bytes();
        Query::Tri(Trigram::from_bytes(b[0], b[1], b[2]))
    }

    fn tris(s: &str) -> Vec<Query> {
        s.as_bytes().windows(3).map(|w| Query::Tri(Trigram::from_bytes(w[0], w[1], w[2]))).collect()
    }

    fn p(pattern: &str) -> Query {
        plan(pattern, PlanCase::Sensitive).unwrap()
    }

    #[test]
    fn literal_becomes_and_of_trigrams() {
        assert_eq!(p("hello"), Query::And(tris("hello")));
    }

    #[test]
    fn short_or_unbounded_patterns_are_unconstrained() {
        assert_eq!(p("ab"), Query::Tri(Trigram::from_pair(b'a', b'b')));
        assert_eq!(p("."), Query::All);
        assert_eq!(p(".*"), Query::All);
        assert_eq!(p("a*b*c*"), Query::All);
    }

    #[test]
    fn alternation_becomes_or() {
        assert_eq!(p("foo|bar"), Query::Or(vec![t("bar"), t("foo")]));
    }

    #[test]
    fn literals_around_wildcards_are_all_required() {
        let q = p("hello.*world");
        let Query::And(parts) = q else { panic!("expected And, got {q:?}") };
        for want in tris("hello").into_iter().chain(tris("world")) {
            assert!(parts.contains(&want), "missing {want:?} in {parts:?}");
        }
    }

    #[test]
    fn plus_keeps_constraint_of_its_operand() {
        let q = p("(?:abcd)+x");
        let Query::And(parts) = q else { panic!("expected And, got {q:?}") };
        assert!(parts.contains(&t("abc")) && parts.contains(&t("bcd")));
    }

    #[test]
    fn small_classes_expand_and_case_folds() {
        // [Ff]oo and -i both reduce to the folded literal "foo".
        assert_eq!(p("[Ff]oo"), t("foo"));
        assert_eq!(plan("FOO", PlanCase::Insensitive).unwrap(), t("foo"));
        assert_eq!(p("FOO"), t("foo"));
    }

    #[test]
    fn large_classes_are_unconstrained_but_neighbours_still_count() {
        let q = p(r"\w+_handler");
        let Query::And(parts) = q else { panic!("expected And, got {q:?}") };
        for want in tris("_handler") {
            assert!(parts.contains(&want), "missing {want:?}");
        }
    }

    #[test]
    fn optional_parts_do_not_constrain() {
        // "colou?r": either "color" or "colour"; both contain "col" and "olo".
        let q = p("colou?r");
        let Query::And(parts) = q.clone() else {
            // Also acceptable: an OR of the two exact spellings.
            assert!(matches!(q, Query::Or(_)), "{q:?}");
            return;
        };
        assert!(parts.contains(&t("col")));
    }

    #[test]
    fn invalid_pattern_is_an_error() {
        assert!(plan("(", PlanCase::Sensitive).is_err());
    }
}
