//! Windows FFI: directory enumeration with file ids, the NTFS/ReFS change journal, a
//! child process's peak memory (for scout-bench), and usable memory (the indexing budget).
//!
//! All `unsafe` in this crate lives here. Every block calls one documented Win32 API with
//! buffers sized and aligned for it, and every record the kernel returns is bounds-checked
//! against the bytes it reported before it is read.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_ID_128, FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FileBasicInfo, FileIdExtdDirectoryInfo, FileIdInfo,
    GetFileInformationByHandleEx, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_UNPRIVILEGED_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V2,
    USN_RECORD_V3,
};

use crate::dirs::{Entry, Kind};

/// Time between 1601-01-01 (the FILETIME epoch) and 1970-01-01, in 100 ns intervals.
/// USN reasons that change *where* an existing file or directory is. Decoded here so
/// the shared journal code never sees Windows reason bits.
const STRUCTURAL: u32 = 0x0000_0200 /* FILE_DELETE */ | 0x0000_1000 /* RENAME_OLD_NAME */ | 0x0000_2000 /* RENAME_NEW_NAME */;
const UNIX_EPOCH_INTERVALS: i64 = 116_444_736_000_000_000;

/// Reparse tags with this bit are "name surrogates" (symlinks, junctions). std treats
/// exactly these as symlinks, and so does the walker, which never follows them.
const NAME_SURROGATE: u32 = 0x2000_0000;

/// Same conversion std applies to a FILETIME, so ids, sizes and mtimes read here equal
/// what `std::fs::Metadata` reports for the same file (tested).
fn filetime_ns(t: i64) -> u64 {
    if t <= UNIX_EPOCH_INTERVALS {
        return 0;
    }
    u64::try_from(t - UNIX_EPOCH_INTERVALS).map_or(0, |v| v.saturating_mul(100))
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn open(path: &Path, access: u32, flags: u32) -> io::Result<OwnedHandle> {
    let w = wide(path);
    // SAFETY: `w` is NUL-terminated and outlives the call; null security attributes and a
    // null template handle are documented as allowed.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `h` is a valid handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// The 128-bit file id of `path` (a directory or file), as used by the change journal.
pub(crate) fn file_id(path: &Path) -> io::Result<u128> {
    let h = open(path, FILE_READ_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS)?;
    let mut info = FILE_ID_INFO { VolumeSerialNumber: 0, FileId: FILE_ID_128 { Identifier: [0; 16] } };
    // SAFETY: `info` is a FILE_ID_INFO and the size passed is exactly its size.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h.as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            u32::try_from(size_of::<FILE_ID_INFO>()).map_err(|_| invalid("FILE_ID_INFO size"))?,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u128::from_le_bytes(info.FileId.Identifier))
}

/// Lists `path` with one handle and 64 KiB batches. Measured at ~79 µs per directory
/// against ~88 µs for `std::fs::read_dir`, and it also returns each entry's file id,
/// which the change journal needs.
pub(crate) fn read_dir(path: &Path) -> io::Result<Vec<Entry>> {
    const NAME_OFFSET: usize = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
    let h = open(path, FILE_LIST_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS)?;
    // u64 elements: records are 8-byte aligned, and so must the buffer be.
    let mut buf = vec![0u64; 8 * 1024];
    let buf_bytes = u32::try_from(buf.len() * 8).map_err(|_| invalid("buffer size"))?;
    let mut out = Vec::new();
    loop {
        // SAFETY: `buf` is valid for writes of `buf_bytes` bytes and suitably aligned.
        let ok = unsafe {
            GetFileInformationByHandleEx(h.as_raw_handle(), FileIdExtdDirectoryInfo, buf.as_mut_ptr().cast(), buf_bytes)
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(ERROR_NO_MORE_FILES.cast_signed()) {
                return Ok(out);
            }
            // Opening a *file* with FILE_LIST_DIRECTORY succeeds (it is the same bit as
            // FILE_READ_DATA); only the listing fails, with a generic error. Say what it
            // is, so callers can treat a directory replaced by a file like a removed one.
            if out.is_empty() && !is_directory(&h)? {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("{} is not a directory", path.display()),
                ));
            }
            return Err(e);
        }
        // SAFETY: the kernel filled `buf_bytes` bytes of `buf`, which owns at least that many.
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf_bytes as usize) };
        let len = bytes.len();
        let mut off = 0usize;
        let mut name = Vec::new();
        loop {
            if off + NAME_OFFSET > len {
                return Err(invalid("directory record out of bounds"));
            }
            // An unaligned copy rather than a reference: the kernel does align records,
            // but relying on that would make soundness depend on undocumented layout,
            // and copying a ~100-byte header costs nothing next to the syscall.
            // SAFETY: the header lies within the filled bytes (checked above), and every
            // bit pattern is a valid FILE_ID_EXTD_DIR_INFO (plain integers).
            let info = unsafe { bytes.as_ptr().add(off).cast::<FILE_ID_EXTD_DIR_INFO>().read_unaligned() };
            let name_bytes = bytes
                .get(off + NAME_OFFSET..off + NAME_OFFSET + info.FileNameLength as usize)
                .ok_or_else(|| invalid("directory entry name out of bounds"))?;
            name.clear();
            name.extend(name_bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])));
            if name != [u16::from(b'.')] && name != [u16::from(b'.'), u16::from(b'.')] {
                out.push(entry(&info, &name));
            }
            if info.NextEntryOffset == 0 {
                break;
            }
            off += info.NextEntryOffset as usize;
        }
    }
}

