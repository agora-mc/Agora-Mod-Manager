use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use windows_sys::Win32::Foundation::HANDLE;

use crate::config::RuntimeConfig;
use crate::hooks::hooks;
use crate::nt::*;
use crate::paths::whited_out;
use crate::util::{guarded, lock_recover, log};

/// (offset of FileNameLength, offset of FileName) for the listing formats we merge.
pub fn name_offsets(class: u32) -> Option<(usize, usize)> {
    Some(match class {
        1 => (60, 64),   // FileDirectoryInformation
        2 => (60, 68),   // FileFullDirectoryInformation
        3 => (60, 94),   // FileBothDirectoryInformation
        12 => (8, 12),   // FileNamesInformation
        37 => (60, 104), // FileIdBothDirectoryInformation
        38 => (60, 80),  // FileIdFullDirectoryInformation
        60 => (60, 88),  // FileIdExtdDirectoryInformation
        63 => (60, 114), // FileIdExtdBothDirectoryInformation
        _ => return None,
    })
}

pub struct Listing {
    pub class: u32,
    pub entries: Vec<Vec<u8>>, // each entry's bytes, NextEntryOffset zeroed
    pub cursor: usize,
    pub returned_any: bool,
}

#[derive(Default)]
pub struct DirectoryScan {
    pattern: OnceLock<Option<String>>,
    listing: Mutex<Option<Listing>>,
}

static LISTINGS: OnceLock<Mutex<HashMap<usize, Arc<DirectoryScan>>>> = OnceLock::new();
pub fn listings() -> &'static Mutex<HashMap<usize, Arc<DirectoryScan>>> {
    LISTINGS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_listing<T>(
    slot: &Mutex<Option<Listing>>,
    class: u32,
    restart: bool,
    build: impl FnOnce() -> Vec<Vec<u8>>,
    consume: impl FnOnce(&mut Listing) -> T,
) -> T {
    if !restart {
        let mut slot = lock_recover(slot);
        if let Some(listing) = slot.as_mut().filter(|listing| listing.class == class) {
            return consume(listing);
        }
    }

    // Enumerating real directories can block. The handle's slot survives a racing close without
    // repopulating the table for a reused handle number.
    let entries = build();
    let mut slot = lock_recover(slot);
    if restart || slot.as_ref().is_some_and(|listing| listing.class != class) {
        *slot = None;
    }
    let listing = slot.get_or_insert(Listing {
        class,
        entries,
        cursor: 0,
        returned_any: false,
    });
    consume(listing)
}

static UNMERGED_CLASSES: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
pub fn log_unmerged_class_once(class: u32, rel: &str) {
    let first =
        lock_recover(UNMERGED_CLASSES.get_or_init(|| Mutex::new(HashSet::new()))).insert(class);
    if first {
        log(format!(
            "listing class {class} not merged for {rel} (passed through)"
        ));
    }
}

pub fn entry_name(entry: &[u8], class: u32) -> String {
    let (len_at, name_at) = name_offsets(class).unwrap();
    let len = u32::from_le_bytes(entry[len_at..len_at + 4].try_into().unwrap()) as usize;
    let bytes = &entry[name_at..(name_at + len).min(entry.len())];
    let wide: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&wide)
}

