//! Killing git together with everything it spawned. A fetch over http runs
//! git → `git-remote-https` → `fetch-pack` → `index-pack`; killing only git leaves the rest
//! writing into the working copy while the next cycle clears its locks and starts again.
//!
//! The one per-OS seam of this crate, kept in this file: Unix puts git in its own process
//! group and kills the group; Windows kills the tree by parent links (`taskkill /T`).

use std::process::{Child, Command};

use anyhow::Context;

/// Prepares `cmd` so [`kill`] can reach every process it starts.
#[cfg(unix)]
pub fn isolate(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Its own group, led by git: helpers inherit it, so one signal reaches them all.
    cmd.process_group(0);
}

/// Prepares `cmd` so [`kill`] can reach every process it starts.
#[cfg(windows)]
pub fn isolate(_cmd: &mut Command) {
    // taskkill /T follows parent links; nothing to set up at spawn.
}

#[cfg(unix)]
fn kill_descendants(child: &Child) -> anyhow::Result<()> {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let pgid = Pid::from_raw(i32::try_from(child.id()).context("pid out of range")?);
    match killpg(pgid, Signal::SIGKILL) {
        // The whole group already exited: nothing left to kill.
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(e) => Err(e).context("killing git's process group"),
    }
}

#[cfg(windows)]
fn kill_descendants(child: &Child) -> anyhow::Result<()> {
    let out = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .output()
        .context("running taskkill")?;
    anyhow::ensure!(
        out.status.success(),
        "taskkill failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Kills `child` and its descendants and reaps it. git itself is always killed and
/// waited for, whatever happened to the helpers; a failure to reach them is still
/// reported, after.
pub fn kill(child: &mut Child) -> anyhow::Result<()> {
    let descendants = kill_descendants(child);
    if child.try_wait().context("checking git")?.is_none() {
        child.kill().context("killing git")?;
    }
    child.wait().context("reaping git")?;
    descendants
}