fn entry(info: &FILE_ID_EXTD_DIR_INFO, name: &[u16]) -> Entry {
    let reparse = info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    let kind = if reparse && info.ReparsePointTag & NAME_SURROGATE != 0 {
        Kind::Other
    } else if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Kind::Dir
    } else {
        Kind::File
    };
    let (name, lossy) = match String::from_utf16(name) {
        Ok(s) => (s, false),
        Err(_) => (String::from_utf16_lossy(name), true),
    };
    Entry {
        name,
        lossy,
        kind,
        id: u128::from_le_bytes(info.FileId.Identifier),
        size: u64::try_from(info.EndOfFile).unwrap_or(0),
        mtime_ns: filetime_ns(info.LastWriteTime),
    }
}

fn is_directory(h: &OwnedHandle) -> io::Result<bool> {
    let mut info =
        FILE_BASIC_INFO { CreationTime: 0, LastAccessTime: 0, LastWriteTime: 0, ChangeTime: 0, FileAttributes: 0 };
    // SAFETY: `info` is a FILE_BASIC_INFO and the size passed is exactly its size.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h.as_raw_handle(),
            FileBasicInfo,
            (&raw mut info).cast(),
            u32::try_from(size_of::<FILE_BASIC_INFO>()).map_err(|_| invalid("FILE_BASIC_INFO size"))?,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0)
}

