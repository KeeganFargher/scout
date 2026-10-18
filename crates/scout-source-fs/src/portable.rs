//! The `os` surface for platforms without a change journal (everything but Windows, so
//! far). Same functions and types as `win.rs`, so no other module needs a `cfg`.
//!
//! [`Journal`] is uninhabited: `open` always reports [`io::ErrorKind::Unsupported`], the
//! context treats that like a FAT volume, and every refresh scans. The type system then
//! proves no journal method is ever reached here.

use std::io;
use std::path::Path;

use crate::dirs::{Entry, Kind};

pub(crate) fn read_dir(path: &Path) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let kind = if ft.is_symlink() {
            Kind::Other
        } else if ft.is_dir() {
            Kind::Dir
        } else {
            Kind::File
        };
        // Only files need a stat; directories are described by their own listing.
        let (size, mtime_ns) = if kind == Kind::File {
            let m = entry.metadata()?;
            (m.len(), crate::mtime_ns(&m))
        } else {
            (0, 0)
        };
        let os = entry.file_name();
        let lossy = os.to_str().is_none();
        out.push(Entry { name: os.to_string_lossy().into_owned(), lossy, kind, id: 0, size, mtime_ns });
    }
    Ok(out)
}

/// Ids only matter to the journal, and there is none here.
#[allow(clippy::unnecessary_wraps, reason = "same signature as win.rs, which can fail")]
pub(crate) fn file_id(_: &Path) -> io::Result<u128> {
    Ok(0)
}

/// Feeds are cached by volume. With no journal every volume's feed is the same absent
/// one, so a single key is exact rather than an approximation.
#[allow(clippy::unnecessary_wraps, reason = "same signature as win.rs, which can fail")]
pub(crate) fn volume_of(_: &Path) -> io::Result<String> {
    Ok(String::new())
}

pub(crate) fn is_unsupported(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Unsupported
}

pub(crate) enum Journal {}

impl Journal {
    pub(crate) fn open(_: &str) -> io::Result<Self> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no change journal on this platform"))
    }

    pub(crate) fn id(&self) -> u64 {
        match *self {}
    }

    pub(crate) fn next(&self) -> i64 {
        match *self {}
    }

    pub(crate) fn lowest_valid(&self) -> i64 {
        match *self {}
    }

    pub(crate) fn read(&self, _: i64, _: i64, _: impl FnMut(i64, u128, u128, bool)) -> io::Result<()> {
        match *self {}
    }
}

/// Memory this process may use, in bytes: `MemTotal` from Linux's `/proc/meminfo`, capped by
/// every cgroup memory limit from the process's own cgroup up to the root. Containers
/// (Docker, Kubernetes) and systemd units limit memory that way while `/proc/meminfo` still
/// shows the host's RAM, and a budget planned from the host's RAM gets the indexer OOM-killed.
/// No FFI needed. Other Unixes have no `/proc/meminfo` and get the open error: pass
/// `--memory-mb` there.
pub(crate) fn usable_memory() -> io::Result<u64> {
    let total = parse_mem_total(&std::fs::read_to_string("/proc/meminfo")?)?;
    let own = std::fs::read_to_string("/proc/self/cgroup")?;
    let limit = cgroup_memory_limit(Path::new("/sys/fs/cgroup"), &own)?;
    Ok(limit.map_or(total, |l| total.min(l)))
}

/// The tightest memory limit on the path from the process's cgroup to the root of the
/// hierarchy that holds the memory controller, or `None` when nothing on it is limited.
/// `cgroup_root` is where the hierarchies are mounted (`/sys/fs/cgroup`).
///
/// A controller lives in exactly one hierarchy. A v1 line naming `memory` (v1-only hosts,
/// and hybrid ones such as Amazon Linux 2 where the unified `0::` line exists too but holds
/// no memory controller) wins and its limits are read as `memory/<path>/memory.limit_in_bytes`;
/// otherwise the unified line's `<path>/memory.max`. With neither, no memory controller is
/// mounted and nothing can limit memory.
fn cgroup_memory_limit(cgroup_root: &Path, proc_self_cgroup: &str) -> io::Result<Option<u64>> {
    let mut unified = None;
    for line in proc_self_cgroup.lines() {
        let mut fields = line.splitn(3, ':');
        let (Some(id), Some(controllers), Some(path)) = (fields.next(), fields.next(), fields.next()) else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("/proc/self/cgroup line {line:?}")));
        };
        if controllers.split(',').any(|c| c == "memory") {
            return tightest_limit(&cgroup_root.join("memory"), path, "memory.limit_in_bytes");
        }
        if id == "0" && controllers.is_empty() {
            unified = Some(path);
        }
    }
    match unified {
        Some(path) => tightest_limit(cgroup_root, path, "memory.max"),
        None => Ok(None),
    }
}

/// Walks from `mount/<own>` up to `mount`, keeping the smallest `file`.
fn tightest_limit(mount: &Path, own: &str, file: &str) -> io::Result<Option<u64>> {
    let rel = Path::new(own.trim().trim_start_matches('/'));
    // A process in a nested cgroup namespace sees its cgroup as `/../x`: the path is
    // relative to a root this mount does not show, so the limits read would be someone
    // else's.
    if rel.components().any(|c| c == std::path::Component::ParentDir) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cgroup {own:?} is outside this namespace's mount; pass --memory-mb"),
        ));
    }
    let mut dir = mount.join(rel);
    // The process's own cgroup must be there. When it is not (a container sharing the
    // host's cgroup namespace but mounting its own cgroupfs), every limit would read as
    // absent and the host's RAM would win: the OOM this function exists to prevent.
    std::fs::metadata(&dir)
        .map_err(|e| io::Error::new(e.kind(), format!("cgroup directory {}: {e}; pass --memory-mb", dir.display())))?;
    let mut tightest: Option<u64> = None;
    loop {
        match std::fs::read_to_string(dir.join(file)) {
            Ok(s) => {
                if let Some(v) = parse_memory_max(&s)? {
                    tightest = Some(tightest.map_or(v, |t| t.min(v)));
                }
            }
            // Inside a directory that exists, a missing file limits nothing: the root cgroup
            // has no memory.max, and v2 only creates it where the parent enabled the
            // controller.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if dir == mount || !dir.pop() || !dir.starts_with(mount) {
            break;
        }
    }
    Ok(tightest)
}

