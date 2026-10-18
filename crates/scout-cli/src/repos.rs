//! Registering repos: explicit paths or discovery of git repos under a directory.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use scout_core::catalog::{Catalog, RepoEntry, sanitize_name};

/// Directories never worth descending into when looking for repos. They are huge and
/// never contain repos a user means to search.
const SKIP_DIRS: &[&str] = &["node_modules", "target", ".venv", "venv", "__pycache__", "dist", "build"];

/// Finds git repos under `dir` up to `max_depth`, without descending into a repo once
/// found: rg would search nested repos as part of the outer one anyway.
pub fn discover(dir: &Path, max_depth: usize) -> anyhow::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        if d.join(".git").exists() {
            found.push(d);
            continue;
        }
        if depth == max_depth {
            continue;
        }
        let entries = std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", d.display()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            if entry.file_type().with_context(|| format!("stat {}", entry.path().display()))?.is_dir() {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Registers `root`. Returns `false` if it was already registered or lies inside (or
/// contains) an already-registered repo, which would double-index files.
pub fn register(catalog: &mut Catalog, root: &Path) -> anyhow::Result<Option<String>> {
    let root = std::path::absolute(root).with_context(|| format!("resolving {}", root.display()))?;
    if !root.is_dir() {
        bail!("not a directory: {}", root.display());
    }
    let root_str = root.to_str().with_context(|| format!("path is not UTF-8: {}", root.display()))?.to_string();
    for r in &catalog.repos {
        let existing = Path::new(&r.root);
        if root.starts_with(existing) || existing.starts_with(&root) {
            return Ok(None);
        }
    }
    let base = sanitize_name(&root.file_name().map_or_else(|| "repo".into(), |n| n.to_string_lossy()));
    let mut name = base.clone();
    let mut n = 2;
    while catalog.find(&name).is_some() {
        name = format!("{base}-{n}");
        n += 1;
    }
    catalog.repos.push(RepoEntry { name: name.clone(), root: root_str, remote: None, index: None });
    Ok(Some(name))
}
