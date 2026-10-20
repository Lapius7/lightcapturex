//! 素の Win32 コントロールだけで作った設定画面。F9 で録画の開始/停止。
use crate::capture::{list_sources, Source, SourceInfo};
use crate::recorder::{self, Recorder, Settings};
use crate::encoder::{self, Codec};
use crate::{audio, d3d, log};
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Instant;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{FOLDERID_Videos, SHGetKnownFolderPath, ShellExecuteW, KF_FLAG_DEFAULT};
use windows::Win32::UI::WindowsAndMessaging::*;

const ID_SOURCE: i32 = 101;
const ID_QUALITY: i32 = 102;
const ID_FPS: i32 = 103;
const ID_RES: i32 = 104;
const ID_MIC: i32 = 105;
const ID_SYS: i32 = 106;
const ID_CURSOR: i32 = 107;
const ID_REC: i32 = 108;
const ID_FOLDER: i32 = 109;
const ID_STATUS: i32 = 110;
const ID_CODEC: i32 = 111;
const ID_GPU: i32 = 112;
const ID_INFO: i32 = 113;
const ID_APPS: i32 = 114;
/// アプリ選択メニューの項目 ID（この値 + 一覧の番号）
const MENU_APP: usize = 1000;
const HOTKEY: i32 = 1;
const TIMER: usize = 1;

const QUALITIES: [&str; 4] = ["低", "中", "高", "最高"];
const FPS: [u32; 2] = [30, 60];
const RES: [(&str, u32); 4] = [("元のまま", 0), ("1080p", 1080), ("720p", 720), ("480p", 480)];
const SYS_MODES: [&str; 3] = ["録音しない", "すべての音", "選んだアプリだけ"];
const SYS_APPS: usize = 2;
/// 起動していなくてもメニューに出すアプリ（exe 名, 表示名）
const PRESET_APPS: [(&str, &str); 2] = [("VALORANT-Win64-Shipping.exe", "VALORANT"), ("Discord.exe", "Discord")];

#[derive(Default)]
struct Ui {
    hwnd: HWND,
    font: HFONT,
    ctrls: Vec<(HWND, [i32; 4])>,
    sources: Vec<SourceInfo>,
    mics: Vec<(String, String)>,
    codecs: Vec<Codec>,
    gpus: Vec<String>,
    /// 「選んだアプリだけ」で録るアプリの exe 名
    apps: Vec<String>,
    /// ユーザーが最後に選んだ録画対象。ゲームを後から起動しても F9 で自動的に選び直す
    preferred: Option<String>,
    rec: Option<(Recorder, Instant, PathBuf)>,
    last_file: Option<PathBuf>,
    hotkey_ok: bool,
}

