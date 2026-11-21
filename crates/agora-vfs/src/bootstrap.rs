//! The parent publishes a copy of its configuration until the suspended child confirms its hooks.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null;

use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ,
    FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, SetEvent, WaitForMultipleObjects, EVENT_MODIFY_STATE,
};

use crate::paths::Prefix;

// A u32 byte count and the child-side event handle as a u64, independent of process bitness.
const HEADER_LEN: usize = 12;

fn mapping_name(pid: u32) -> Vec<u16> {
    format!("Local\\agora-vfs-child-{pid}")
        .encode_utf16()
        .chain(Some(0))
        .collect()
}

struct View(MEMORY_MAPPED_VIEW_ADDRESS);

impl View {
    unsafe fn map(mapping: &OwnedHandle, access: u32, len: usize) -> Result<Self, String> {
        let view = MapViewOfFile(mapping.as_raw_handle(), access, 0, 0, len);
        if view.Value.is_null() {
            return Err(format!(
                "could not map the child configuration (error {})",
                GetLastError()
            ));
        }
        Ok(Self(view))
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: this owns a successfully mapped view.
        unsafe { UnmapViewOfFile(self.0) };
    }
}

pub(crate) struct Handoff {
    _mapping: OwnedHandle,
    ready: OwnedHandle,
}

impl Handoff {
    /// Called with the child still suspended, before any loader work starts.
    pub(crate) unsafe fn publish(
        process: HANDLE,
        pid: u32,
        config: &str,
        prefixes: &[Prefix],
    ) -> Result<Self, String> {
        let snapshot = serde_json::to_vec(&(config, prefixes))
            .map_err(|error| format!("could not encode the child configuration: {error}"))?;
        let size = u32::try_from(snapshot.len().saturating_add(HEADER_LEN))
            .ok()
            .filter(|size| *size as usize <= isize::MAX as usize)
            .ok_or("child configuration is too large")?;
        let mapping = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            null(),
            PAGE_READWRITE,
            0,
            size,
            mapping_name(pid).as_ptr(),
        );
        let mapping_error = GetLastError();
        if mapping.is_null() {
            return Err(format!(
                "could not publish the child configuration (error {mapping_error})"
            ));
        }
        let mapping = OwnedHandle::from_raw_handle(mapping);
        if mapping_error == ERROR_ALREADY_EXISTS {
            return Err("the child's configuration mapping already exists".to_string());
        }
        let view = View::map(&mapping, FILE_MAP_WRITE, size as usize)?;
        let ready = CreateEventW(null(), 1, 0, null());
        if ready.is_null() {
            return Err(format!(
                "could not create the child ready event (error {})",
                GetLastError()
            ));
        }
        let ready = OwnedHandle::from_raw_handle(ready);
        let mut child_ready = std::ptr::null_mut();
        if DuplicateHandle(
            GetCurrentProcess(),
            ready.as_raw_handle(),
            process,
            &mut child_ready,
            EVENT_MODIFY_STATE,
            0,
            0,
        ) == 0
        {
            return Err(format!(
                "could not pass the child ready event (error {})",
                GetLastError()
            ));
        }
        let buffer = std::slice::from_raw_parts_mut(view.0.Value.cast::<u8>(), size as usize);
        buffer[..4].copy_from_slice(&(snapshot.len() as u32).to_le_bytes());
        buffer[4..HEADER_LEN].copy_from_slice(&(child_ready as usize as u64).to_le_bytes());
        buffer[HEADER_LEN..].copy_from_slice(&snapshot);
        Ok(Self {
            _mapping: mapping,
            ready,
        })
    }

    pub(crate) unsafe fn wait(&self, process: HANDLE) -> Result<(), String> {
        let handles = [process, self.ready.as_raw_handle()];
        match WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, 5_000) {
            status if status == WAIT_OBJECT_0 + 1 => Ok(()),
            WAIT_OBJECT_0 => Err("the child exited before confirming its VFS hooks".to_string()),
            WAIT_TIMEOUT => {
                Err("the child did not confirm its VFS hooks within 5 seconds".to_string())
            }
            _ => Err(format!(
                "waiting for the child's hooks failed (error {})",
                GetLastError()
            )),
        }
    }
}

pub(crate) struct ChildConfig {
    pub json: String,
    pub prefixes: Vec<Prefix>,
    pub ready: HANDLE,
}

pub(crate) unsafe fn receive() -> Result<Option<ChildConfig>, String> {
    let mapping = OpenFileMappingW(FILE_MAP_READ, 0, mapping_name(std::process::id()).as_ptr());
    if mapping.is_null() {
        let error = GetLastError();
        return if error == ERROR_FILE_NOT_FOUND {
            Ok(None)
        } else {
            Err(format!(
                "could not open the child configuration (error {error})"
            ))
        };
    }
    let mapping = OwnedHandle::from_raw_handle(mapping);
    let header = View::map(&mapping, FILE_MAP_READ, HEADER_LEN)?;
    let header = std::slice::from_raw_parts(header.0.Value.cast::<u8>(), HEADER_LEN);
    let count = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    let handle = u64::from_le_bytes(header[4..].try_into().unwrap());
    let handle = usize::try_from(handle).unwrap_or(0) as HANDLE;
    let size = count
        .checked_add(HEADER_LEN)
        .filter(|size| *size <= isize::MAX as usize);
    let Some(size) =
        size.filter(|_| count != 0 && !handle.is_null() && handle != INVALID_HANDLE_VALUE)
    else {
        return Err("the child configuration header is invalid".to_string());
    };
    let view = View::map(&mapping, FILE_MAP_READ, size)?;
    let bytes = std::slice::from_raw_parts(view.0.Value.cast::<u8>(), size);
    let (json, prefixes) = serde_json::from_slice(&bytes[HEADER_LEN..])
        .map_err(|error| format!("the child configuration is invalid: {error}"))?;
    Ok(Some(ChildConfig {
        json,
        prefixes,
        ready: handle,
    }))
}

pub(crate) unsafe fn acknowledge(ready: HANDLE) -> bool {
    // The parent gives the child only EVENT_MODIFY_STATE. A bad handle fails the handshake.
    if SetEvent(ready) == 0 {
        return false;
    }
    CloseHandle(ready);
    true
}
