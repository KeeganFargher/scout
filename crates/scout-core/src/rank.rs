//! Ranking matched files (docs/ranking.md). Pure: per-file features in, a score out, so
//! the scorer is unit-tested without an index and the evaluation harness can re-score
//! saved features under other weights without re-running searches.

use serde::{Deserialize, Serialize};

/// Hits of one query term (one top-level alternation branch) in one file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermHits {
    /// Matches in the content.
    pub matches: u32,
    /// Of those, matches bounded by non-word bytes on both sides.
    pub word: u32,
    /// Of those, matches on a line that looks like a definition ([`defined_name`]).
    pub def: u32,
    /// The term matches the file name.
    pub basename: bool,
    /// The term matches a directory segment of the repo-relative path.
    pub path: bool,
}

/// Why a file is likely not what anyone searching wants to read first. Ordered from
/// least to most severe; a file in several classes gets the most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Noise {
    None,
    Tests,
    Fixtures,
    Build,
    Vendored,
    Generated,
    Lockfile,
    Minified,
    Huge,
}

/// Everything the scorer needs about one matched file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileFeatures {
    /// The path as printed (root plus repo-relative path).
    pub path: String,
    pub bytes: u64,
    pub noise: Noise,
    /// xxh3 of the content, to collapse identical copies across repos.
    pub content_hash: u64,
    /// One entry per query term, in pattern order.
    pub terms: Vec<TermHits>,
}

/// Every tunable of the score; docs/ranking.md has the formula.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Weights {
    pub w_word: f64,
    pub w_sub: f64,
    pub w_def: f64,
    pub w_base: f64,
    pub w_path: f64,
    pub k1: f64,
    pub b: f64,
    pub l_cap: f64,
    pub p_tests: f64,
    pub p_fixtures: f64,
    pub p_build: f64,
    pub p_vendored: f64,
    pub p_generated: f64,
    pub p_lockfile: f64,
    pub p_minified: f64,
    pub p_huge: f64,
}

impl Default for Weights {
    /// Starting points from zoekt and the research report (docs/ranking-research.md).
    /// Tuning them on the golden set lost in cross-validation (docs/experiments.md, R3).
    fn default() -> Self {
        Self {
            w_word: 1.0,
            w_sub: 0.3,
            w_def: 1.0,
            w_base: 1.5,
            w_path: 0.5,
            k1: 1.2,
            b: 0.75,
            l_cap: 20.0,
            p_tests: 0.5,
            p_fixtures: 0.3,
            p_build: 0.2,
            p_vendored: 0.2,
            p_generated: 0.2,
            p_lockfile: 0.1,
            p_minified: 0.1,
            p_huge: 0.05,
        }
    }
}

impl Weights {
    #[must_use]
    pub fn multiplier(&self, noise: Noise) -> f64 {
        match noise {
            Noise::None => 1.0,
            Noise::Tests => self.p_tests,
            Noise::Fixtures => self.p_fixtures,
            Noise::Build => self.p_build,
            Noise::Vendored => self.p_vendored,
            Noise::Generated => self.p_generated,
            Noise::Lockfile => self.p_lockfile,
            Noise::Minified => self.p_minified,
            Noise::Huge => self.p_huge,
        }
    }
}

/// Collection statistics over one search's matched files: what IDF and length
/// normalisation are relative to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Corpus {
    /// Docs searched (matched or not): IDF's N.
    pub docs: u64,
    /// Per term, the matched files containing it.
    pub df: Vec<u64>,
    /// Mean size of the matched files that are not noise.
    pub avg_bytes: f64,
}