thread_local! {
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn item(id: i32) -> HWND {
    UI.with(|u| unsafe { GetDlgItem(Some(u.borrow().hwnd), id).unwrap_or_default() })
}

fn send(h: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    unsafe { SendMessageW(h, msg, Some(WPARAM(wp)), Some(LPARAM(lp))).0 }
}

fn combo_fill(id: i32, items: &[String], sel: usize) {
    let h = item(id);
    send(h, CB_RESETCONTENT, 0, 0);
    for s in items {
        let w = wide(s);
        send(h, CB_ADDSTRING, 0, w.as_ptr() as isize);
    }
    send(h, CB_SETCURSEL, sel.min(items.len().saturating_sub(1)), 0);
}

fn combo_sel(id: i32) -> usize {
    send(item(id), CB_GETCURSEL, 0, 0).max(0) as usize
}

fn app_label(exe: &str) -> &str {
    if let Some((_, label)) = PRESET_APPS.iter().find(|(e, _)| e.eq_ignore_ascii_case(exe)) {
        return label;
    }
    exe.len().checked_sub(4).filter(|&i| exe.is_char_boundary(i) && exe[i..].eq_ignore_ascii_case(".exe")).map_or(exe, |i| &exe[..i])
}

fn update_apps_button() {
    let apps = UI.with(|u| u.borrow().apps.clone());
    let text = if apps.is_empty() { "アプリを選ぶ…".to_string() } else { apps.iter().map(|a| app_label(a)).collect::<Vec<_>>().join(", ") };
    set_text(ID_APPS, &text);
}

/// 音を出しているアプリと選択済みのアプリをメニューで出し、押したものの選択を切り替える
fn pick_apps() {
    let (hwnd, mut list) = UI.with(|u| (u.borrow().hwnd, u.borrow().apps.clone()));
    let selected = list.len();
    for a in PRESET_APPS.iter().map(|p| p.0.to_string()).chain(audio::audio_apps()) {
        if !list.iter().any(|x| x.eq_ignore_ascii_case(&a)) {
            list.push(a);
        }
    }
    let cmd = unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        for (i, n) in list.iter().enumerate() {
            let flag = if i < selected { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(menu, MF_STRING | flag, MENU_APP + i, &HSTRING::from(app_label(n)));
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_GRAYED, 0, w!("ほかのアプリは、起動して音を出すと表示されます"));
        let mut r = RECT::default();
        let _ = GetWindowRect(item(ID_APPS), &mut r);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY, r.left, r.bottom, None, hwnd, None).0 as usize;
        let _ = DestroyMenu(menu);
        cmd
    };
    let Some(name) = cmd.checked_sub(MENU_APP).and_then(|i| list.get(i)).cloned() else { return };
    let empty = UI.with(|u| {
        let apps = &mut u.borrow_mut().apps;
        match apps.iter().position(|x| x.eq_ignore_ascii_case(&name)) {
            Some(i) => {
                apps.remove(i);
            }
            None => apps.push(name),
        }
        apps.is_empty()
    });
    if !empty {
        send(item(ID_SYS), CB_SETCURSEL, SYS_APPS, 0);
    }
    update_apps_button();
}

fn checked(id: i32) -> bool {
    send(item(id), BM_GETCHECK, 0, 0) == 1
}

fn set_text(id: i32, s: &str) {
    let w = wide(s);
    unsafe { let _ = SetWindowTextW(item(id), PCWSTR(w.as_ptr())); }
}

// ---- 設定の保存（%APPDATA%\LightCaptureX\settings.ini に key=value）----

fn config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("LightCaptureX").join("settings.ini")
}

fn load_config() -> Vec<(String, String)> {
    std::fs::read_to_string(config_path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect()
}

fn cfg<'a>(c: &'a [(String, String)], k: &str) -> Option<&'a str> {
    c.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.as_str())
}

fn save_config() {
    let (src, mic, codec, gpu, apps) = UI.with(|u| {
        let u = u.borrow();
        let src = u.preferred.clone().unwrap_or_default();
        let m = combo_sel(ID_MIC);
        let mic = if m == 0 { String::new() } else { u.mics.get(m - 1).map(|x| x.0.clone()).unwrap_or_default() };
        let codec = u.codecs.get(combo_sel(ID_CODEC)).map(|c| c.label()).unwrap_or_default();
        let g = combo_sel(ID_GPU);
        let gpu = if g == 0 { String::new() } else { u.gpus.get(g - 1).cloned().unwrap_or_default() };
        (src, mic, codec, gpu, u.apps.join("|"))
    });
    let text = format!(
        "source={src}\nquality={}\nfps={}\nres={}\nmic={mic}\nsysmode={}\napps={apps}\ncursor={}\ncodec={codec}\ngpu={gpu}\n",
        combo_sel(ID_QUALITY),
        combo_sel(ID_FPS),
        combo_sel(ID_RES),
        combo_sel(ID_SYS),
        checked(ID_CURSOR) as u8
    );
    let p = config_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let _ = std::fs::write(p, text);
}

// ---- 一覧の更新 ----

