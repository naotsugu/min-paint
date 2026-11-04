//! Native clipboard access for Windows.
//!
//! gpui only publishes the registered "PNG" format, so apps that read CF_DIB /
//! CF_BITMAP / CF_DIBV5 cannot paste the image. Transparency is preserved in
//! "PNG" and CF_DIBV5; CF_DIB (which has no alpha) is flattened onto white.

use std::ffi::c_void;

type Handle = *mut c_void;

const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
const GMEM_MOVEABLE: u32 = 0x0002;

#[link(name = "user32")]
unsafe extern "system" {
    fn OpenClipboard(hwnd: Handle) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(format: u32, mem: Handle) -> Handle;
    fn RegisterClipboardFormatW(name: *const u16) -> u32;
    fn GetActiveWindow() -> Handle;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
    fn GetClipboardData(format: u32) -> Handle;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> Handle;
    fn GlobalLock(mem: Handle) -> *mut c_void;
    fn GlobalUnlock(mem: Handle) -> i32;
    fn GlobalFree(mem: Handle) -> Handle;
    fn GlobalSize(mem: Handle) -> usize;
}

/// Build a 24bpp bottom-up CF_DIB payload (BITMAPINFOHEADER + pixel rows).
/// CF_DIB has no alpha, so transparent pixels are composited onto white.
fn build_dib(width: usize, height: usize, pixels: &[[u8; 4]]) -> Vec<u8> {
    let stride = (width * 3 + 3) & !3; // rows are padded to 4 bytes
    let image_size = stride * height;
    let mut dib = Vec::with_capacity(40 + image_size);

    // BITMAPINFOHEADER
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&(width as i32).to_le_bytes()); // biWidth
    dib.extend_from_slice(&(height as i32).to_le_bytes()); // biHeight (positive = bottom-up)
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&(image_size as u32).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // source over white
    let flat = |c: u8, a: u32| ((c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;

    for y in (0..height).rev() {
        let row = &pixels[y * width..(y + 1) * width];
        for p in row {
            let a = p[3] as u32;
            dib.extend_from_slice(&[flat(p[2], a), flat(p[1], a), flat(p[0], a)]); // BGR
        }
        dib.resize(dib.len() + (stride - width * 3), 0);
    }
    dib
}

/// Build a 32bpp bottom-up CF_DIBV5 payload with a straight (non-premultiplied) alpha channel.
fn build_dibv5(width: usize, height: usize, pixels: &[[u8; 4]]) -> Vec<u8> {
    let image_size = width * height * 4;
    let mut dib = Vec::with_capacity(124 + image_size);

    // BITMAPV5HEADER (124 bytes)
    dib.extend_from_slice(&124u32.to_le_bytes()); // bV5Size
    dib.extend_from_slice(&(width as i32).to_le_bytes()); // bV5Width
    dib.extend_from_slice(&(height as i32).to_le_bytes()); // bV5Height (positive = bottom-up)
    dib.extend_from_slice(&1u16.to_le_bytes()); // bV5Planes
    dib.extend_from_slice(&32u16.to_le_bytes()); // bV5BitCount
    dib.extend_from_slice(&3u32.to_le_bytes()); // bV5Compression = BI_BITFIELDS
    dib.extend_from_slice(&(image_size as u32).to_le_bytes()); // bV5SizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // bV5XPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes()); // bV5YPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // bV5ClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // bV5ClrImportant
    dib.extend_from_slice(&0x00FF_0000u32.to_le_bytes()); // bV5RedMask
    dib.extend_from_slice(&0x0000_FF00u32.to_le_bytes()); // bV5GreenMask
    dib.extend_from_slice(&0x0000_00FFu32.to_le_bytes()); // bV5BlueMask
    dib.extend_from_slice(&0xFF00_0000u32.to_le_bytes()); // bV5AlphaMask
    dib.extend_from_slice(&0x7352_4742u32.to_le_bytes()); // bV5CSType = LCS_sRGB ('sRGB')
    dib.extend_from_slice(&[0u8; 36]); // bV5Endpoints (unused for sRGB)
    dib.extend_from_slice(&[0u8; 12]); // bV5GammaRed / Green / Blue
    dib.extend_from_slice(&4u32.to_le_bytes()); // bV5Intent = LCS_GM_IMAGES
    dib.extend_from_slice(&0u32.to_le_bytes()); // bV5ProfileData
    dib.extend_from_slice(&0u32.to_le_bytes()); // bV5ProfileSize
    dib.extend_from_slice(&0u32.to_le_bytes()); // bV5Reserved

    for y in (0..height).rev() {
        for p in &pixels[y * width..(y + 1) * width] {
            dib.extend_from_slice(&[p[2], p[1], p[0], p[3]]); // BGRA
        }
    }
    dib
}

/// Hand a copy of `data` to the clipboard under `format`.
/// On success the system owns the memory; on failure it is freed here.
fn set_data(format: u32, data: &[u8]) -> bool {
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, data.len());
        if mem.is_null() {
            return false;
        }
        let ptr = GlobalLock(mem);
        if ptr.is_null() {
            GlobalFree(mem);
            return false;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
        GlobalUnlock(mem);
        if SetClipboardData(format, mem).is_null() {
            GlobalFree(mem);
            return false;
        }
        true
    }
}

