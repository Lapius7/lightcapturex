//! サムネイル。録画対象のプレビューと、録画ファイルの代表画像を BGRA で作る。
use crate::capture::Source;
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_THUMBNAILONLY};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

pub struct Thumb {
    pub w: u32,
    pub h: u32,
    /// 上から下へ並ぶ BGRA（アルファは不透明）
    pub bgra: Vec<u8>,
}

/// HBITMAP の中身を取り出す。真っ黒なら None（ウィンドウを取り込めなかった場合）
unsafe fn read(dc: HDC, hbm: HBITMAP, w: i32, h: i32, reject_black: bool) -> Option<Thumb> {
    let mut bi = BITMAPINFO::default();
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -h,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    if GetDIBits(dc, hbm, 0, h as u32, Some(buf.as_mut_ptr() as _), &mut bi, DIB_RGB_COLORS) == 0 {
        return None;
    }
    let lit = buf.chunks_exact(4).step_by(7).any(|p| p[0] > 8 || p[1] > 8 || p[2] > 8);
    if reject_black && !lit {
        return None;
    }
    buf.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    Some(Thumb { w: w as u32, h: h as u32, bgra: buf })
}

/// 録画対象の今の画面を、tw×th に収まるように縮めて返す
pub fn source(src: Source, tw: i32, th: i32) -> Option<Thumb> {
    unsafe {
        let (rect, hwnd) = match src {
            Source::Monitor(h) => {
                let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
                GetMonitorInfoW(HMONITOR(h as *mut _), &mut mi).as_bool().then_some(())?;
                (mi.rcMonitor, None)
            }
            Source::Window(h) => {
                let mut r = RECT::default();
                GetWindowRect(HWND(h as *mut _), &mut r).ok()?;
                (r, Some(HWND(h as *mut _)))
            }
        };
        let (sw, sh) = (rect.right - rect.left, rect.bottom - rect.top);
        if sw <= 0 || sh <= 0 {
            return None;
        }
        let s = (tw as f32 / sw as f32).min(th as f32 / sh as f32).min(1.0);
        let (ow, oh) = (((sw as f32 * s) as i32).max(2), ((sh as f32 * s) as i32).max(2));
        let screen = GetDC(None);
        let full_dc = CreateCompatibleDC(Some(screen));
        let full = CreateCompatibleBitmap(screen, sw, sh);
        let old = SelectObject(full_dc, full.into());
        let ok = match hwnd {
            Some(w) => PrintWindow(w, full_dc, PRINT_WINDOW_FLAGS(2)).as_bool(),
            None => BitBlt(full_dc, 0, 0, sw, sh, Some(screen), rect.left, rect.top, SRCCOPY).is_ok(),
        };
        let mut out = None;
        if ok {
            let small_dc = CreateCompatibleDC(Some(screen));
            let small = CreateCompatibleBitmap(screen, ow, oh);
            let old2 = SelectObject(small_dc, small.into());
            SetStretchBltMode(small_dc, HALFTONE);
            let _ = SetBrushOrgEx(small_dc, 0, 0, None);
            let _ = StretchBlt(small_dc, 0, 0, ow, oh, Some(full_dc), 0, 0, sw, sh, SRCCOPY);
            SelectObject(small_dc, old2);
            out = read(screen, small, ow, oh, hwnd.is_some());
            let _ = DeleteObject(small.into());
            let _ = DeleteDC(small_dc);
        }
        SelectObject(full_dc, old);
        let _ = DeleteObject(full.into());
        let _ = DeleteDC(full_dc);
        ReleaseDC(None, screen);
        out
    }
}

/// エクスプローラーと同じサムネイルを取り出す（別スレッドから呼ぶ）
pub fn file(path: &str, w: i32, h: i32) -> Option<Thumb> {
    unsafe {
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let f: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let hbm = f.GetImage(SIZE { cx: w, cy: h }, SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK).ok()?;
        let mut bm = BITMAP::default();
        GetObjectW(hbm.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as _));
        let screen = GetDC(None);
        let t = read(screen, hbm, bm.bmWidth, bm.bmHeight, false);
        ReleaseDC(None, screen);
        let _ = DeleteObject(hbm.into());
        t
    }
}