/// keep に挙げた名前のうち、最初に見つかったものを選択する
fn refresh_sources(keep: &[Option<String>]) {
    let hwnd = UI.with(|u| u.borrow().hwnd);
    let list = list_sources(hwnd);
    let names: Vec<String> = list.iter().map(|s| s.name.clone()).collect();
    let sel = keep.iter().flatten().find_map(|k| names.iter().position(|n| n == k)).unwrap_or(0);
    UI.with(|u| u.borrow_mut().sources = list);
    combo_fill(ID_SOURCE, &names, sel);
}

fn refresh_mics(keep: Option<String>) {
    let mics = audio::list_mics();
    let mut names = vec!["なし".to_string()];
    names.extend(mics.iter().map(|m| m.1.clone()));
    let sel = keep.and_then(|k| mics.iter().position(|m| m.0 == k)).map(|i| i + 1).unwrap_or(0);
    UI.with(|u| u.borrow_mut().mics = mics);
    combo_fill(ID_MIC, &names, sel);
}

fn selected_source_name() -> Option<String> {
    UI.with(|u| u.borrow().sources.get(combo_sel(ID_SOURCE)).map(|s| s.name.clone()))
}

fn selected_mic_id() -> Option<String> {
    let m = combo_sel(ID_MIC);
    if m == 0 {
        return None;
    }
    UI.with(|u| u.borrow().mics.get(m - 1).map(|x| x.0.clone()))
}

// ---- 録画 ----

fn output_dir() -> PathBuf {
    let videos = unsafe {
        SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None)
            .ok()
            .and_then(|p| {
                let s = p.to_string().ok();
                windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _));
                s
            })
    };
    videos.map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("LightCaptureX")
}

fn new_file_path() -> PathBuf {
    let t = unsafe { GetLocalTime() };
    output_dir().join(format!(
        "LightCaptureX_{:04}{:02}{:02}_{:02}{:02}{:02}.mp4",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    ))
}

fn set_recording_ui(on: bool) {
    for id in [ID_SOURCE, ID_QUALITY, ID_FPS, ID_RES, ID_CODEC, ID_GPU, ID_MIC, ID_SYS, ID_APPS, ID_CURSOR] {
        unsafe { let _ = EnableWindow(item(id), !on); }
    }
    set_text(ID_REC, if on { "■ 停止 (F9)" } else { "● 録画開始 (F9)" });
}

fn toggle() {
    let running = UI.with(|u| u.borrow().rec.is_some());
    if running {
        stop_recording();
    } else {
        start_recording();
    }
}

fn start_recording() {
    // 起動後に開いたゲームのウィンドウも拾えるよう、開始直前に一覧を取り直す
    let preferred = UI.with(|u| u.borrow().preferred.clone());
    refresh_sources(&[preferred, selected_source_name()]);
    let source: Option<Source> = UI.with(|u| u.borrow().sources.get(combo_sel(ID_SOURCE)).map(|s| s.source));
    let Some(source) = source else { return };
    let path = new_file_path();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let s = Settings {
        source,
        fps: FPS[combo_sel(ID_FPS).min(1)],
        quality: combo_sel(ID_QUALITY) as u32,
        max_height: RES[combo_sel(ID_RES).min(RES.len() - 1)].1,
        cursor: checked(ID_CURSOR),
        system_audio: match combo_sel(ID_SYS) {
            0 => false,
            SYS_APPS => !UI.with(|u| u.borrow().apps.is_empty()),
            _ => true,
        },
        apps: if combo_sel(ID_SYS) == SYS_APPS { UI.with(|u| u.borrow().apps.clone()) } else { Vec::new() },
        mic: selected_mic_id(),
        codec: UI.with(|u| u.borrow().codecs.get(combo_sel(ID_CODEC)).copied()).unwrap_or(Codec::H264),
        gpu: {
            let g = combo_sel(ID_GPU);
            if g == 0 { None } else { UI.with(|u| u.borrow().gpus.get(g - 1).cloned()) }
        },
        path: path.to_string_lossy().into_owned(),
    };
    save_config();
    set_text(ID_STATUS, "開始中…");
    match recorder::start(s) {
        Ok(r) => {
            set_text(ID_INFO, &r.info);
            UI.with(|u| u.borrow_mut().rec = Some((r, Instant::now(), path)));
            set_recording_ui(true);
            tick();
            let hwnd = UI.with(|u| u.borrow().hwnd);
            unsafe { SetTimer(Some(hwnd), TIMER, 500, None) };
        }
        Err(e) => {
            log(&format!("start: {e}"));
            set_text(ID_STATUS, &format!("開始できませんでした: {}", e.message()));
        }
    }
}

