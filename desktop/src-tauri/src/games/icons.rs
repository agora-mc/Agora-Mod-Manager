//! The game's own executable icon (MASTER_SPEC §26).
//!
//! Reading an icon out of an `.exe` is a platform primitive, so it lives in
//! the desktop adapter rather than in core. The command is decoration: every
//! expected miss (no install, no exe, no icon, another platform) answers
//! `null`, and a Win32 failure is logged and also answers `null` so the UI
//! never has to handle an error for a picture.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use agora_core::ctx::Ctx;
use agora_core::game_registry::{GameInventory, GameRegistry};
use agora_game_api::{GameId, GamePath};
use serde::Serialize;

/// A PNG icon as the data URL the switcher puts in an `<img>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameIcon {
    pub data_url: String,
}

/// The icon for `game_id`, or `null` when there is nothing to show.
#[tauri::command]
pub async fn games_icon(app: tauri::AppHandle, game_id: String) -> Option<GameIcon> {
    tokio::task::spawn_blocking(move || {
        let ctx = match crate::core_context(&app) {
            Ok(ctx) => ctx,
            Err(error) => {
                eprintln!("[games] icon: core context unavailable: {error}");
                return None;
            }
        };
        cached_icon(&ctx, &game_id)
    })
    .await
    .unwrap_or_else(|error| {
        eprintln!("[games] icon: lookup did not finish: {error}");
        None
    })
}

type IconCache = Mutex<HashMap<(String, Option<PathBuf>), Option<GameIcon>>>;

fn cache() -> &'static IconCache {
    static CACHE: OnceLock<IconCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_icon(ctx: &Ctx, game_id: &str) -> Option<GameIcon> {
    let inventory = agora_core::game_registry::installed_games(ctx);
    let executable = game_executable(&ctx.games, &inventory, game_id);
    let key = (game_id.to_string(), executable.clone());
    {
        let cache = cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
    }
    let icon = executable.as_deref().and_then(icon_for_exe);
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.insert(key, icon.clone());
    icon
}

/// The game's launch executable for its first found install. Both halves come from the
/// backend: the install from discovery, the relative path from the game package. `None` when
/// the game has no install, the package has no declarative launch recipe (Minecraft builds its
/// own command line), or the executable is not a runtime file.
pub fn game_executable(
    registry: &GameRegistry,
    inventory: &GameInventory,
    game_id: &str,
) -> Option<PathBuf> {
    let game = GameId::new(game_id).ok()?;
    let definition = registry.game(&game)?;
    let GamePath::Runtime { path } = &definition.launch.as_ref()?.executable else {
        return None;
    };
    let install = inventory
        .installs
        .iter()
        .find(|install| install.game == game)?;
    Some(install.discovered.location.join(path.as_str()))
}