/// The volume `path` lives on, as a `\\?\Volume{GUID}\` name: stable across drive-letter
/// changes, and what the journal is opened through.
pub(crate) fn volume_of(path: &Path) -> io::Result<String> {
    let w = wide(path);
    let mut mount = vec![0u16; 1024];
    // SAFETY: `w` is NUL-terminated; `mount` is writable for the length passed.
    let ok = unsafe { GetVolumePathNameW(w.as_ptr(), mount.as_mut_ptr(), u32::try_from(mount.len()).unwrap_or(0)) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut guid = vec![0u16; 64];
    // SAFETY: `mount` is NUL-terminated (written above); `guid` is writable for its length.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(mount.as_ptr(), guid.as_mut_ptr(), u32::try_from(guid.len()).unwrap_or(0))
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let end = guid.iter().position(|&c| c == 0).unwrap_or(guid.len());
    String::from_utf16(&guid[..end]).map_err(|_| invalid("volume name is not UTF-16"))
}

/// An open change journal.
pub(crate) struct Journal {
    volume: OwnedHandle,
    id: u64,
    /// The next USN to be written when the journal was opened: everything before it is
    /// readable now, everything from it on happens "after" this process looked.
    next: i64,
    /// Records below this were overwritten (the journal is a fixed-size ring).
    lowest_valid: i64,
}

/// Why a volume has no usable journal. Anything else is a real error.
pub(crate) fn is_unsupported(e: &io::Error) -> bool {
    use windows_sys::Win32::Foundation::{
        ERROR_INVALID_FUNCTION, ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_NOT_ACTIVE, ERROR_NOT_SUPPORTED,
    };
    matches!(
        e.raw_os_error().and_then(|c| u32::try_from(c).ok()),
        Some(
            ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_JOURNAL_NOT_ACTIVE | ERROR_JOURNAL_DELETE_IN_PROGRESS
        )
    )
}

impl Journal {
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn next(&self) -> i64 {
        self.next
    }

    pub(crate) fn lowest_valid(&self) -> i64 {
        self.lowest_valid
    }

    /// Opens `volume` (a `\\?\Volume{GUID}\` name). `FILE_TRAVERSE` is enough for the
    /// unprivileged query and read FSCTLs, so this works without admin rights.
    pub(crate) fn open(volume: &str) -> io::Result<Self> {
        let device = volume.trim_end_matches('\\');
        let h = open(Path::new(device), FILE_TRAVERSE, 0)?;
        // SAFETY: zeroed is a valid bit pattern for this plain C struct.
        let mut data: USN_JOURNAL_DATA_V2 = unsafe { std::mem::zeroed() };
        let mut got = 0u32;
        // SAFETY: the output buffer is a USN_JOURNAL_DATA_V2 of the size passed; no input.
        let ok = unsafe {
            DeviceIoControl(
                h.as_raw_handle(),
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                (&raw mut data).cast(),
                u32::try_from(size_of::<USN_JOURNAL_DATA_V2>()).map_err(|_| invalid("journal data size"))?,
                &raw mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { volume: h, id: data.UsnJournalID, next: data.NextUsn, lowest_valid: data.LowestValidUsn })
    }

    /// Calls `f(usn, file_id, parent_id, structural)` for every record in `[from, to)`;
    /// `structural` means the entry itself was renamed or deleted, i.e. it moved.
    pub(crate) fn read(&self, from: i64, to: i64, mut f: impl FnMut(i64, u128, u128, bool)) -> io::Result<()> {
        const NAME_OFFSET: usize = std::mem::offset_of!(USN_RECORD_V3, FileName);
        let mut req = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: from,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: self.id,
            // V3 records carry 128-bit ids on every file system (NTFS zero-extends its
            // 64-bit ones), so ids compare equal to FILE_ID_INFO / directory listings.
            MinMajorVersion: 3,
            MaxMajorVersion: 3,
        };
        let mut buf = vec![0u64; 128 * 1024];
        let buf_bytes = u32::try_from(buf.len() * 8).map_err(|_| invalid("buffer size"))?;
        while req.StartUsn < to {
            let mut got = 0u32;
            // SAFETY: input is a READ_USN_JOURNAL_DATA_V1 of the size passed; output is
            // `buf`, writable for `buf_bytes` bytes and 8-aligned.
            let ok = unsafe {
                DeviceIoControl(
                    self.volume.as_raw_handle(),
                    FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
                    (&raw const req).cast(),
                    u32::try_from(size_of::<READ_USN_JOURNAL_DATA_V1>()).map_err(|_| invalid("request size"))?,
                    buf.as_mut_ptr().cast(),
                    buf_bytes,
                    &raw mut got,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let got = got as usize;
            if got < 8 {
                return Err(invalid("short journal read"));
            }
            // SAFETY: the kernel filled `got` bytes of `buf`, which owns at least that many.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), got) };
            let next = i64::from_le_bytes(bytes[..8].try_into().map_err(|_| invalid("short journal read"))?);
            let mut off = 8usize;
            while off < got {
                if off + NAME_OFFSET > got {
                    return Err(invalid("journal record out of bounds"));
                }
                // SAFETY: the header lies within the returned bytes (checked above), and
                // every bit pattern is a valid USN_RECORD_V3 (plain integers). Copied
                // unaligned for the same reason as directory records.
                let r = unsafe { bytes.as_ptr().add(off).cast::<USN_RECORD_V3>().read_unaligned() };
                if r.MajorVersion != 3 || (r.RecordLength as usize) < NAME_OFFSET || off + r.RecordLength as usize > got
                {
                    return Err(invalid("unexpected journal record"));
                }
                if r.Usn >= to {
                    return Ok(());
                }
                f(
                    r.Usn,
                    u128::from_le_bytes(r.FileReferenceNumber.Identifier),
                    u128::from_le_bytes(r.ParentFileReferenceNumber.Identifier),
                    r.Reason & STRUCTURAL != 0,
                );
                off += r.RecordLength as usize;
            }
            if next <= req.StartUsn {
                // No progress means the end of the journal.
                return Ok(());
            }
            req.StartUsn = next;
        }
        Ok(())
    }
}

/// Memory this process may use, in bytes: total physical memory. (A Job Object memory limit
/// is not read; scout is not run inside one.)
pub(crate) fn usable_memory() -> io::Result<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>()).map_err(|_| invalid("status size"))?,
        ..Default::default()
    };
    // SAFETY: `status` is a MEMORYSTATUSEX whose `dwLength` is its own size, which is the
    // documented contract of GlobalMemoryStatusEx; it writes only into that struct.
    let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(status.ullTotalPhys)
}

/// Peak working set and peak private commit of an exited, not yet reaped child, in bytes.
pub(crate) fn peak_memory(child: &std::process::Child) -> io::Result<(u64, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>()).map_err(|_| invalid("counters size"))?,
        ..Default::default()
    };
    // SAFETY: the child's handle stays valid while `child` is borrowed (Child owns it and
    // closes it on drop), and `c` is a PROCESS_MEMORY_COUNTERS whose `cb` is its own size,
    // which is the documented contract of GetProcessMemoryInfo.
    let ok = unsafe { GetProcessMemoryInfo(child.as_raw_handle(), &raw mut c, c.cb) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((c.PeakWorkingSetSize as u64, c.PeakPagefileUsage as u64))
}