/// Raw entries of one real directory, in `class` format.
pub unsafe fn read_real_dir(dir: &Path, class: u32) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut wide: Vec<u16> = "\\??\\"
        .encode_utf16()
        .chain(dir.as_os_str().encode_wide())
        .collect();
    let len = (wide.len() * 2) as u16;
    let mut name = UnicodeString {
        length: len,
        maximum_length: len,
        buffer: wide.as_mut_ptr(),
    };
    let mut oa = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: std::ptr::null_mut(),
        object_name: &mut name,
        attributes: 0x40, // OBJ_CASE_INSENSITIVE
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut handle: HANDLE = std::ptr::null_mut();
    // An IO_STATUS_BLOCK is two pointer-sized fields: 8 bytes on i686, 16 on x64.
    let mut iosb = [0usize; 2];
    let status = hooks().open.call(
        &mut handle,
        FILE_LIST_DIRECTORY | SYNCHRONIZE,
        &mut oa,
        iosb.as_mut_ptr() as *mut c_void,
        FILE_SHARE_ALL,
        FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
    );
    if status < 0 {
        return out;
    }
    let (len_at, name_at) = name_offsets(class).unwrap();
    let mut buf = vec![0u64; 64 * 1024 / 8];
    let mut first = 1u8;
    loop {
        let status = hooks().query_dir.call(
            handle,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            iosb.as_mut_ptr() as *mut c_void,
            buf.as_mut_ptr() as *mut c_void,
            (buf.len() * 8) as u32,
            class,
            0,
            std::ptr::null_mut(),
            first,
        );
        first = 0;
        if status < 0 || status == STATUS_NO_MORE_FILES {
            break;
        }
        let bytes = std::slice::from_raw_parts(buf.as_ptr() as *const u8, iosb[1]);
        let mut offset = 0usize;
        while offset + name_at <= bytes.len() {
            let next = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            let name_len = u32::from_le_bytes(
                bytes[offset + len_at..offset + len_at + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            let end = (offset + name_at + name_len).min(bytes.len());
            let mut entry = bytes[offset..end].to_vec();
            entry[0..4].copy_from_slice(&0u32.to_le_bytes());
            out.push(entry);
            if next == 0 {
                break;
            }
            offset += next;
        }
    }
    hooks().close.call(handle);
    out
}

/// Windows file-name matching, including the DOS wildcards FindFirstFile translates to.
pub fn wildcard(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) | (Some('<'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some('?'), Some(_)) | (Some('>'), Some(_)) => go(&p[1..], &n[1..]),
            (Some('"'), Some('.')) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) => {
                agora_game_api::fold_path_case(&a.to_string())
                    == agora_game_api::fold_path_case(&b.to_string())
                    && go(&p[1..], &n[1..])
            }
            (Some(_), None) => p.iter().all(|c| matches!(c, '*' | '<' | '>' | '"')),
            (None, Some(_)) => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

/// The VFS's own whiteout folder, matched through the same case fold as every other name.
fn is_whiteout_dir(folded_name: &str) -> bool {
    folded_name == ".agvfs-wh"
}

/// The merged listing of virtual directory `rel`: upper first, then lowers in priority order;
/// a name seen in a higher layer hides the same name below, whiteouts hide it entirely.
/// If `mount == lowers[0]` or any folders point to the exact same path, duplicates are de-duped.
pub unsafe fn build_listing(
    rel: &str,
    class: u32,
    pattern: Option<&str>,
    cfg: &RuntimeConfig,
) -> Vec<Vec<u8>> {
    let mut sources = Vec::new();
    let mut added_dirs: HashSet<PathBuf> = HashSet::new();

    let upper_dir = cfg.upper.join(rel);
    if added_dirs.insert(upper_dir.clone()) {
        sources.push(upper_dir);
    }

    for lower in &cfg.lowers {
        let dir = lower.join(rel);
        if added_dirs.insert(dir.clone()) {
            sources.push(dir);
        }
    }

    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for dir in sources {
        if !dir.is_dir() {
            continue;
        }
        for entry in read_real_dir(&dir, class) {
            let name = entry_name(&entry, class);
            let key = agora_game_api::fold_path_case(&name);
            if rel.is_empty() && is_whiteout_dir(&key) {
                continue;
            }
            if !seen.insert(key) {
                continue;
            }
            if name != "." && name != ".." {
                let child = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}\\{name}")
                };
                if whited_out(&child, &cfg.upper) {
                    continue;
                }
            }
            if let Some(p) = pattern {
                if !wildcard(p, &name) {
                    continue;
                }
            }
            entries.push(entry);
        }
    }
    entries
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn query_directory(
    handle: HANDLE,
    iosb: *mut c_void,
    out: *mut c_void,
    length: u32,
    class: u32,
    single: bool,
    pattern: *mut UnicodeString,
    restart: bool,
    cfg: &RuntimeConfig,
) -> Option<NTSTATUS> {
    if crate::util::is_inside() {
        return None;
    }
    let (tracked, slot) = {
        // Keep the handle table locked through slot acquisition, so NtClose cannot remove a
        // tracked handle and leave a newly created slot behind for the same number.
        let handles = lock_recover(crate::handles());
        let tracked = handles
            .get(&(handle as usize))
            .filter(|tracked| tracked.dir)?
            .clone();
        let slot = name_offsets(class).map(|_| {
            lock_recover(listings())
                .entry(handle as usize)
                .or_default()
                .clone()
        });
        (tracked, slot)
    };
    let Some(slot) = slot else {
        guarded(|| log_unmerged_class_once(class, &tracked.rel));
        return None;
    };
    // NtQueryDirectoryFile captures FileName on the first call for a handle. RestartScan
    // rewinds the scan without changing that search expression.
    // https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/nf-ntifs-ntquerydirectoryfile
    let pattern = slot
        .pattern
        .get_or_init(|| unicode(pattern).filter(|p| !p.is_empty() && p != "*"))
        .as_deref();
    guarded(|| {
        log(format!(
            "list {:?} class {class} single {single} restart {restart} pattern {pattern:?}",
            tracked.rel
        ))
    });
    guarded(|| {
        with_listing(
            &slot.listing,
            class,
            restart,
            || build_listing(&tracked.rel, class, pattern, cfg),
            |listing| write_listing(listing, iosb, out, length, single),
        )
    })
}