fn stop_recording() {
    let Some((r, _, path)) = UI.with(|u| u.borrow_mut().rec.take()) else { return };
    let hwnd = UI.with(|u| u.borrow().hwnd);
    unsafe { let _ = KillTimer(Some(hwnd), TIMER); }
    set_text(ID_STATUS, "保存中…");
    let res = r.stop();
    set_recording_ui(false);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match res {
        Ok(()) => set_text(ID_STATUS, &format!("保存しました: {name}")),
        Err(e) => {
            log(&format!("stop: {e}"));
            set_text(ID_STATUS, &format!("エラー: {}", e.message()));
        }
    }
    UI.with(|u| u.borrow_mut().last_file = Some(path));
}

fn tick() {
    let st = UI.with(|u| u.borrow().rec.as_ref().map(|r| (r.1.elapsed().as_secs(), std::fs::metadata(&r.2).map(|m| m.len()).unwrap_or(0))));
    if let Some((s, size)) = st {
        let mb = size as f64 / 1_048_576.0;
        set_text(ID_STATUS, &format!("● 録画中  {:02}:{:02}:{:02}   {mb:.1} MB", s / 3600, s / 60 % 60, s % 60));
    }
}

fn open_folder() {
    let dir = output_dir();
    let _ = std::fs::create_dir_all(&dir);
    let last = UI.with(|u| u.borrow().last_file.clone()).filter(|p| p.exists());
    unsafe {
        match last {
            // 直前に保存したファイルを選択した状態で開く
            Some(f) => ShellExecuteW(None, w!("open"), w!("explorer.exe"), &HSTRING::from(format!("/select,\"{}\"", f.display())), None, SW_SHOWNORMAL),
            None => ShellExecuteW(None, w!("open"), &HSTRING::from(dir.as_os_str()), None, None, SW_SHOWNORMAL),
        };
    }
}

// ---- 画面 ----

