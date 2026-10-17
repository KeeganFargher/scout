//! Host introspection used to adapt to the machine: cores and physical memory.

use std::num::NonZeroUsize;

#[must_use]
pub fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

/// Total physical memory in bytes.
#[cfg(windows)]
pub fn physical_memory() -> anyhow::Result<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX { dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>())?, ..Default::default() };
    // SAFETY: `status` is a properly sized, initialized MEMORYSTATUSEX with dwLength set,
    // which is the documented contract of GlobalMemoryStatusEx.
    #[allow(unsafe_code)]
    let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
    anyhow::ensure!(ok != 0, "GlobalMemoryStatusEx failed: {}", std::io::Error::last_os_error());
    Ok(status.ullTotalPhys)
}

/// Total physical memory in bytes.
#[cfg(unix)]
pub fn physical_memory() -> anyhow::Result<u64> {
    // SAFETY: sysconf has no preconditions; it returns -1 on error, checked below.
    #[allow(unsafe_code)]
    let (pages, page_size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
    anyhow::ensure!(pages > 0 && page_size > 0, "sysconf could not report physical memory");
    Ok(u64::try_from(pages)? * u64::try_from(page_size)?)
}