unsafe fn write_listing(
    listing: &mut Listing,
    iosb: *mut c_void,
    out: *mut c_void,
    length: u32,
    single: bool,
) -> NTSTATUS {
    let out_bytes = std::slice::from_raw_parts_mut(out as *mut u8, length as usize);
    let mut written = 0usize;
    let mut last_entry_at: Option<usize> = None;
    while listing.cursor < listing.entries.len() {
        let entry = &listing.entries[listing.cursor];
        let start = written.next_multiple_of(8);
        if start + entry.len() > out_bytes.len() {
            if last_entry_at.is_none() {
                let iosb = iosb as *mut usize;
                *iosb = STATUS_BUFFER_OVERFLOW as u32 as usize;
                *iosb.add(1) = 0;
                return STATUS_BUFFER_OVERFLOW;
            }
            break;
        }
        if let Some(prev) = last_entry_at {
            out_bytes[prev..prev + 4].copy_from_slice(&((start - prev) as u32).to_le_bytes());
        }
        out_bytes[start..start + entry.len()].copy_from_slice(entry);
        last_entry_at = Some(start);
        written = start + entry.len();
        listing.cursor += 1;
        if single {
            break;
        }
    }
    let status = if last_entry_at.is_some() {
        listing.returned_any = true;
        STATUS_SUCCESS
    } else if listing.returned_any {
        STATUS_NO_MORE_FILES
    } else {
        listing.returned_any = true;
        STATUS_NO_SUCH_FILE
    };
    let iosb = iosb as *mut usize;
    *iosb = status as u32 as usize;
    *iosb.add(1) = written;
    status
}

#[cfg(test)]
mod tests {
    use super::{is_whiteout_dir, listings, with_listing};

    #[test]
    fn directory_listing_io_does_not_hold_the_listing_table_lock() {
        let table_available = std::cell::Cell::new(false);
        let slot = std::sync::Mutex::new(None);
        with_listing(
            &slot,
            12,
            true,
            || {
                table_available.set(listings().try_lock().is_ok() && slot.try_lock().is_ok());
                Vec::new()
            },
            |_| (),
        );
        assert!(
            table_available.get(),
            "directory I/O held the listing table lock"
        );
    }

    #[test]
    fn whiteout_folder_name_matches_whatever_its_case() {
        for name in [".agvfs-wh", ".AGVFS-WH", ".AgVfs-Wh"] {
            assert!(is_whiteout_dir(&agora_game_api::fold_path_case(name)));
        }
        assert!(!is_whiteout_dir(&agora_game_api::fold_path_case(
            ".agvfs-wh-copy"
        )));
    }
}
