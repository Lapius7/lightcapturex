//! 音声アプリ選択用に、いま起動しているアプリ（ウィンドウを持つもの・音を出しているもの）を集める

use crate::audio;
use crate::gfx::gfx;
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Imaging::IWICBitmap;
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 起動していなくても一覧に出すアプリ（exe 名, 表示名）
pub const PRESETS: [(&str, &str); 2] = [("VALORANT-Win64-Shipping.exe", "VALORANT"), ("Discord.exe", "Discord")];
/// 一覧に出しても意味のないシステムのプロセス
const SKIP: [&str; 4] = ["ApplicationFrameHost.exe", "TextInputHost.exe", "explorer.exe", "SystemSettings.exe"];

#[derive(Clone)]
pub struct AppEntry {
    pub exe: String,
    pub name: String,
    pub running: bool,
    /// 既定の再生デバイスで、いま音を出している
    pub playing: bool,
    pub icon: Option<IWICBitmap>,
}

pub fn preset_label(exe: &str) -> Option<&'static str> {
    PRESETS.iter().find(|(e, _)| e.eq_ignore_ascii_case(exe)).map(|p| p.1)
}

/// 表示名の既定値（プリセットの名前か、exe 名から .exe を除いたもの）
pub fn short_name(exe: &str) -> String {
    if let Some(l) = preset_label(exe) {
        return l.to_string();
    }
    let n = exe.len().checked_sub(4).filter(|&i| exe.is_char_boundary(i) && exe[i..].eq_ignore_ascii_case(".exe")).map_or(exe, |i| &exe[..i]);
    n.to_string()
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lp: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lp.0 as *mut Vec<(HWND, u32)>) };
    unsafe {
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let owner = GetWindow(hwnd, GW_OWNER).unwrap_or_default();
        if !IsWindowVisible(hwnd).as_bool() || !owner.is_invalid() || ex & WS_EX_TOOLWINDOW.0 != 0 || GetWindowTextLengthW(hwnd) == 0 {
            return BOOL(1);
        }
        // 別の仮想デスクトップや、閉じたあとも残る UWP のウィンドウは除く
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
        if cloaked != 0 {
            return BOOL(1);
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        out.push((hwnd, pid));
    }
    BOOL(1)
}

fn image_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// exe の「ファイルの説明」（例: Discord.exe → Discord）
fn file_description(path: &str) -> Option<String> {
    unsafe {
        let p = HSTRING::from(path);
        let size = GetFileVersionInfoSizeW(&p, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(&p, None, size, data.as_mut_ptr() as _).ok()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        let sub = |q: &str, ptr: &mut *mut std::ffi::c_void, len: &mut u32| VerQueryValueW(data.as_ptr() as _, &HSTRING::from(q), ptr, len).as_bool() && *len > 0;
        let lang = if sub("\\VarFileInfo\\Translation", &mut ptr, &mut len) {
            let t = *(ptr as *const [u16; 2]);
            format!("{:04x}{:04x}", t[0], t[1])
        } else {
            "040904b0".to_string()
        };
        if !sub(&format!("\\StringFileInfo\\{lang}\\FileDescription"), &mut ptr, &mut len) {
            return None;
        }
        let s = PCWSTR(ptr as *const u16).to_string().ok()?;
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

/// アイコンを WIC のビットマップにする。exe から取れないとき（アンチチートで開けない等）はウィンドウのアイコンを使う
fn icon_for(path: Option<&str>, hwnd: Option<HWND>, px: i32) -> Option<IWICBitmap> {
    let wic = gfx().wic;
    unsafe {
        if let Some(p) = path {
            let mut name = [0u16; 260];
            for (d, s) in name.iter_mut().zip(p.encode_utf16().take(259)) {
                *d = s;
            }
            let mut icons = [HICON::default()];
            if PrivateExtractIconsW(&name, 0, px, px, Some(&mut icons), None, 0) >= 1 && !icons[0].is_invalid() {
                let b = wic.CreateBitmapFromHICON(icons[0]).ok();
                let _ = DestroyIcon(icons[0]);
                if b.is_some() {
                    return b;
                }
            }
        }
        // ウィンドウのアイコンは相手のプロセスが持っているので、破棄しない
        let hwnd = hwnd?;
        let mut res = 0usize;
        for kind in [ICON_BIG, ICON_SMALL2] {
            let ok = SendMessageTimeoutW(hwnd, WM_GETICON, WPARAM(kind as usize), LPARAM(0), SMTO_ABORTIFHUNG, 100, Some(&mut res));
            if ok.0 != 0 && res != 0 {
                return wic.CreateBitmapFromHICON(HICON(res as _)).ok();
            }
        }
        let h = GetClassLongPtrW(hwnd, GCLP_HICON);
        (h != 0).then(|| wic.CreateBitmapFromHICON(HICON(h as _)).ok()).flatten()
    }
}

/// 起動中のアプリと、選択済み・プリセットのアプリをまとめた一覧（選択済み→再生中→名前の順）
pub fn gather(selected: &[String], icon_px: i32) -> Vec<AppEntry> {
    let me = std::process::id();
    let procs = audio::processes();
    let exe_of = |pid: u32| procs.iter().find(|p| p.0 == pid).map(|p| p.2.clone());
    let mut wins: Vec<(HWND, u32)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut wins as *mut _ as isize));
    }
    let sessions = audio::audio_sessions();

    // (exe, 代表の pid, 代表のウィンドウ, 再生中)
    let mut found: Vec<(String, u32, Option<HWND>, bool)> = Vec::new();
    let mut add = |pid: u32, hwnd: Option<HWND>, playing: bool| {
        if pid == 0 || pid == me {
            return;
        }
        let Some(exe) = exe_of(pid) else { return };
        if SKIP.iter().any(|s| s.eq_ignore_ascii_case(&exe)) {
            return;
        }
        match found.iter_mut().find(|f| f.0.eq_ignore_ascii_case(&exe)) {
            Some(f) => {
                f.3 |= playing;
                if f.2.is_none() && hwnd.is_some() {
                    f.1 = pid;
                    f.2 = hwnd;
                }
            }
            None => found.push((exe, pid, hwnd, playing)),
        }
    };
    for &(h, pid) in &wins {
        add(pid, Some(h), false);
    }
    for &(pid, active) in &sessions {
        add(pid, None, active);
    }

    let mut out: Vec<AppEntry> = found
        .into_iter()
        .map(|(exe, pid, hwnd, playing)| {
            let path = image_path(pid);
            let name = preset_label(&exe)
                .map(String::from)
                .or_else(|| path.as_deref().and_then(file_description))
                .unwrap_or_else(|| short_name(&exe));
            let icon = icon_for(path.as_deref(), hwnd, icon_px);
            AppEntry { exe, name, running: true, playing, icon }
        })
        .collect();
    for exe in selected.iter().map(String::as_str).chain(PRESETS.iter().map(|p| p.0)) {
        if !out.iter().any(|e| e.exe.eq_ignore_ascii_case(exe)) {
            out.push(AppEntry { exe: exe.to_string(), name: short_name(exe), running: false, playing: false, icon: None });
        }
    }
    let is_sel = |e: &AppEntry| selected.iter().any(|s| s.eq_ignore_ascii_case(&e.exe));
    out.sort_by_cached_key(|e| (!is_sel(e), !e.playing, !e.running, e.name.to_lowercase()));
    out
}