impl Corpus {
    /// `docs` counts every doc the search looked at, not only the matched ones.
    #[must_use]
    pub fn of(files: &[FileFeatures], terms: usize, docs: u64) -> Self {
        let mut df = vec![0u64; terms];
        let (mut sum, mut n) = (0u64, 0u64);
        for f in files {
            for (d, t) in df.iter_mut().zip(&f.terms) {
                *d += u64::from(t.matches > 0 || t.basename || t.path);
            }
            if f.noise == Noise::None {
                sum += f.bytes;
                n += 1;
            }
        }
        // All-noise results still need a scale; their own mean is the honest one then.
        if n == 0 {
            sum = files.iter().map(|f| f.bytes).sum();
            n = files.len() as u64;
        }
        #[allow(clippy::cast_precision_loss)] // Byte counts far below 2^52.
        let avg_bytes = if n == 0 { 1.0 } else { (sum as f64 / n as f64).max(1.0) };
        Self { docs: docs.max(files.len() as u64), df, avg_bytes }
    }
}

#[must_use]
#[allow(clippy::cast_precision_loss)] // Counts far below 2^52.
pub fn score(f: &FileFeatures, c: &Corpus, w: &Weights) -> f64 {
    let rel_len = (f.bytes as f64 / c.avg_bytes).min(w.l_cap);
    let norm = w.k1 * (1.0 - w.b + w.b * rel_len);
    let mut total = 0.0;
    for (t, &df) in f.terms.iter().zip(&c.df) {
        // Content hits saturate (BM25): the 5,000th match says little. Structural hits do
        // not, and are added outside the saturation: otherwise every term tops out near
        // k1 + 1 and a data dump with thousands of hits ties the file that defines the
        // name (zoekt's default scorer likewise adds symbol hits on top of word hits).
        let tf = w.w_word * f64::from(t.word) + w.w_sub * f64::from(t.matches - t.word);
        let content = if tf > 0.0 { (w.k1 + 1.0) * tf / (norm + tf) } else { 0.0 };
        let structure = if t.def > 0 { w.w_def } else { 0.0 }
            + if t.basename { w.w_base } else { 0.0 }
            + if t.path { w.w_path } else { 0.0 };
        if content + structure <= 0.0 {
            continue;
        }
        let n = c.docs as f64;
        let df = df as f64;
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        total += idf * (content + structure);
    }
    w.multiplier(f.noise) * total
}

/// Indices of `files` in rank order: score descending, then path for a stable order.
#[must_use]
pub fn order(files: &[FileFeatures], c: &Corpus, w: &Weights) -> Vec<usize> {
    let scores: Vec<f64> = files.iter().map(|f| score(f, c, w)).collect();
    let mut idx: Vec<usize> = (0..files.len()).collect();
    idx.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then_with(|| files[a].path.cmp(&files[b].path)));
    idx
}

/// Moves every identical copy (same content hash as a file ranked above it) behind all
/// first copies, keeping relative order: a fork's or worktree's copy of a file the agent
/// has already read is pure noise in its top results. Nothing is dropped, so the ranked
/// set stays rg's. Measured on the golden set in docs/experiments.md (R2).
#[must_use]
pub fn collapse_copies(files: &[FileFeatures], ranked: Vec<usize>) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();
    let (first, copies): (Vec<usize>, Vec<usize>) =
        ranked.into_iter().partition(|&i| seen.insert(files[i].content_hash));
    [first, copies].concat()
}

/// Bytes of a file the noise markers are looked for in: generators stamp their header at
/// the top.
pub const HEAD_BYTES: usize = 1024;

/// The noise class of a file from its repo-relative path (`/`-separated), its first
/// [`HEAD_BYTES`] and its shape.
#[must_use]
pub fn classify(rel: &str, head: &[u8], bytes: u64, lines: u64, longest_line: u64) -> Noise {
    let lower = rel.to_ascii_lowercase();
    let (dirs, name) = match lower.rsplit_once('/') {
        Some((d, n)) => (d, n),
        None => ("", lower.as_str()),
    };
    let segments: Vec<&str> = dirs.split('/').filter(|s| !s.is_empty()).collect();
    let has = |set: &[&str]| segments.iter().any(|s| set.contains(s));
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);

    let mut worst = Noise::None;
    let mut mark = |n: Noise| worst = worst.max(n);

    let avg_line = bytes / lines.max(1);
    // One enormous line is a blob (data, a bundle); a long file of normal lines is not.
    if bytes > 2 << 20 || (longest_line > 4096 && lines < 50) {
        mark(Noise::Huge);
    }
    let script = matches!(ext, "js" | "mjs" | "cjs" | "css");
    if name.contains(".min.") || (script && avg_line > 110) {
        mark(Noise::Minified);
    }
    if LOCKFILES.contains(&name) {
        mark(Noise::Lockfile);
    }
    if is_generated(name, ext, &segments, head) {
        mark(Noise::Generated);
    }
    if has(VENDOR_DIRS) || lower.contains("wwwroot/lib/") {
        mark(Noise::Vendored);
    }
    if has(BUILD_DIRS) {
        mark(Noise::Build);
    }
    if has(FIXTURE_DIRS) || ext == "snap" {
        mark(Noise::Fixtures);
    }
    let original_name = rel.rsplit_once('/').map_or(rel, |(_, n)| n);
    if has(TEST_DIRS) || is_test_name(original_name) {
        mark(Noise::Tests);
    }
    worst
}