fn icon_for_exe(path: &Path) -> Option<GameIcon> {
    #[cfg(windows)]
    {
        match windows_icon::extract_png(path) {
            Ok(Some(png)) => Some(GameIcon {
                data_url: png_data_url(&png),
            }),
            Ok(None) => None,
            Err(error) => {
                eprintln!("[games] icon: {error} (from {})", path.display());
                None
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

fn png_data_url(png: &[u8]) -> String {
    format!("data:image/png;base64,{}", base64_encode(png))
}

/// Standard base64 with padding. Hand-written because the desktop crate has no
/// direct `base64` dependency and this brief only allowed `windows-sys` and `png`.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let bits = (first << 16) | (second << 8) | third;
        out.push(TABLE[((bits >> 18) & 63) as usize] as char);
        out.push(TABLE[((bits >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((bits >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(bits & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(windows)]
mod windows_icon {
    //! Extract an icon from an executable with the Win32 shell API.
    //!
    //! Calls and their Microsoft documentation:
    //! - `ExtractIconExW`: <https://learn.microsoft.com/windows/win32/api/shellapi/nf-shellapi-extracticonexw>
    //! - `GetIconInfo`: <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-geticoninfo>
    //! - `GetObjectW`: <https://learn.microsoft.com/windows/win32/api/wingdi/nf-wingdi-getobjectw>
    //! - `GetDC`: <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-getdc>
    //! - `GetDIBits`: <https://learn.microsoft.com/windows/win32/api/wingdi/nf-wingdi-getdibits>
    //! - `ReleaseDC`: <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-releasedc>
    //! - `DestroyIcon`: <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-destroyicon>
    //! - `DeleteObject`: <https://learn.microsoft.com/windows/win32/api/wingdi/nf-wingdi-deleteobject>

    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    };
    use windows_sys::Win32::UI::Shell::ExtractIconExW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

    /// A handle that is closed on every return path.
    struct IconHandle(HICON);

    impl Drop for IconHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: a non-null HICON from ExtractIconExW is ours to destroy.
                unsafe {
                    DestroyIcon(self.0);
                }
            }
        }
    }

    /// A GDI object that is deleted on every return path.
    struct GdiHandle(HGDIOBJ);

    impl Drop for GdiHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: a non-null bitmap from GetIconInfo is ours to delete.
                unsafe {
                    DeleteObject(self.0);
                }
            }
        }
    }

    /// A device context that is released on every return path.
    struct DcHandle(HDC);

    impl Drop for DcHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: GetDC(NULL) pairs with ReleaseDC(NULL, hdc).
                unsafe {
                    ReleaseDC(std::ptr::null_mut::<c_void>() as HWND, self.0);
                }
            }
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// The large icon in `path` as a PNG, `Ok(None)` when the file has no icon
    /// (or does not exist), and `Err` for a Win32 failure.
    pub fn extract_png(path: &Path) -> Result<Option<Vec<u8>>, String> {
        if !path.is_file() {
            return Ok(None);
        }
        let path_wide = wide(path);
        let mut large: HICON = std::ptr::null_mut();
        let mut small: HICON = std::ptr::null_mut();
        // SAFETY: path_wide is nul-terminated; both out-pointers are valid for one HICON.
        let count = unsafe { ExtractIconExW(path_wide.as_ptr(), 0, &mut large, &mut small, 1) };
        let large = IconHandle(large);
        // The small icon is not read, but it was returned and must be destroyed.
        let _small = IconHandle(small);
        if count == 0 || large.0.is_null() {
            return Ok(None);
        }

        let mut info = ICONINFO {
            fIcon: 0,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: std::ptr::null_mut(),
            hbmColor: std::ptr::null_mut(),
        };
        // SAFETY: large is a live icon and info is a valid out-parameter.
        if unsafe { GetIconInfo(large.0, &mut info) } == 0 {
            return Err("GetIconInfo failed".to_string());
        }
        let mask = GdiHandle(info.hbmMask as HGDIOBJ);
        let color = GdiHandle(info.hbmColor as HGDIOBJ);
        if color.0.is_null() {
            // A monochrome icon has no colour bitmap. Exes with a real icon have one.
            return Ok(None);
        }

        let mut bitmap: BITMAP = unsafe { std::mem::zeroed() };
        // SAFETY: color is a live HBITMAP and bitmap is large enough for a BITMAP.
        let got = unsafe {
            GetObjectW(
                color.0,
                std::mem::size_of::<BITMAP>() as i32,
                &mut bitmap as *mut BITMAP as *mut c_void,
            )
        };
        if got == 0 || bitmap.bmWidth <= 0 || bitmap.bmHeight <= 0 {
            return Err("GetObjectW failed".to_string());
        }
        let width = bitmap.bmWidth as u32;
        let height = bitmap.bmHeight as u32;

        let dc = DcHandle(unsafe { GetDC(std::ptr::null_mut::<HWND>() as HWND) });
        if dc.0.is_null() {
            return Err("GetDC failed".to_string());
        }
        let color_pixels = dib_bits(dc.0, color.0 as HBITMAP, width, height)?;
        // The mask is a 1bpp AND mask: set bits are transparent. Modern icons
        // carry their transparency in the alpha channel instead, so it is only
        // consulted when the colour bitmap has no alpha at all.
        let mask_pixels = if mask.0.is_null() {
            None
        } else {
            dib_bits(dc.0, mask.0 as HBITMAP, width, height).ok()
        };

        let has_alpha = color_pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] != 0);
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for index in 0..(width * height) as usize {
            let b = color_pixels[index * 4];
            let g = color_pixels[index * 4 + 1];
            let r = color_pixels[index * 4 + 2];
            let a = match (&mask_pixels, has_alpha) {
                (_, true) => color_pixels[index * 4 + 3],
                (Some(mask), false) => {
                    if mask[index * 4] != 0 {
                        0
                    } else {
                        255
                    }
                }
                (None, false) => 255,
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }

        encode_png(width, height, &rgba).map(Some)
    }

    /// 32bpp top-down BGRA pixels for `bitmap`.
    fn dib_bits(dc: HDC, bitmap: HBITMAP, width: u32, height: u32) -> Result<Vec<u8>, String> {
        let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            // Negative height asks for top-down rows, matching PNG's order.
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        // SAFETY: dc is live, bitmap is live, and pixels holds width*height BGRA.
        let lines = unsafe {
            GetDIBits(
                dc,
                bitmap,
                0,
                height,
                pixels.as_mut_ptr() as *mut c_void,
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        if lines == 0 {
            return Err("GetDIBits failed".to_string());
        }
        Ok(pixels)
    }

    fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
            writer
                .write_image_data(rgba)
                .map_err(|error| error.to_string())?;
        }
        Ok(png)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agora_core::game_registry::PackageSource;

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn registry() -> GameRegistry {
        let mut builder = GameRegistry::builder();
        builder
            .add(
                PackageSource::Compiled {
                    crate_name: "agora-game-creation".to_string(),
                },
                agora_game_creation::game_package(),
            )
            .expect("the Creation Engine package registers");
        builder.build()
    }

    fn skyrim_at(location: &Path) -> agora_core::game_registry::IdentifiedInstall {
        serde_json::from_value(serde_json::json!({
            "game": "skyrim-se",
            "install_id": "steam:489830",
            "discovered": {
                "store": "steam",
                "product": "489830",
                "name": "Skyrim Special Edition",
                "kind": "base_game",
                "parent_product": null,
                "location": location,
                "store_version": "1.6.1170",
                "store_build": null,
                "executables": [],
                "capabilities": {
                    "executables_readable": true,
                    "accepts_new_files": true,
                    "relocatable": true,
                },
                "volume": null,
            },
            "add_ons": [],
            "runtime": { "status": "unidentified", "reasons": ["test"] },
        }))
        .expect("an install in the core's shape")
    }

    fn temp_dir(label: &str) -> PathBuf {
        let unique = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agora-game-icon-{}-{}-{label}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp folder");
        dir
    }

    fn inventory(installs: Vec<agora_core::game_registry::IdentifiedInstall>) -> GameInventory {
        GameInventory {
            installs,
            unsupported: Vec::new(),
        }
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn an_unknown_game_has_no_executable() {
        let registry = registry();
        assert!(game_executable(&registry, &inventory(Vec::new()), "not-a-game").is_none());
    }

    #[test]
    fn a_game_with_no_found_install_has_no_icon() {
        let registry = registry();
        assert!(game_executable(&registry, &inventory(Vec::new()), "skyrim-se").is_none());
    }

    #[test]
    fn a_missing_executable_yields_no_icon() {
        let registry = registry();
        let root = temp_dir("missing");
        let installs = inventory(vec![skyrim_at(&root)]);
        let executable = game_executable(&registry, &installs, "skyrim-se").expect("a path");
        assert_eq!(executable, root.join("SkyrimSE.exe"));
        assert!(icon_for_exe(&executable).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_that_is_not_an_executable_yields_no_icon() {
        let root = temp_dir("not-an-exe");
        let executable = root.join("SkyrimSE.exe");
        std::fs::write(&executable, b"this is not an executable").expect("a fixture file");
        assert!(icon_for_exe(&executable).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_nonexistent_game_id_yields_no_icon() {
        let registry = registry();
        let root = temp_dir("unknown-id");
        let installs = inventory(vec![skyrim_at(&root)]);
        assert!(game_executable(&registry, &installs, "not-a-game").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn extracts_a_decodable_png_with_opaque_pixels_from_a_system_exe() {
        let exe = Path::new("C:/Windows/notepad.exe");
        if !exe.is_file() {
            return;
        }
        let png = windows_icon::extract_png(exe)
            .expect("notepad's icon extraction")
            .expect("notepad has an icon");
        let decoder = png::Decoder::new(std::io::Cursor::new(&png));
        let mut reader = decoder.read_info().expect("a decodable PNG");
        let size = reader.output_buffer_size().expect("a bounded image");
        let mut pixels = vec![0u8; size];
        let info = reader.next_frame(&mut pixels).expect("one frame");
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        assert_eq!(info.width, info.height);
        assert!(
            info.width == 32 || info.width == 48,
            "unexpected icon size {}x{}",
            info.width,
            info.height
        );
        assert!(
            pixels[..info.buffer_size()]
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] != 0),
            "the icon has no opaque pixels"
        );
    }
}
