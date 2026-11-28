//! Fixture executable for the import-order test: statically imports `agora_early_import.dll`
//! (declared `raw-dylib`, so it needs no import library and no build ordering) and exits.
//!
//! With the argument `spawn`, it also starts `Child.exe` from its own folder (another copy of
//! this program, standing in for what SKSE's loader does to `SkyrimSE.exe`) and exits with the
//! child's exit code.
//!
//! With the argument `headers`, it compares its own in-memory PE headers with the file on disk
//! (import directory, bound-import directory, section count and section headers) and exits 1 if
//! they differ. Import-table injection rewrites them in the suspended process, and the injected
//! DLL must put them back (`DetourRestoreAfterWith`) because DRM such as SteamStub reads them.

#[cfg(windows)]
#[link(name = "agora_early_import", kind = "raw-dylib")]
extern "C" {
    fn agora_early_import_marker() -> u32;
}

#[cfg(windows)]
fn main() {
    // SAFETY: a plain function of the fixture DLL that returns a constant.
    let marker = unsafe { agora_early_import_marker() };
    if marker != 0xA60A {
        std::process::exit(2);
    }
    if std::env::args().nth(1).as_deref() == Some("read-view") {
        let observed = std::fs::read("view-marker.txt").unwrap_or_default();
        std::process::exit(if observed == b"visible only through the view" {
            0
        } else {
            4
        });
    }
    if std::env::args().nth(1).as_deref() == Some("spawn-empty-environment") {
        let child = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("Child.exe")));
        let code = child
            .and_then(|child| {
                std::process::Command::new(child)
                    .env_clear()
                    .arg("read-view")
                    .status()
                    .ok()
            })
            .and_then(|status| status.code())
            .unwrap_or(3);
        std::process::exit(code);
    }
    if std::env::args().nth(1).as_deref() == Some("headers") {
        std::process::exit(if headers_match_the_file() { 0 } else { 1 });
    }
    if std::env::args().nth(1).as_deref() == Some("spawn") {
        let child = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("Child.exe")));
        let code = child
            .and_then(|child| std::process::Command::new(child).status().ok())
            .and_then(|status| status.code())
            .unwrap_or(3);
        std::process::exit(code);
    }
}

/// The parts of the PE headers import-table injection changes, from a mapped image or a file.
#[cfg(windows)]
fn header_fingerprint(image: &[u8]) -> Option<Vec<u8>> {
    let u16_at = |o: usize| {
        image
            .get(o..o + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |o: usize| {
        image
            .get(o..o + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let nt = u32_at(0x3C)? as usize;
    if image.get(nt..nt + 4)? != b"PE\0\0" {
        return None;
    }
    let sections = u16_at(nt + 6)? as usize;
    let optional_size = u16_at(nt + 20)? as usize;
    let optional = nt + 24;
    if u16_at(optional)? != 0x20B {
        return None; // PE32+ only: the fixture is a 64-bit program
    }
    let mut out = Vec::new();
    out.extend_from_slice(&(sections as u16).to_le_bytes());
    // Data directories start 112 bytes into the optional header: import is entry 1, bound import 11.
    for entry in [1usize, 11] {
        out.extend_from_slice(
            image.get(optional + 112 + entry * 8..optional + 112 + entry * 8 + 8)?,
        );
    }
    let table = optional + optional_size;
    out.extend_from_slice(image.get(table..table + sections * 40)?);
    Some(out)
}

#[cfg(windows)]
fn headers_match_the_file() -> bool {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    let Ok(file) = std::env::current_exe().and_then(std::fs::read) else {
        return false;
    };
    // SAFETY: the null module is this program's image, mapped for as long as it runs; its headers
    // fit in the first page.
    let mapped = unsafe {
        let base = GetModuleHandleW(std::ptr::null()) as *const u8;
        std::slice::from_raw_parts(base, 0x1000)
    };
    match (header_fingerprint(mapped), header_fingerprint(&file)) {
        (Some(mapped), Some(file)) => mapped == file,
        _ => false,
    }
}

#[cfg(not(windows))]
fn main() {}