const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
    "cargo.lock",
    "composer.lock",
    "poetry.lock",
    "uv.lock",
    "pipfile.lock",
    "gemfile.lock",
    "flake.lock",
    "deno.lock",
    "packages.lock.json",
    "podfile.lock",
    "pubspec.lock",
    "go.sum",
];

const VENDOR_DIRS: &[&str] = &[
    "node_modules",
    "vendor",
    "vendors",
    "third_party",
    "thirdparty",
    "3rdparty",
    "bower_components",
    "pods",
    ".yarn",
    "site-packages",
];

const BUILD_DIRS: &[&str] = &["dist", "build", "out", "bin", "obj", "target", ".next", "coverage"];

const FIXTURE_DIRS: &[&str] = &["fixtures", "__fixtures__", "__snapshots__", "testdata"];

const TEST_DIRS: &[&str] = &["test", "tests", "__tests__", "spec", "specs"];

/// Test files by naming convention. Takes the name in its original case: `WidgetTest.java`
/// and `OrderTests.cs` mark the suffix with a capital, while `contest.ts` and
/// `greatest.rs` only end in the same letters.
#[allow(clippy::case_sensitive_file_extension_comparisons)] // The case is the signal.
fn is_test_name(name: &str) -> bool {
    let stem = name.split_once('.').map_or(name, |(s, _)| s);
    let lower = name.to_ascii_lowercase();
    let after_stem = &lower[stem.len()..];
    let lower_stem = stem.to_ascii_lowercase();
    lower_stem.ends_with("_test")
        || lower_stem.ends_with("-test")
        || lower_stem.ends_with("_tests")
        || lower_stem.ends_with("_spec")
        || stem.ends_with("Test")
        || stem.ends_with("Tests")
        || after_stem.contains(".test.")
        || after_stem.contains(".spec.")
        || lower_stem.starts_with("test_")
}

const GENERATED_MARKERS: &[&str] =
    &["@generated", "do not edit", "<auto-generated", "autogenerated", "auto-generated by", "this file is generated"];

#[allow(clippy::case_sensitive_file_extension_comparisons)] // Lowercased by `classify`.
fn is_generated(name: &str, ext: &str, segments: &[&str], head: &[u8]) -> bool {
    let by_name = name.ends_with(".designer.cs")
        || name.ends_with(".g.cs")
        || name.ends_with(".g.i.cs")
        || name.ends_with(".pb.go")
        || name.ends_with("_pb2.py")
        || name.ends_with(".pb.cc")
        || name.ends_with(".pb.h")
        || ext == "map"
        || segments.contains(&"__generated__")
        || segments.contains(&"generated")
        || segments.contains(&"migrations") && name.ends_with(".cs") && name.contains('_');
    if by_name {
        return true;
    }
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();
    GENERATED_MARKERS.iter().any(|m| head.contains(m))
}

/// Keywords that open a definition in the languages this corpus holds (Rust, TS/JS, C#,
/// Python, Go, C/C++, `GDScript`, Lua, Java, Kotlin).
const DEF_KEYWORDS: &[&str] = &[
    "fn",
    "def",
    "class",
    "struct",
    "enum",
    "interface",
    "trait",
    "type",
    "func",
    "fun",
    "function",
    "impl",
    "module",
    "namespace",
    "record",
    "object",
    "const",
    "let",
    "var",
    "val",
    "macro_rules!",
    "#define",
    "signal",
    "local",
];

