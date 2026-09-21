//! The tray icon, built from the logo at runtime.
//!
//! A faithful port of the 3.x `internal/wintray/icon_windows.go`, including
//! the reasons it was written that way. ⚠ There never was a `.ico` to
//! recover: decoding the PNG at run time keeps ONE source image in the
//! repository and lets the icon follow the user's DPI, which a fixed 16x16
//! resource cannot do.

#![cfg(windows)]

use windows_sys::Win32::Foundation::HANDLE;

/// The alpha below which a pixel counts as absent when measuring the logo.
///
/// The artwork carries an antialiasing fringe peaking at 48/255 -- invisible
/// on screen, but enough to push the bounding box a pixel wide on each side
/// and waste that pixel at icon size.
const ALPHA_FLOOR: u8 = 63;

const LOGO: &[u8] = include_bytes!("../../../web/static/hydra-logo.png");

/// Decode the logo and build an HICON at the shell's small-icon size.
///
/// Returns None on any failure; the caller falls back to the stock icon.
pub fn from_logo() -> Option<HANDLE> {
    use windows_sys::Win32::Graphics::Gdi::{
        CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateIconIndirect, GetSystemMetrics, ICONINFO, SM_CXSMICON, SM_CYSMICON,
    };

    let (pix, sw, sh) = decode(LOGO)?;

    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
    let (w, h) = if w <= 0 || h <= 0 { (16i32, 16i32) } else { (w, h) };
    let (wu, hu) = (w as usize, h as usize);

    // Tightest rectangle holding the logo itself. Sampling the raw canvas
    // instead would carry the transparent padding into the icon, landing the
    // glyph off-centre and a little small.
    let (bx0, by0, bx1, by1) = opaque_bounds(&pix, sw, sh);
    let (bw, bh) = ((bx1 - bx0) as f64, (by1 - by0) as f64);
    if bw <= 0.0 || bh <= 0.0 {
        return None;
    }

    // Cover rather than contain: scale until the glyph fills the square on
    // both axes and let the wider one spill past the edges. Containing it
    // leaves a dead row above and below; covering trims ~7% off each side,
    // which costs only ~3% of the ink because all that lives out there are the
    // tips of the outer two snouts.
    let scale = (w as f64 / bw).max(h as f64 / bh);
    let off_x = (bw * scale - w as f64) / 2.0;
    let off_y = (bh * scale - h as f64) / 2.0;

    unsafe {
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            // ⚠ Negative: a top-down DIB, so row 0 is the TOP row and the
            // pixel order matches the image being sampled.
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB as u32,
            ..std::mem::zeroed()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbm = CreateDIBSection(
            std::ptr::null_mut(),
            &bi,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if hbm.is_null() || bits.is_null() {
            return None;
        }
        let px = std::slice::from_raw_parts_mut(bits as *mut u8, wu * hu * 4);

        for y in 0..hu {
            for x in 0..wu {
                let x0 = bx0 as f64 + (x as f64 + off_x) / scale;
                let x1 = bx0 as f64 + (x as f64 + 1.0 + off_x) / scale;
                let y0 = by0 as f64 + (y as f64 + off_y) / scale;
                let y1 = by0 as f64 + (y as f64 + 1.0 + off_y) / scale;
                let (r, g, b, a) =
                    average(&pix, sw, (bx0, by0, bx1, by1), x0, y0, x1, y1);
                // Premultiplied, which is what a 32bpp DIB icon wants. Order
                // in memory is BGRA.
                let i = (y * wu + x) * 4;
                px[i] = b;
                px[i + 1] = g;
                px[i + 2] = r;
                px[i + 3] = a;
            }
        }

        // A 1bpp all-zero mask: every pixel opaque, so the colour bitmap's own
        // alpha channel decides transparency.
        let hmask = CreateBitmap(w, h, 1, 1, std::ptr::null());
        if hmask.is_null() {
            DeleteObject(hbm as _);
            return None;
        }
        let ii = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hmask,
            hbmColor: hbm,
        };
        let hicon = CreateIconIndirect(&ii);
        // CreateIconIndirect copies the bitmaps; ours are ours to free.
        DeleteObject(hmask as _);
        DeleteObject(hbm as _);
        if hicon.is_null() {
            None
        } else {
            Some(hicon as HANDLE)
        }
    }
}

/// Straight RGBA8 pixels, plus the image size.
fn decode(data: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    let dec = png::Decoder::new(std::io::Cursor::new(data));
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    // The logo is RGBA8; anything else is not what this function was written
    // for, and guessing at a conversion would produce a wrong icon silently.
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(w * h * 4);
    Some((buf, w, h))
}

fn opaque_bounds(pix: &[u8], w: usize, h: usize) -> (usize, usize, usize, usize) {
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    let mut found = false;
    for y in 0..h {
        for x in 0..w {
            if pix[(y * w + x) * 4 + 3] > ALPHA_FLOOR {
                found = true;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    // Nothing opaque enough: use the canvas and let it look wrong, rather than
    // return an empty box and divide by zero.
    if !found {
        return (0, 0, w, h);
    }
    (x0, y0, x1 + 1, y1 + 1)
}

/// Average every source pixel behind one icon pixel, clipped to the crop.
///
/// ⚠ Picking a single sample instead is what made the 3.x icon read as
/// speckle: a 214px illustration reduced to 16px by nearest-neighbour lands on
/// isolated pixels, so only 38% of the icon carried ink where neighbouring
/// tray icons carry 69%. Averaging keeps the strokes joined up and lifts that
/// to 68%.
fn average(
    pix: &[u8],
    w: usize,
    clip: (usize, usize, usize, usize),
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
) -> (u8, u8, u8, u8) {
    let mut ix0 = x0.floor() as isize;
    let mut iy0 = y0.floor() as isize;
    let mut ix1 = x1.floor() as isize;
    let mut iy1 = y1.floor() as isize;
    if ix1 <= ix0 {
        ix1 = ix0 + 1;
    }
    if iy1 <= iy0 {
        iy1 = iy0 + 1;
    }
    ix0 = ix0.max(clip.0 as isize);
    iy0 = iy0.max(clip.1 as isize);
    ix1 = ix1.min(clip.2 as isize);
    iy1 = iy1.min(clip.3 as isize);
    if ix1 <= ix0 || iy1 <= iy0 {
        return (0, 0, 0, 0);
    }
    let (mut sr, mut sg, mut sb, mut sa) = (0u64, 0u64, 0u64, 0u64);
    for y in iy0..iy1 {
        for x in ix0..ix1 {
            let i = (y as usize * w + x as usize) * 4;
            let a = pix[i + 3] as u64;
            // ⚠ Premultiply. Go reached these through color.RGBA(), which
            // premultiplies; the png crate hands back straight alpha, so doing
            // it here is what keeps the two ports identical.
            sr += pix[i] as u64 * a / 255;
            sg += pix[i + 1] as u64 * a / 255;
            sb += pix[i + 2] as u64 * a / 255;
            sa += a;
        }
    }
    let n = ((ix1 - ix0) * (iy1 - iy0)) as u64;
    ((sr / n) as u8, (sg / n) as u8, (sb / n) as u8, (sa / n) as u8)
}