/// Put the image on the clipboard as "PNG" and CF_DIBV5 (both with alpha),
/// plus CF_DIB flattened onto white for apps that know nothing about transparency.
pub fn copy_image(width: usize, height: usize, pixels: &[[u8; 4]], png: &[u8]) -> Result<(), String> {
    if width == 0 || height == 0 || pixels.len() != width * height {
        return Err("invalid image size".to_string());
    }
    let dib = build_dib(width, height, pixels);
    let dibv5 = build_dibv5(width, height, pixels);

    unsafe {
        // another process may hold the clipboard for a moment, so retry briefly
        let owner = GetActiveWindow();
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(owner) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !opened {
            return Err("failed to open the clipboard".to_string());
        }

        EmptyClipboard();

        // registered "PNG" format: keeps the alpha channel in browsers, Office, etc.
        let name: Vec<u16> = "PNG\0".encode_utf16().collect();
        let png_format = RegisterClipboardFormatW(name.as_ptr());
        if png_format != 0 {
            set_data(png_format, png);
        }

        // Set both DIB formats explicitly: when only one is set, Windows synthesizes
        // the other one, and the synthesized CF_DIB would show transparent pixels as black.
        // CF_DIBV5 keeps the alpha; CF_DIB is the white-flattened fallback.
        let ok_v5 = set_data(CF_DIBV5, &dibv5);
        let ok = set_data(CF_DIB, &dib);
        CloseClipboard();

        if ok || ok_v5 { Ok(()) } else { Err("failed to set CF_DIB".to_string()) }
    }
}

/// Copy the bytes of a clipboard format (the clipboard must be open).
fn read_data(format: u32) -> Option<Vec<u8>> {
    unsafe {
        if IsClipboardFormatAvailable(format) == 0 {
            return None;
        }
        let mem = GetClipboardData(format);
        if mem.is_null() {
            return None;
        }
        let size = GlobalSize(mem);
        if size == 0 {
            return None;
        }
        let ptr = GlobalLock(mem);
        if ptr.is_null() {
            return None;
        }
        // the clipboard owns the memory: copy it, never free it
        let data = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
        GlobalUnlock(mem);
        Some(data)
    }
}

/// Wrap a packed DIB (CF_DIB / CF_DIBV5) in a BMP file header and decode it.
fn decode_dib(dib: &[u8], force_opaque: bool) -> Option<image::RgbaImage> {
    let u16_at = |o: usize| dib.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |o: usize| dib.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));

    let header_size = u32_at(0)? as usize;
    if header_size < 40 {
        return None; // OS/2 core headers are not supported
    }
    let bit_count = u16_at(14)?;
    let compression = u32_at(16)?;
    let clr_used = u32_at(32)? as usize;

    // color masks follow a plain 40-byte header when BI_BITFIELDS / BI_ALPHABITFIELDS is used
    let masks = if header_size == 40 {
        match compression {
            3 => 12,
            6 => 16,
            _ => 0,
        }
    } else {
        0
    };
    // color table (4 bytes per entry)
    let palette = if clr_used > 0 {
        clr_used
    } else if bit_count <= 8 {
        1usize << bit_count
    } else {
        0
    };
    let offset = 14 + header_size + masks + palette * 4;
    if offset > 14 + dib.len() {
        return None;
    }

    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&(offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);

    let mut img = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp)
        .ok()?
        .to_rgba8();

    // the alpha byte of a 32bpp DIB is often unused (0): treat such images as opaque
    if force_opaque || img.pixels().all(|p| p[3] == 0) {
        for p in img.pixels_mut() {
            p[3] = 0xff;
        }
    }
    Some(img)
}

/// Read an image from the clipboard: "PNG", then CF_DIBV5, then CF_DIB.
/// Returns None when the clipboard holds no (decodable) image.
pub fn read_image() -> Option<image::RgbaImage> {
    let (png, dibv5, dib) = unsafe {
        // another process may hold the clipboard for a moment, so retry briefly
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(GetActiveWindow()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !opened {
            return None;
        }
        let name: Vec<u16> = "PNG\0".encode_utf16().collect();
        let png_format = RegisterClipboardFormatW(name.as_ptr());
        let png = if png_format != 0 { read_data(png_format) } else { None };
        let dibv5 = read_data(CF_DIBV5);
        let dib = read_data(CF_DIB);
        // release the clipboard before the (slow) decoding
        CloseClipboard();
        (png, dibv5, dib)
    };

    if let Some(png) = png {
        if let Ok(img) = image::load_from_memory_with_format(&png, image::ImageFormat::Png) {
            return Some(img.to_rgba8());
        }
    }
    if let Some(img) = dibv5.as_deref().and_then(|d| decode_dib(d, false)) {
        return Some(img);
    }
    // CF_DIB has no defined alpha, so it is always treated as opaque
    dib.as_deref().and_then(|d| decode_dib(d, true))
}