/// Words that may precede a definition keyword or a typed C-family member.
const MODIFIERS: &[&str] = &[
    "export",
    "default",
    "pub",
    "pub(crate)",
    "pub(super)",
    "public",
    "private",
    "protected",
    "internal",
    "static",
    "async",
    "abstract",
    "sealed",
    "partial",
    "override",
    "virtual",
    "final",
    "readonly",
    "declare",
    "unsafe",
    "extern",
    "inline",
    "const",
    "data",
    "open",
];

/// Words that open statements, never a typed definition: `return foo(x)` is a call.
const CONTROL: &[&str] = &[
    "return", "if", "else", "while", "for", "switch", "case", "new", "await", "throw", "yield", "delete", "typeof",
    "goto", "sizeof", "do", "try", "catch", "using", "import", "from", "package", "include", "require", "echo",
    "print", "not", "and", "or", "in", "of", "assert", "raise", "with", "elif", "match", "when", "go", "defer",
];

/// A definition keyword, also in its generator form (`function*`).
fn is_keyword(word: &str) -> bool {
    DEF_KEYWORDS.contains(&word.strip_suffix('*').unwrap_or(word))
}

/// The byte range of the name a line defines, if it looks like a definition: zoekt
/// boosts matches on ctags symbols; this is the cheap approximation of that
/// (docs/ranking.md). A match only counts as a definition hit when it overlaps this name,
/// so `const w = makeWidget(3)` defines `w`, not `makeWidget`. Heuristic by design: no
/// per-language parser, so it misses some shapes (see the `definitions` test for what it
/// covers) and only ever adds a bonus, never removes a file.
#[must_use]
pub fn defined_name(line: &[u8]) -> Option<std::ops::Range<usize>> {
    let mut pos = skip_space(line, 0);
    let mut modified = false;
    loop {
        let (start, end) = token(line, pos)?;
        let word = std::str::from_utf8(&line[start..end]).ok()?;
        // `const` is both: a modifier in C++/C# (`public const int X`), a keyword in JS.
        if is_keyword(word) {
            if let Some(name) = keyword_name(line, word, end) {
                return Some(name);
            }
            if !MODIFIERS.contains(&word) {
                return None;
            }
        }
        if !MODIFIERS.contains(&word) {
            break;
        }
        modified = true;
        pos = skip_space(line, end);
    }
    typed_name(line, pos, modified)
}

/// The name after a definition keyword that ends at `end`, or `None` when the keyword
/// is a modifier here (`const int X` names a type first).
fn keyword_name(line: &[u8], word: &str, end: usize) -> Option<std::ops::Range<usize>> {
    let mut at = skip_space(line, end);
    // Go methods: `func (r *Recv) Name(`.
    if line.get(at) == Some(&b'(') {
        at = skip_space(line, line[at..].iter().position(|&c| c == b')')? + at + 1);
    }
    // `impl Display for Foo` implements for Foo.
    if word == "impl"
        && let Some(f) = find(line, b" for ")
    {
        at = skip_space(line, f + 5);
    }
    let mut name = ident(line, at);
    // `let mut x`, `local function f`, `export const async`…: the word after the keyword
    // can itself be a keyword or modifier; the name is the next identifier.
    while !name.is_empty() {
        let w = std::str::from_utf8(&line[name.clone()]).ok()?;
        if w != "mut" && !is_keyword(w) && !MODIFIERS.contains(&w) {
            break;
        }
        name = ident(line, skip_space(line, name.end));
    }
    if name.is_empty() {
        return None;
    }
    // `function M.update`, `function M:update`: the method, not the table.
    while matches!(line.get(name.end), Some(b'.' | b':')) && line.get(name.end + 1).is_some_and(|&c| is_ident(c)) {
        name = ident(line, name.end + 1);
    }
    // A modifier-keyword followed by a second identifier (`const int X`) named a type.
    let typed = MODIFIERS.contains(&word) && line.get(skip_space(line, name.end)).is_some_and(|&c| is_ident(c));
    (!typed).then_some(name)
}