/// 論理座標(96dpi)で配置を書き、DPI に合わせて拡大する
fn layout(hwnd: HWND) {
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as i32;
    let sc = |v: i32| v * dpi / 96;
    let font = unsafe {
        let mut ncm = NONCLIENTMETRICSW { cbSize: size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        let _ = SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, ncm.cbSize, Some(&mut ncm as *mut _ as *mut _), 0, dpi as u32);
        CreateFontIndirectW(&ncm.lfMessageFont)
    };
    UI.with(|u| {
        let mut u = u.borrow_mut();
        if !u.font.is_invalid() {
            unsafe { let _ = DeleteObject(u.font.into()); }
        }
        u.font = font;
        for (h, r) in &u.ctrls {
            unsafe {
                let _ = MoveWindow(*h, sc(r[0]), sc(r[1]), sc(r[2]), sc(r[3]), false);
                SendMessageW(*h, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            }
        }
    });
    let mut rc = RECT { left: 0, top: 0, right: sc(WIDTH), bottom: sc(HEIGHT) };
    unsafe {
        let _ = windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi(&mut rc, STYLE, false, WINDOW_EX_STYLE(0), dpi as u32);
        let _ = SetWindowPos(hwnd, None, 0, 0, rc.right - rc.left, rc.bottom - rc.top, SWP_NOMOVE | SWP_NOZORDER);
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

const WIDTH: i32 = 380;
const HEIGHT: i32 = 322;
const STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0);

fn create_controls(hwnd: HWND) {
    let inst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let add = |class: PCWSTR, text: &str, style: u32, id: i32, r: [i32; 4]| {
        let t = wide(text);
        let h = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                PCWSTR(t.as_ptr()),
                WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | style),
                0,
                0,
                0,
                0,
                Some(hwnd),
                Some(HMENU(id as isize as *mut _)),
                Some(inst.into()),
                None,
            )
            .unwrap_or_default()
        };
        UI.with(|u| u.borrow_mut().ctrls.push((h, r)));
    };
    let combo = (CBS_DROPDOWNLIST as u32) | WS_VSCROLL.0 | WS_TABSTOP.0;
    let label = 0x200; // SS_CENTERIMAGE
    let (lx, cx, row) = (16, 84, 32);
    let y = |i: i32| 16 + i * row;
    add(w!("STATIC"), "録画対象", label, -1, [lx, y(0), 64, 24]);
    add(w!("COMBOBOX"), "", combo, ID_SOURCE, [cx, y(0), 280, 300]);
    add(w!("STATIC"), "画質", label, -1, [lx, y(1), 64, 24]);
    add(w!("COMBOBOX"), "", combo, ID_QUALITY, [cx, y(1), 80, 200]);
    add(w!("STATIC"), "FPS", label, -1, [176, y(1), 30, 24]);
    add(w!("COMBOBOX"), "", combo, ID_FPS, [208, y(1), 56, 200]);
    add(w!("COMBOBOX"), "", combo, ID_RES, [272, y(1), 92, 200]);
    add(w!("STATIC"), "エンコード", label, -1, [lx, y(2), 64, 24]);
    add(w!("COMBOBOX"), "", combo, ID_CODEC, [cx, y(2), 80, 200]);
    add(w!("STATIC"), "GPU", label, -1, [176, y(2), 30, 24]);
    add(w!("COMBOBOX"), "", combo, ID_GPU, [208, y(2), 156, 200]);
    add(w!("STATIC"), "マイク", label, -1, [lx, y(3), 64, 24]);
    add(w!("COMBOBOX"), "", combo, ID_MIC, [cx, y(3), 280, 300]);
    let btn = WS_TABSTOP.0;
    add(w!("STATIC"), "PC の音", label, -1, [lx, y(4), 64, 24]);
    add(w!("COMBOBOX"), "", combo, ID_SYS, [cx, y(4), 124, 200]);
    add(w!("BUTTON"), "アプリを選ぶ…", btn, ID_APPS, [216, y(4) - 1, 148, 26]);
    let check = (BS_AUTOCHECKBOX as u32) | WS_TABSTOP.0;
    add(w!("BUTTON"), "カーソルを録画", check, ID_CURSOR, [cx, y(5), 140, 24]);
    add(w!("BUTTON"), "● 録画開始 (F9)", btn | BS_DEFPUSHBUTTON as u32, ID_REC, [lx, y(6) + 8, 220, 34]);
    add(w!("BUTTON"), "保存先を開く", btn, ID_FOLDER, [244, y(6) + 8, 120, 34]);
    let text = 0xC | 0x4000; // SS_LEFTNOWORDWRAP | SS_ENDELLIPSIS
    add(w!("STATIC"), "", text, ID_STATUS, [lx, y(7) + 18, 348, 20]);
    add(w!("STATIC"), "", text, ID_INFO, [lx, y(7) + 40, 348, 20]);
}

