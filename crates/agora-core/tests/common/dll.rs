#![allow(dead_code)]
//! Invented PE files for the SKSE inspector tests. Nothing here is copied from a real DLL; the
//! bytes are laid out by hand so every case (a good export, a missing one, a truncated file) is
//! exact.

const FILE_ALIGN: usize = 0x200;
const SECTION_RVA: u32 = 0x1000;
const E_LFANEW: usize = 0x80;
const OPTIONAL_SIZE: usize = 0xf0;
const VERSION_DATA_SIZE: usize = 0x350;

/// The `SKSE::PluginVersionData` an invented DLL carries.
#[derive(Clone, Debug)]
pub struct VersionData {
    pub data_version: u32,
    pub plugin_version: [u16; 4],
    pub name: String,
    pub version_independence_ex: u32,
    pub version_independence: u32,
    pub compatible: Vec<[u16; 4]>,
    pub xse_minimum: [u16; 4],
}

impl Default for VersionData {
    fn default() -> Self {
        Self {
            data_version: 1,
            plugin_version: [1, 2, 3, 0],
            name: "Invented Plugin".to_string(),
            // AddressLibraryV5, the CommonLibSSE-NG default.
            version_independence_ex: 0x2,
            version_independence: 0,
            compatible: Vec::new(),
            xse_minimum: [0, 0, 0, 0],
        }
    }
}

/// `major << 24 | minor << 16 | patch << 4 | build`, as `REL::Version::pack`.
pub fn pack_version(parts: [u16; 4]) -> u32 {
    (u32::from(parts[0]) & 0xff) << 24
        | (u32::from(parts[1]) & 0xff) << 16
        | (u32::from(parts[2]) & 0xfff) << 4
        | (u32::from(parts[3]) & 0xf)
}

fn put_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn align(value: usize, to: usize) -> usize {
    value.div_ceil(to) * to
}

/// A DLL exporting `SKSEPlugin_Version`, pointing at `data`.
pub fn dll(data: &VersionData) -> Vec<u8> {
    pe(Some("SKSEPlugin_Version"), Some(data))
}

/// A DLL exporting one name, pointing at the version data (or at a dummy when `data` is `None`).
pub fn dll_exporting(name: &str, data: Option<&VersionData>) -> Vec<u8> {
    pe(Some(name), data)
}

/// A PE with no export directory at all.
pub fn dll_without_exports() -> Vec<u8> {
    pe(None, None)
}

/// `export_name`: `None` means no export directory. When `Some`, the export table names it and
/// points at the version data (or a dummy if `data` is `None`).
pub fn pe(export_name: Option<&str>, data: Option<&VersionData>) -> Vec<u8> {
    let mut section = vec![0u8; 0x1000];

    let export_off = 0x00usize;
    let names_off = 0x28usize;
    let ordinals_off = 0x2cusize;
    let functions_off = 0x30usize;
    let name_off = 0x34usize;
    let name = export_name.unwrap_or("");
    let name_end = name_off + name.len() + 1;
    let struct_off = align(name_end, 0x10);
    let needed = if data.is_some() {
        struct_off + VERSION_DATA_SIZE
    } else {
        name_end + 4
    };
    section.resize(needed, 0);

    if export_name.is_some() {
        // IMAGE_EXPORT_DIRECTORY
        put_u32(&mut section, export_off + 16, 1); // Base
        put_u32(&mut section, export_off + 20, 1); // NumberOfFunctions
        put_u32(&mut section, export_off + 24, 1); // NumberOfNames
        put_u32(
            &mut section,
            export_off + 28,
            SECTION_RVA + functions_off as u32,
        );
        put_u32(
            &mut section,
            export_off + 32,
            SECTION_RVA + names_off as u32,
        );
        put_u32(
            &mut section,
            export_off + 36,
            SECTION_RVA + ordinals_off as u32,
        );

        put_u32(&mut section, names_off, SECTION_RVA + name_off as u32);
        put_u16(&mut section, ordinals_off, 0);
        put_u32(
            &mut section,
            functions_off,
            SECTION_RVA
                + if data.is_some() {
                    struct_off as u32
                } else {
                    name_off as u32
                },
        );
        section[name_off..name_off + name.len()].copy_from_slice(name.as_bytes());
    }

    if let Some(data) = data {
        put_u32(&mut section, struct_off, data.data_version);
        put_u32(
            &mut section,
            struct_off + 0x004,
            pack_version(data.plugin_version),
        );
        let name_bytes = data.name.as_bytes();
        let name_len = name_bytes.len().min(255);
        section[struct_off + 0x008..struct_off + 0x008 + name_len]
            .copy_from_slice(&name_bytes[..name_len]);
        put_u32(
            &mut section,
            struct_off + 0x304,
            data.version_independence_ex,
        );
        put_u32(&mut section, struct_off + 0x308, data.version_independence);
        for (index, version) in data.compatible.iter().take(16).enumerate() {
            put_u32(
                &mut section,
                struct_off + 0x30c + index * 4,
                pack_version(*version),
            );
        }
        put_u32(
            &mut section,
            struct_off + 0x34c,
            pack_version(data.xse_minimum),
        );
    }

    let raw_size = align(section.len(), FILE_ALIGN);
    let mut file = vec![0u8; FILE_ALIGN + raw_size];
    file[0] = b'M';
    file[1] = b'Z';
    put_u32(&mut file, 0x3c, E_LFANEW as u32);
    file[E_LFANEW..E_LFANEW + 4].copy_from_slice(b"PE\0\0");
    // COFF: one section, optional header size.
    put_u16(&mut file, E_LFANEW + 4, 0x8664);
    put_u16(&mut file, E_LFANEW + 6, 1);
    put_u16(&mut file, E_LFANEW + 20, OPTIONAL_SIZE as u16);
    // Optional header: PE32+ and one data directory.
    let optional = E_LFANEW + 24;
    put_u16(&mut file, optional, 0x20b);
    put_u32(&mut file, optional + 108, 1);
    if export_name.is_some() {
        put_u32(&mut file, optional + 112, SECTION_RVA);
        put_u32(&mut file, optional + 116, 0x40);
    }
    // Section table.
    let table = optional + OPTIONAL_SIZE;
    file[table..table + 6].copy_from_slice(b".rdata");
    put_u32(&mut file, table + 8, section.len() as u32);
    put_u32(&mut file, table + 12, SECTION_RVA);
    put_u32(&mut file, table + 16, raw_size as u32);
    put_u32(&mut file, table + 20, FILE_ALIGN as u32);
    file[FILE_ALIGN..FILE_ALIGN + section.len()].copy_from_slice(&section);
    file
}