/// A C-family typed definition starting at `pos` (after any modifiers): `int count = 0;`
/// (modified only), `logger: Logger = …`, `Task<Order> GetOrder(int id)`,
/// `void parse(int x) {`.
fn typed_name(line: &[u8], pos: usize, modified: bool) -> Option<std::ops::Range<usize>> {
    let (first_start, first_end) = token(line, pos)?;
    let first = std::str::from_utf8(&line[first_start..first_end]).ok()?;
    let first_word = &first[..first.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(first.len())];
    if CONTROL.contains(&first_word) || !is_ident(line[first_start]) {
        return None;
    }
    let paren = line[pos..].iter().position(|&c| c == b'(').map(|p| p + pos);
    let assign = line[pos..].iter().position(|&c| c == b'=').map(|a| a + pos).filter(|&a| paren.is_none_or(|p| a < p));
    if let Some(a) = assign {
        // Unmodified `x = 1` is an assignment, not a definition.
        if !modified {
            return None;
        }
        // `name: Type = …` (TS fields, Rust statics): the name is before the colon.
        let before = line[pos..a].iter().position(|&c| c == b':').map_or(a, |c| c + pos);
        return name_before(line, pos, before);
    }
    let p = paren?;
    let trimmed = line.trim_ascii_end();
    // A `(` line ending in `;` is a call or a declaration, unless it is an
    // expression-bodied member (`Get(int id) => Ok();`).
    if trimmed.ends_with(b";") && find(&line[p..], b"=>").is_none() {
        return None;
    }
    // Unmodified lines must look like a C signature: `type name(`, ending in `{` or `)`.
    if !(modified || trimmed.ends_with(b"{") || trimmed.ends_with(b")") || trimmed.ends_with(b") const")) {
        return None;
    }
    let name = name_before(line, pos, p)?;
    // A type must come before the name: `connect(mapState)` after `export default` is a call.
    let has_type = line[pos..name.start].iter().any(|&c| is_ident(c));
    // Unmodified, the name must be the second token (`void parse(`), so prose such as
    // `// call foo(x)` is not a definition.
    let second =
        !modified && token(line, skip_space(line, first_end)).is_some_and(|(s, _)| s <= name.start && name.end <= p);
    (has_type && (modified || second)).then_some(name)
}

