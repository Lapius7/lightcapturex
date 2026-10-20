//! タスクトレーのアイコン・通知・右クリックメニュー。
use std::sync::Mutex;
use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// トレーのアイコンから届くメッセージ
pub const WM_TRAY: u32 = WM_APP + 1;
const UID: u32 = 1;

pub enum Item {
    Cmd { id: u32, text: String, checked: bool, enabled: bool },
    Sub { text: String, items: Vec<Item>, enabled: bool },
    Sep,
}

impl Item {
    pub fn cmd(id: u32, text: impl Into<String>) -> Self {
        Item::Cmd { id, text: text.into(), checked: false, enabled: true }
    }
    pub fn on(self, enabled: bool) -> Self {
        match self {
            Item::Cmd { id, text, checked, .. } => Item::Cmd { id, text, checked, enabled },
            Item::Sub { text, items, .. } => Item::Sub { text, items, enabled },
            s => s,
        }
    }
    pub fn checked(self, checked: bool) -> Self {
        match self {
            Item::Cmd { id, text, enabled, .. } => Item::Cmd { id, text, checked, enabled },
            s => s,
        }
    }
    pub fn sub(text: impl Into<String>, items: Vec<Item>) -> Self {
        Item::Sub { text: text.into(), items, enabled: true }
    }
}

/// (通常のアイコン, 録画中の赤いアイコン)
static ICONS: Mutex<Option<(isize, isize)>> = Mutex::new(None);

fn small() -> i32 {
    unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16)
}

fn app_icon() -> HICON {
    unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        let n = small();
        match LoadImageW(Some(inst.into()), PCWSTR(1 as _), IMAGE_ICON, n, n, LR_DEFAULTCOLOR) {
            Ok(h) => HICON(h.0),
            Err(_) => LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        }
    }
}

/// 録画中を示す赤い丸のアイコンを描く
fn red_icon() -> HICON {
    unsafe {
        let n = small();
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: n,
                biHeight: -n,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0) else { return HICON::default() };
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (n * n) as usize);
        let c = n as f32 / 2.0;
        let rad = c - 1.0;
        for y in 0..n {
            for x in 0..n {
                let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt();
                let a = (rad - d + 0.5).clamp(0.0, 1.0);
                // 縁を少し暗くして立体感を出す
                let k = if d > rad - 1.5 { 0.82 } else { 1.0 };
                let (r, g, b) = ((0xE5 as f32 * k) as u32, (0x48 as f32 * k) as u32, (0x4D as f32 * k) as u32);
                px[(y * n + x) as usize] = ((a * 255.0) as u32) << 24 | r << 16 | g << 8 | b;
            }
        }
        let mask = CreateBitmap(n, n, 1, 1, None);
        let info = ICONINFO { fIcon: TRUE, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

fn wide<const N: usize>(s: &str) -> [u16; N] {
    let mut a = [0u16; N];
    for (d, c) in a.iter_mut().zip(s.encode_utf16().take(N - 1)) {
        *d = c;
    }
    a
}

fn data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: UID, ..Default::default() }
}

/// トレーにアイコンを出す（Explorer の再起動後にも呼ぶ）
pub fn add(hwnd: HWND, tip: &str) {
    let mut icons = ICONS.lock().unwrap();
    let (n, r) = *icons.get_or_insert_with(|| (app_icon().0 as isize, red_icon().0 as isize));
    let _ = r;
    let mut d = data(hwnd);
    d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    d.uCallbackMessage = WM_TRAY;
    d.hIcon = HICON(n as _);
    d.szTip = wide(tip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &d);
    }
}

pub fn remove(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data(hwnd));
    }
}

/// アイコンとツールチップを切り替える（録画中の点滅用）
pub fn set(hwnd: HWND, red: bool, tip: &str) {
    let Some((n, r)) = *ICONS.lock().unwrap() else { return };
    let mut d = data(hwnd);
    d.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    d.hIcon = HICON(if red { r } else { n } as _);
    d.szTip = wide(tip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
    }
}

/// 音を鳴らさない通知
pub fn balloon(hwnd: HWND, title: &str, text: &str) {
    let mut d = data(hwnd);
    d.uFlags = NIF_INFO;
    d.szInfoTitle = wide(title);
    d.szInfo = wide(text);
    d.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
    }
}

/// メニューをダークにする（uxtheme の SetPreferredAppMode(ForceDark)。起動時に 1 回）
pub fn dark_menus() {
    unsafe {
        let Ok(m) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        if let Some(f) = GetProcAddress(m, PCSTR(135 as *const u8)) {
            let f: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            f(2);
        }
        if let Some(f) = GetProcAddress(m, PCSTR(136 as *const u8)) {
            let f: extern "system" fn() = std::mem::transmute(f);
            f();
        }
    }
}

fn build(items: &[Item]) -> HMENU {
    unsafe {
        let m = CreatePopupMenu().unwrap_or_default();
        for it in items {
            match it {
                Item::Sep => {
                    let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());
                }
                Item::Cmd { id, text, checked, enabled } => {
                    let mut f = MF_STRING;
                    if *checked {
                        f |= MF_CHECKED;
                    }
                    if !*enabled {
                        f |= MF_GRAYED;
                    }
                    let _ = AppendMenuW(m, f, *id as usize, &windows::core::HSTRING::from(text.as_str()));
                }
                Item::Sub { text, items, enabled } => {
                    let sub = build(items);
                    let mut f = MF_STRING | MF_POPUP;
                    if !*enabled {
                        f |= MF_GRAYED;
                    }
                    let _ = AppendMenuW(m, f, sub.0 as usize, &windows::core::HSTRING::from(text.as_str()));
                }
            }
        }
        m
    }
}

/// カーソルの位置にメニューを出し、選ばれた項目の ID を返す
pub fn popup(hwnd: HWND, items: &[Item]) -> Option<u32> {
    unsafe {
        let menu = build(items);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // 前面にしてから開く。しないと、外側をクリックしてもメニューが閉じない
        let _ = SetForegroundWindow(hwnd);
        let r = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        Some(r.0 as u32).filter(|&v| v != 0)
    }
}