fn init_values() {
    let c = load_config();
    let num = |k: &str, d: usize| cfg(&c, k).and_then(|v| v.parse().ok()).unwrap_or(d);
    let preferred = cfg(&c, "source").filter(|s| !s.is_empty()).map(String::from);
    UI.with(|u| u.borrow_mut().preferred = preferred.clone());
    refresh_sources(&[preferred]);
    combo_fill(ID_QUALITY, &QUALITIES.map(String::from), num("quality", 2));
    combo_fill(ID_FPS, &FPS.map(|f| f.to_string()), num("fps", 1));
    combo_fill(ID_RES, &RES.map(|r| r.0.to_string()), num("res", 0));
    let codecs = encoder::available_codecs();
    let codecs = if codecs.is_empty() { vec![Codec::H264] } else { codecs };
    let csel = cfg(&c, "codec").and_then(|v| codecs.iter().position(|x| x.label() == v)).unwrap_or(0);
    combo_fill(ID_CODEC, &codecs.iter().map(|x| x.label().to_string()).collect::<Vec<_>>(), csel);
    let gpus = d3d::adapter_names();
    let gsel = cfg(&c, "gpu").and_then(|v| gpus.iter().position(|x| x == v)).map(|i| i + 1).unwrap_or(0);
    let mut gnames = vec!["自動".to_string()];
    gnames.extend(gpus.iter().cloned());
    combo_fill(ID_GPU, &gnames, gsel);
    UI.with(|u| {
        let mut u = u.borrow_mut();
        u.codecs = codecs;
        u.gpus = gpus;
    });
    refresh_mics(cfg(&c, "mic").filter(|s| !s.is_empty()).map(String::from));
    // 旧設定（system=0/1）からも引き継ぐ
    combo_fill(ID_SYS, &SYS_MODES.map(String::from), num("sysmode", num("system", 1)));
    let apps = cfg(&c, "apps").unwrap_or("").split('|').filter(|a| !a.is_empty()).map(String::from).collect();
    UI.with(|u| u.borrow_mut().apps = apps);
    update_apps_button();
    send(item(ID_CURSOR), BM_SETCHECK, num("cursor", 1), 0);
    let hk = UI.with(|u| u.borrow().hotkey_ok);
    set_text(ID_STATUS, if hk { "待機中" } else { "待機中（F9 は他のアプリが使用中）" });
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            UI.with(|u| u.borrow_mut().hwnd = hwnd);
            create_controls(hwnd);
            layout(hwnd);
            let ok = unsafe { RegisterHotKey(Some(hwnd), HOTKEY, MOD_NOREPEAT, VK_F9.0 as u32).is_ok() };
            UI.with(|u| u.borrow_mut().hotkey_ok = ok);
            init_values();
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wp.0 & 0xffff) as i32;
            let code = (wp.0 >> 16) as u32;
            match (id, code) {
                (ID_REC, _) => toggle(),
                (ID_FOLDER, _) => open_folder(),
                (ID_APPS, _) => pick_apps(),
                // 開くたびに一覧を取り直す（新しく起動したゲームも選べるように）
                (ID_SOURCE, CBN_DROPDOWN) => refresh_sources(&[selected_source_name()]),
                (ID_SOURCE, CBN_SELCHANGE) => UI.with(|u| u.borrow_mut().preferred = selected_source_name()),
                (ID_MIC, CBN_DROPDOWN) => refresh_mics(selected_mic_id()),
                _ => {}
            }
            LRESULT(0)
        }
        WM_HOTKEY if wp.0 as i32 == HOTKEY => {
            toggle();
            LRESULT(0)
        }
        WM_TIMER => {
            tick();
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => unsafe {
            SetBkMode(HDC(wp.0 as *mut _), TRANSPARENT);
            LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize)
        },
        WM_DPICHANGED => {
            layout(hwnd);
            let r = unsafe { &*(lp.0 as *const RECT) };
            unsafe { let _ = SetWindowPos(hwnd, None, r.left, r.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER); }
            LRESULT(0)
        }
        WM_CLOSE => {
            stop_recording();
            save_config();
            unsafe { let _ = DestroyWindow(hwnd); }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

pub fn run() {
    unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            hIcon: LoadIconW(Some(inst.into()), PCWSTR(1 as _)).unwrap_or_default(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            lpszClassName: w!("LightCaptureX"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("LightCaptureX"),
            w!("LightCaptureX"),
            STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            WIDTH,
            HEIGHT,
            None,
            None,
            Some(inst.into()),
            None,
        ) else {
            return;
        };
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if IsDialogMessageW(hwnd, &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
