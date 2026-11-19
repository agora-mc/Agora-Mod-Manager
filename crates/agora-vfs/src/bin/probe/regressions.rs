use std::ffi::{c_void, CStr};
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_RENAME_INFO,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use super::names::Unicode;

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: HANDLE,
    name: *mut Unicode,
    attributes: u32,
    security: *mut c_void,
    qos: *mut c_void,
}

unsafe fn nt_proc<T>(name: &CStr) -> T {
    let module = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
    let address = GetProcAddress(module, name.as_ptr().cast()).unwrap();
    std::mem::transmute_copy(&address)
}

fn set_name(handle: HANDLE, root: HANDLE, name: &str, flags: u32, class: u32) -> i32 {
    let name: Vec<u16> = name.encode_utf16().collect();
    let len = std::mem::offset_of!(FILE_RENAME_INFO, FileName) + name.len() * 2;
    let mut buffer = vec![0u64; len.div_ceil(8)];
    // FILE_LINK_INFORMATION and FILE_RENAME_INFORMATION have identical layouts.
    unsafe {
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        (*info).Anonymous.Flags = flags;
        (*info).RootDirectory = root;
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        let set: unsafe extern "system" fn(
            HANDLE,
            *mut IO_STATUS_BLOCK,
            *const FILE_RENAME_INFO,
            u32,
            u32,
        ) -> i32 = nt_proc(c"NtSetInformationFile");
        set(handle, &mut std::mem::zeroed(), info, len as u32, class)
    }
}

fn delete_by_name(path: &str) -> i32 {
    let mut name: Vec<u16> = path.encode_utf16().collect();
    let len = (name.len() * 2) as u16;
    let mut name = Unicode {
        length: len,
        maximum_length: len,
        buffer: name.as_mut_ptr(),
    };
    let mut oa = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root: null_mut(),
        name: &mut name,
        attributes: 0x40, // OBJ_CASE_INSENSITIVE
        security: null_mut(),
        qos: null_mut(),
    };
    unsafe {
        let delete: unsafe extern "system" fn(*mut ObjectAttributes) -> i32 =
            nt_proc(c"NtDeleteFile");
        delete(&mut oa)
    }
}

fn query_names(dir: HANDLE, pattern: Option<&str>, restart: bool) -> Vec<String> {
    let mut pattern = pattern.map(|pattern| pattern.encode_utf16().collect::<Vec<_>>());
    let mut name = pattern.as_mut().map(|pattern| Unicode {
        length: (pattern.len() * 2) as u16,
        maximum_length: (pattern.len() * 2) as u16,
        buffer: pattern.as_mut_ptr(),
    });
    let name = name.as_mut().map_or(null_mut(), std::ptr::from_mut);
    let mut bytes = [0u64; 128];
    unsafe {
        let query: unsafe extern "system" fn(
            HANDLE,
            HANDLE,
            *mut c_void,
            *mut c_void,
            *mut IO_STATUS_BLOCK,
            *mut c_void,
            u32,
            u32,
            u8,
            *mut Unicode,
            u8,
        ) -> i32 = nt_proc(c"NtQueryDirectoryFile");
        let mut iosb: IO_STATUS_BLOCK = std::mem::zeroed();
        let status = query(
            dir,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut iosb,
            bytes.as_mut_ptr().cast(),
            std::mem::size_of_val(&bytes) as u32,
            12,
            0,
            name,
            restart.into(),
        );
        assert_eq!(status, 0, "directory query returned {status:#x}");
        let bytes = std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), iosb.Information);
        let mut names = Vec::new();
        let mut offset = 0;
        loop {
            let next = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            let len =
                u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()) as usize;
            let (units, rest) = bytes[offset + 12..offset + 12 + len].as_chunks::<2>();
            assert!(rest.is_empty());
            let units: Vec<_> = units.iter().copied().map(u16::from_le_bytes).collect();
            names.push(String::from_utf16(&units).unwrap());
            if next == 0 {
                return names;
            }
            offset += next;
        }
    }
}

pub(super) fn run(case: &str) {
    match case {
        "failed-create" => {
            let path = Path::new("blocked/leaf.txt");
            assert!(OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .is_err());
            assert_eq!(fs::read(path).unwrap_err().kind(), ErrorKind::NotFound);
        }
        "failed-delete" => {
            let path = Path::new("leaf.txt");
            assert_eq!(
                fs::remove_file(path).unwrap_err().kind(),
                ErrorKind::PermissionDenied
            );
            assert_eq!(fs::read(path).unwrap(), b"lower");
        }
        "relative-rename" => {
            let root = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open("folder")
                .unwrap();
            let source = OpenOptions::new()
                .access_mode(DELETE)
                .open("origin.txt")
                .unwrap();
            let status = set_name(
                source.as_raw_handle(),
                root.as_raw_handle(),
                "destination.txt",
                1,
                10,
            );
            assert_eq!(status, 0, "relative rename returned {status:#x}");
            assert_eq!(fs::read("folder/destination.txt").unwrap(), b"upper source");
            assert!(!Path::new("origin.txt").exists());
        }
        "lower-link" | "lower-link-ex" => {
            let source = fs::File::open("leaf.txt").unwrap();
            let class = if case == "lower-link" { 11 } else { 72 };
            let status = set_name(source.as_raw_handle(), null_mut(), "link.txt", 0, class);
            assert_eq!(
                status as u32, 0xc000_0022,
                "lower link returned {status:#x}"
            );
        }
        "native-delete" => {
            let cwd = std::env::current_dir().unwrap();
            let name = format!(r"\??\{}", cwd.join("leaf.txt").display());
            let status = delete_by_name(&name);
            assert_eq!(status, 0, "native delete returned {status:#x}");
            assert_eq!(
                fs::read("leaf.txt").unwrap_err().kind(),
                ErrorKind::NotFound
            );
        }
        "listing-restart" => {
            let dir = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open("folder")
                .unwrap();
            let handle = dir.as_raw_handle();
            assert_eq!(query_names(handle, Some("*.txt"), false), ["keep.txt"]);
            assert_eq!(query_names(handle, None, true), ["keep.txt"]);
            assert_eq!(query_names(handle, Some("*.bin"), true), ["keep.txt"]);
        }
        _ => panic!("unknown regression case: {case}"),
    }
}