/// The identifier ending right before `at` (spaces and pointer sigils skipped), if it
/// starts at or after `pos`.
fn name_before(line: &[u8], pos: usize, at: usize) -> Option<std::ops::Range<usize>> {
    let end = line[pos..at].iter().rposition(|&c| !c.is_ascii_whitespace())? + pos + 1;
    let start = line[pos..end].iter().rposition(|&c| !is_ident(c)).map_or(pos, |i| i + pos + 1);
    (start < end).then_some(start..end)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn skip_space(line: &[u8], mut at: usize) -> usize {
    while line.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
}

/// The whitespace-delimited token starting at `at`.
fn token(line: &[u8], at: usize) -> Option<(usize, usize)> {
    let len =
        line[at.min(line.len())..].iter().position(u8::is_ascii_whitespace).unwrap_or(line.len() - at.min(line.len()));
    (len > 0).then_some((at, at + len))
}

/// The identifier starting at `at` (possibly empty), past pointer and reference sigils.
fn ident(line: &[u8], at: usize) -> std::ops::Range<usize> {
    let mut start = at;
    while line.get(start).is_some_and(|&c| c == b'*' || c == b'&') {
        start += 1;
    }
    let mut end = start;
    while line.get(end).is_some_and(|&c| is_ident(c)) {
        end += 1;
    }
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, bytes: u64, noise: Noise, terms: Vec<TermHits>) -> FileFeatures {
        FileFeatures { path: path.into(), bytes, noise, content_hash: 0, terms }
    }

    fn hits(matches: u32, word: u32, def: u32) -> TermHits {
        TermHits { matches, word, def, basename: false, path: false }
    }

    #[test]
    fn noise_classes() {
        assert_eq!(classify("src/app.ts", b"import x", 2_000, 50, 80), Noise::None);
        assert_eq!(classify("node_modules/react/index.js", b"", 2_000, 50, 80), Noise::Vendored);
        assert_eq!(classify("web/package-lock.json", b"{", 2_000, 50, 80), Noise::Lockfile);
        assert_eq!(classify("dist/app.min.js", b"", 2_000, 1, 2_000), Noise::Minified);
        assert_eq!(classify("bundle.js", b"", 300_000, 3, 200_000), Noise::Huge);
        assert_eq!(classify("api/Models/Foo.Designer.cs", b"", 2_000, 50, 80), Noise::Generated);
        assert_eq!(classify("gen/types.ts", b"// @generated by tool\n", 2_000, 50, 80), Noise::Generated);
        assert_eq!(classify("src/foo.test.ts", b"", 2_000, 50, 80), Noise::Tests);
        assert_eq!(classify("tests/integration.rs", b"", 2_000, 50, 80), Noise::Tests);
        assert_eq!(classify("src/__snapshots__/a.snap", b"", 2_000, 50, 80), Noise::Fixtures);
        assert_eq!(classify("api/bin/Debug/x.json", b"", 2_000, 50, 80), Noise::Build);
        assert_eq!(classify("src/latest.ts", b"", 2_000, 50, 80), Noise::None, "latest is not a test");
        for name in ["contest.ts", "greatest.rs", "contests.ts", "shortest.py"] {
            assert_eq!(classify(&format!("src/{name}"), b"", 2_000, 50, 80), Noise::None, "{name}");
        }
        for name in
            ["WidgetTest.java", "OrderTests.cs", "widget_test.go", "test_widget.py", "w.spec.ts", "api.client.test.ts"]
        {
            assert_eq!(classify(&format!("src/{name}"), b"", 2_000, 50, 80), Noise::Tests, "{name}");
        }
        // Migrations at any depth, the top level included.
        assert_eq!(classify("Migrations/20200101_Init.cs", b"", 2_000, 50, 80), Noise::Generated);
        assert_eq!(classify("Api/Data/Migrations/20200101_Init.cs", b"", 2_000, 50, 80), Noise::Generated);
        assert_eq!(classify("Api/MigrationsOld/2020_Init.cs", b"", 2_000, 50, 80), Noise::None);
        // The most severe class wins.
        assert_eq!(classify("node_modules/x/dist/x.min.js", b"", 2_000, 1, 2_000), Noise::Minified);
    }

    #[test]
    fn definitions() {
        let name = |line: &str| defined_name(line.as_bytes()).map(|r| line[r].to_string());
        for (line, defined) in [
            ("fn parse_args() {", "parse_args"),
            ("pub fn new() -> Self {", "new"),
            ("export default function App() {", "App"),
            ("export const getProduct = async () => {", "getProduct"),
            ("    def run(self):", "run"),
            ("public class OrderService : IOrderService", "OrderService"),
            ("    public async Task<Order> GetOrder(int id)", "GetOrder"),
            ("    public const int MaxPlayers = 64;", "MaxPlayers"),
            ("    private static readonly Logger Log = new Logger();", "Log"),
            ("    private readonly logger: Logger = new Logger();", "logger"),
            ("pub static FOO: u32 = 0;", "FOO"),
            ("type Props = {", "Props"),
            ("struct Soldier {", "Soldier"),
            ("#define MAX_PLAYERS 64", "MAX_PLAYERS"),
            ("func (s *Server) Handle(w http.ResponseWriter) {", "Handle"),
            ("const w = makeWidget(3);", "w"),
            ("let mut count = 0;", "count"),
            ("local function helper()", "helper"),
            ("function M.update(dt)", "update"),
            ("function M:draw()", "draw"),
            ("impl Display for Foo {", "Foo"),
            ("impl Foo {", "Foo"),
            ("void parse(int x) {", "parse"),
            ("int main(void)", "main"),
            ("static char *read_line(FILE *f) {", "read_line"),
            ("fun main() {", "main"),
            ("function* gen() {", "gen"),
            ("    public Order Get(int id) => Ok();", "Get"),
        ] {
            assert_eq!(name(line).as_deref(), Some(defined), "{line}");
        }
        for line in [
            "    return getProduct(id);",
            "    await service.GetOrder(id);",
            "// fn in a comment",
            "// call foo(x)",
            "x = 1",
            "x = foo(1)",
            "export default connect(mapState)(App)",
            "    foo(bar);",
            "if (ready) {",
            "} else if (x) {",
            "We call makeWidgets in a loop.",
            "    public abstract Order Get(int id);",
            "fn",
            "func (",
            "(",
            "",
        ] {
            assert_eq!(name(line), None, "{line}");
        }
        assert_eq!(defined_name(b"\xff\xfe fn x"), None, "non-UTF-8 first token");
    }

    #[test]
    fn saturation_keeps_a_data_dump_below_a_definition() {
        let w = Weights::default();
        let files = vec![
            file("src/service.ts", 4_000, Noise::None, vec![hits(3, 3, 1)]),
            file("data/dump.json", 400_000, Noise::None, vec![hits(5_000, 5_000, 0)]),
        ];
        let c = Corpus::of(&files, 1, 10_000);
        assert_eq!(order(&files, &c, &w), [0, 1]);
    }

    #[test]
    fn noise_demotes_but_never_hides_a_strong_match() {
        let w = Weights::default();
        let mut vendored_def = hits(1, 1, 1);
        vendored_def.basename = true;
        let files = vec![
            file("node_modules/lib/thing.js", 3_000, Noise::Vendored, vec![vendored_def]),
            file("src/a.ts", 3_000, Noise::None, vec![hits(1, 0, 0)]),
            file("node_modules/lib/other.js", 3_000, Noise::Vendored, vec![hits(1, 1, 0)]),
        ];
        let c = Corpus::of(&files, 1, 10_000);
        let ranked = order(&files, &c, &w);
        assert_eq!(ranked.last(), Some(&2), "a weak vendored hit sinks below main code");
        assert_eq!(ranked[0], 0, "a vendored definition named like the query still leads");
    }

    #[test]
    fn covering_more_terms_beats_repeating_one() {
        let w = Weights::default();
        let files = vec![
            file("a.rs", 3_000, Noise::None, vec![hits(6, 6, 0), hits(0, 0, 0), hits(0, 0, 0)]),
            file("b.rs", 3_000, Noise::None, vec![hits(1, 1, 0), hits(1, 1, 0), hits(1, 1, 0)]),
        ];
        let c = Corpus::of(&files, 3, 10_000);
        assert_eq!(order(&files, &c, &w), [1, 0]);
    }

    #[test]
    fn rare_terms_weigh_more() {
        let w = Weights::default();
        let mut files = vec![
            file("rare.rs", 3_000, Noise::None, vec![hits(0, 0, 0), hits(1, 1, 0)]),
            file("common.rs", 3_000, Noise::None, vec![hits(1, 1, 0), hits(0, 0, 0)]),
        ];
        // Many more files with the first term, so it is the common one.
        for i in 0..50 {
            files.push(file(&format!("f{i}.rs"), 3_000, Noise::None, vec![hits(1, 0, 0), hits(0, 0, 0)]));
        }
        let c = Corpus::of(&files, 2, 10_000);
        let ranked = order(&files, &c, &w);
        assert_eq!(ranked[0], 0);
    }

    #[test]
    fn copies_move_behind_every_first_copy_and_none_is_dropped() {
        let with_hash = |path: &str, h: u64| FileFeatures { content_hash: h, ..file(path, 100, Noise::None, vec![]) };
        let files = vec![
            with_hash("a", 1),
            with_hash("fork/a", 1),
            with_hash("b", 2),
            with_hash("fork/b", 2),
            with_hash("c", 3),
        ];
        assert_eq!(collapse_copies(&files, vec![0, 1, 2, 3, 4]), [0, 2, 4, 1, 3]);
    }
}