/// A `memory.max` file: `max` (unlimited) or a byte count. v1's `memory.limit_in_bytes` is
/// always a byte count (unlimited is a page-rounded `i64::MAX`, which `min` then ignores).
fn parse_memory_max(s: &str) -> io::Result<Option<u64>> {
    match s.trim() {
        "max" => Ok(None),
        v => v
            .parse::<u64>()
            .map(Some)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("memory.max is {v:?}"))),
    }
}

/// The `MemTotal:  16314536 kB` line of `/proc/meminfo`, in bytes.
fn parse_mem_total(meminfo: &str) -> io::Result<u64> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "/proc/meminfo has no MemTotal in kB");
    let line = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:")).ok_or_else(bad)?;
    let kib = line.trim().strip_suffix("kB").ok_or_else(bad)?.trim().parse::<u64>().map_err(|_| bad())?;
    Ok(kib * 1024)
}

/// Not measured off Windows yet: the counters need the process before it is reaped, and no
/// portable std API reads them.
pub(crate) fn peak_memory(_child: &std::process::Child) -> io::Result<(u64, u64)> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(test)]
mod tests {
    use super::{cgroup_memory_limit, parse_mem_total, parse_memory_max};

    #[test]
    fn mem_total() {
        let meminfo = "MemTotal:       16314536 kB\nMemFree:         1234 kB\n";
        assert_eq!(parse_mem_total(meminfo).unwrap(), 16_314_536 * 1024);
        // A missing line or a unit other than kB is an error, never a guess.
        assert!(parse_mem_total("MemFree: 1 kB\n").is_err());
        assert!(parse_mem_total("MemTotal: 16 MB\n").is_err());
    }

    #[test]
    fn cgroup_limit_is_the_tightest_on_the_path_to_the_root() {
        let mount = tempfile::tempdir().unwrap();
        let scope = mount.path().join("user.slice/app.slice/run.scope");
        std::fs::create_dir_all(&scope).unwrap();
        // The root has no memory.max; the slice limits to 4 GiB, the scope to 2 GiB.
        std::fs::write(mount.path().join("user.slice/memory.max"), "4294967296\n").unwrap();
        std::fs::write(mount.path().join("user.slice/app.slice/memory.max"), "max\n").unwrap();
        std::fs::write(scope.join("memory.max"), "2147483648\n").unwrap();
        let own = "0::/user.slice/app.slice/run.scope\n";
        assert_eq!(cgroup_memory_limit(mount.path(), own).unwrap(), Some(2 << 30));

        // A container sees itself at the root of its namespace; its limit is the root's file.
        let container = tempfile::tempdir().unwrap();
        std::fs::write(container.path().join("memory.max"), "1073741824").unwrap();
        assert_eq!(cgroup_memory_limit(container.path(), "0::/\n").unwrap(), Some(1 << 30));

        // Unlimited everywhere.
        std::fs::write(scope.join("memory.max"), "max").unwrap();
        std::fs::write(mount.path().join("user.slice/memory.max"), "max").unwrap();
        assert_eq!(cgroup_memory_limit(mount.path(), own).unwrap(), None);

        assert!(parse_memory_max("lots").is_err(), "garbage is an error, never unlimited");
    }

    #[test]
    fn cgroup_v1_memory_controller_wins_over_the_unified_line() {
        // A hybrid host: the unified line exists but the memory controller is in v1.
        let root = tempfile::tempdir().unwrap();
        let svc = root.path().join("memory/system.slice/scout.service");
        std::fs::create_dir_all(&svc).unwrap();
        std::fs::write(root.path().join("memory/memory.limit_in_bytes"), "9223372036854771712\n").unwrap();
        std::fs::write(svc.join("memory.limit_in_bytes"), "2147483648\n").unwrap();
        // Named v1 hierarchies (`name=systemd`) carry no controller and are skipped.
        let own = "12:pids:/system.slice/scout.service\n\
                   1:name=systemd:/system.slice/scout.service\n\
                   4:memory:/system.slice/scout.service\n\
                   0::/system.slice/scout.service\n";
        assert_eq!(cgroup_memory_limit(root.path(), own).unwrap(), Some(2 << 30));
        // No memory controller mounted anywhere: nothing can limit memory.
        assert_eq!(cgroup_memory_limit(root.path(), "3:cpu,cpuacct:/x\n").unwrap(), None);
    }

    #[test]
    fn cgroup_paths_the_mount_cannot_show_are_errors() {
        let root = tempfile::tempdir().unwrap();
        // The host's cgroup namespace path, under a container's own cgroupfs mount.
        let err = cgroup_memory_limit(root.path(), "0::/system.slice/docker-1.scope\n").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("--memory-mb"), "{err}");
        // A nested cgroup namespace.
        let err = cgroup_memory_limit(root.path(), "0::/../sibling\n").unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
        assert!(cgroup_memory_limit(root.path(), "garbage\n").is_err());
    }
}
