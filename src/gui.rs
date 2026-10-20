//! Direct2D で自前描画する設定画面。録画の開始/停止はショートカット（既定は F9）
//! 配置は `ctl_rect` の1か所で決め、描画・当たり判定・画像出力（--render-ui）で共有する。
use crate::apps::{self, AppEntry};
use crate::capture::{list_sources, Source, SourceInfo};
use crate::encoder::{self, Codec};
use crate::gfx::{glyph, tok, Align, Font, Painter, C, R};
use crate::recorder::{self, Recorder, Settings};
use crate::recs::{self, RecEntry};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use crate::{audio, d3d, log, stats, sysmon, thumb, tray, update};
use std::cell::RefCell;
use std::path::PathBuf;
use std::thread::LocalKey;
use std::time::Instant;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::IWICBitmap;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{FOLDERID_Videos, SHGetKnownFolderPath, ShellExecuteW, KF_FLAG_DEFAULT};
use windows::Win32::UI::WindowsAndMessaging::*;

const HOTKEY: i32 = 1;
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER: usize = 1;
const METER: usize = 2;
/// 別のインスタンスから「画面を出して」と頼むメッセージ
const WM_SHOWME: u32 = WM_APP + 2;
static ANNOUNCED: AtomicBool = AtomicBool::new(false);

const QUALITIES: [&str; 4] = ["低", "中", "高", "最高"];
const FPS: [u32; 5] = [30, 60, 120, 144, 240];
const RES: [(&str, u32); 4] = [("元のまま", 0), ("1080p", 1080), ("720p", 720), ("480p", 480)];
const SYS_MODES: [&str; 3] = ["なし", "すべて", "アプリ"];
const SYS_APPS: usize = 2;

/// 操作できる部品。並び順がそのまま Tab の順になる
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ctl {
    Source,
    Quality,
    Fps,
    Res,
    Codec,
    Gpu,
    Bitrate,
    Cursor,
    Sys,
    Apps,
    Mic,
    Rec,
    Folder,
    Tab,
    ListRefresh,
    ListOpen,
    ListShow,
    ListDel,
    SetDir,
    SetDirOpen,
    SetKey,
    SetKeyReset,
    TogTray,
    TogNotify,
    TogUpd,
    UpdBtn,
}
const ORDER: [Ctl; 23] = [
    Ctl::Tab,
    Ctl::Source,
    Ctl::Quality,
    Ctl::Fps,
    Ctl::Res,
    Ctl::Codec,
    Ctl::Gpu,
    Ctl::Bitrate,
    Ctl::Cursor,
    Ctl::Sys,
    Ctl::Apps,
    Ctl::Mic,
    Ctl::Rec,
    Ctl::Folder,
    Ctl::ListRefresh,
    Ctl::SetDir,
    Ctl::SetDirOpen,
    Ctl::SetKey,
    Ctl::SetKeyReset,
    Ctl::TogTray,
    Ctl::TogNotify,
    Ctl::TogUpd,
    Ctl::UpdBtn,
];

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Tab {
    #[default]
    Rec,
    List,
    Set,
}
const TABS: [Tab; 3] = [Tab::Rec, Tab::List, Tab::Set];

/// 部品と、切り替えボタンなら何番目か
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Hit {
    ctl: Ctl,
    idx: usize,
}

#[derive(Clone, Default, PartialEq)]
enum Status {
    #[default]
    Ready,
    Starting,
    Recording,
    Saving,
    Saved(String),
    /// (見出し, 詳細)
    Failed(String, String),
}

#[derive(Default)]
struct Ui {
    hwnd: HWND,
    dpi: u32,
    sources: Vec<SourceInfo>,
    src: usize,
    mics: Vec<(String, String)>,
    /// 0 は「なし」
    mic: usize,
    codecs: Vec<Codec>,
    codec: usize,
    gpus: Vec<String>,
    /// 0 は「自動」
    gpu: usize,
    quality: usize,
    fps: usize,
    res: usize,
    sys: usize,
    cursor: bool,
    /// 「アプリ」で録るアプリの exe 名
    apps: Vec<String>,
    /// exe 名（小文字）ごとのアイコン
    icons: Vec<(String, IWICBitmap)>,
    /// exe 名（小文字）ごとの表示名
    names: Vec<(String, String)>,
    /// ユーザーが最後に選んだ録画対象。ゲームを後から起動しても F9 で自動的に選び直す
    preferred: Option<String>,
    rec: Option<(Recorder, Instant, PathBuf)>,
    last_file: Option<PathBuf>,
    out_dir: String,
    hotkey_ok: bool,
    tab: Tab,
    free: Option<u64>,
    /// ショートカット（修飾キー MOD_*, 仮想キー）
    hotkey: (u32, u32),
    /// ショートカットの入力待ち
    capturing: bool,
    /// 画面に出す短い通知（ショートカットが使えないときなど）
    note: Option<String>,
    tray_close: bool,
    notify: bool,
    auto_update: bool,
    /// BITRATES の何番目か（0 は自動）
    bitrate: usize,
    tray_on: bool,
    list: Vec<RecEntry>,
    /// 録画ファイルのサムネイル（パスごと）と、作成中のパス
    thumbs: Vec<(String, IWICBitmap)>,
    thumb_busy: Vec<String>,
    list_off: usize,
    // 録画中の数値
    fps_now: f32,
    dropped: u64,
    mbps: f32,
    cpu: Option<u32>,
    gpu_load: Option<u32>,
    fps_hist: Vec<f32>,
    last_fresh: u64,
    last_bytes: u64,
    last_t: Option<Instant>,
    /// 音量メーター（PC の音, マイク）0..1 と、ピークの保持
    meter: [f32; 2],
    hold: [f32; 2],
    hold_age: [u32; 2],
    status: Status,
    info: String,
    elapsed: u64,
    size_mb: f64,
    blink: bool,
    hover: Option<Hit>,
    press: Option<Hit>,
    focus: Option<Ctl>,
    /// キーボードで操作しているときだけフォーカスの枠を出す
    kb: bool,
    /// 一覧を開いている部品
    open: Option<Ctl>,
    tracking: bool,
}

type Slot = LocalKey<RefCell<Option<Painter>>>;

thread_local! {
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
    static POP: RefCell<Option<Pop>> = const { RefCell::new(None) };
    static PICK: RefCell<Option<Pick>> = const { RefCell::new(None) };
    // 描画先は状態と別に持ち、描画中に状態を書き換える処理と借用がぶつからないようにする
    static MAIN_P: RefCell<Option<Painter>> = const { RefCell::new(None) };
    static POP_P: RefCell<Option<Painter>> = const { RefCell::new(None) };
    static PICK_P: RefCell<Option<Painter>> = const { RefCell::new(None) };
}

// ---- 配置（DIP）----

const W: f32 = 760.0;
const H: f32 = 536.0;
const PAD: f32 = 20.0;
const HEAD: f32 = 44.0;
const ROW: f32 = 44.0;
const SOURCE: R = R::new(PAD, 56.0, W - PAD * 2.0, 40.0);
const VIDEO: R = R::new(PAD, 112.0, 412.0, 364.0);
const AUDIO: R = R::new(448.0, 112.0, 292.0, 184.0);
const PANEL: R = R::new(448.0, 312.0, 292.0, 164.0);
const FOOTER: R = R::new(PAD, 492.0, W - PAD * 2.0, 32.0);
const SET_DIR: R = R::new(PAD, 62.0, W - PAD * 2.0, 116.0);
const SET_KEY: R = R::new(PAD, 194.0, W - PAD * 2.0, 96.0);
const SET_ACT: R = R::new(PAD, 306.0, W - PAD * 2.0, 44.0 + ROW * 3.0 + 4.0);
const LIST: R = R::new(PAD, 96.0, W - PAD * 2.0, 392.0);
const LROW: f32 = 56.0;
const LVIS: usize = 7;
/// ビットレート（Mbps）。0 は画質から自動で決める
const BITRATES: [u32; 9] = [0, 6, 10, 16, 24, 35, 50, 80, 100];
const LABEL_V: f32 = 112.0;
const LABEL_A: f32 = 72.0;

fn row(card: R, i: usize) -> R {
    R::new(card.x, card.y + HEAD + i as f32 * ROW, card.w, ROW)
}

fn field(card: R, i: usize, label: f32) -> R {
    let r = row(card, i);
    R::new(r.x + 16.0 + label, r.y + 6.0, r.w - 32.0 - label, 32.0)
}

fn ctl_rect(c: Ctl) -> R {
    match c {
        Ctl::Source => SOURCE,
        Ctl::Quality => field(VIDEO, 0, LABEL_V),
        Ctl::Fps => field(VIDEO, 1, LABEL_V),
        Ctl::Res => field(VIDEO, 2, LABEL_V),
        Ctl::Codec => field(VIDEO, 3, LABEL_V),
        Ctl::Gpu => field(VIDEO, 4, LABEL_V),
        Ctl::Bitrate => field(VIDEO, 5, LABEL_V),
        Ctl::Cursor => field(VIDEO, 6, LABEL_V),
        Ctl::Sys => field(AUDIO, 0, LABEL_A),
        Ctl::Apps => field(AUDIO, 1, LABEL_A),
        Ctl::Mic => field(AUDIO, 2, LABEL_A),
        Ctl::Rec => R::new(PANEL.x + 16.0, PANEL.bottom() - 56.0, PANEL.w - 32.0, 40.0),
        Ctl::Folder => R::new(FOOTER.right() - 132.0, FOOTER.y, 132.0, 32.0),
        Ctl::Tab => R::new((W - 216.0) / 2.0, 14.0, 216.0, 32.0),
        Ctl::ListRefresh => R::new(W - PAD - 96.0, 54.0, 96.0, 32.0),
        Ctl::SetDir => R::new(SET_DIR.right() - 16.0 - 96.0 - 8.0 - 96.0, SET_DIR.y + 52.0, 96.0, 32.0),
        Ctl::SetDirOpen => R::new(SET_DIR.right() - 16.0 - 96.0, SET_DIR.y + 52.0, 96.0, 32.0),
        Ctl::SetKey => R::new(SET_KEY.right() - 16.0 - 96.0 - 8.0 - 112.0, SET_KEY.y + 44.0, 112.0, 32.0),
        Ctl::SetKeyReset => R::new(SET_KEY.right() - 16.0 - 96.0, SET_KEY.y + 44.0, 96.0, 32.0),
        Ctl::TogTray => row(SET_ACT, 0).inset(8.0, 2.0),
        Ctl::TogNotify => row(SET_ACT, 1).inset(8.0, 2.0),
        Ctl::TogUpd => row(SET_ACT, 2).inset(8.0, 2.0),
        Ctl::UpdBtn => R::new(SET_ACT.right() - 16.0 - 120.0, SET_ACT.y + 6.0, 120.0, 32.0),
        // 一覧の行の中身は list_row_rect で決める
        Ctl::ListOpen | Ctl::ListShow | Ctl::ListDel => R::default(),
    }
}

/// 押せる範囲（高さ 40 以上。カーソルの行は見出しごと押せる）
fn hit_rect(c: Ctl) -> R {
    match c {
        Ctl::Cursor => row(VIDEO, 6).inset(8.0, 2.0),
        Ctl::Source | Ctl::Rec | Ctl::TogTray | Ctl::TogNotify | Ctl::TogUpd => ctl_rect(c),
        _ => ctl_rect(c).inset(0.0, -4.0),
    }
}

fn segs(r: R, n: usize) -> impl Iterator<Item = R> {
    let w = (r.w - tok::SEG_PAD * 2.0) / n.max(1) as f32;
    (0..n).map(move |i| R::new(r.x + tok::SEG_PAD + i as f32 * w, r.y + tok::SEG_PAD, w, r.h - tok::SEG_PAD * 2.0))
}

fn is_seg(c: Ctl) -> bool {
    matches!(c, Ctl::Quality | Ctl::Fps | Ctl::Res | Ctl::Codec | Ctl::Sys | Ctl::Tab)
}

fn seg_labels(u: &Ui, c: Ctl) -> Vec<String> {
    match c {
        Ctl::Quality => QUALITIES.map(String::from).to_vec(),
        Ctl::Fps => FPS.map(|f| f.to_string()).to_vec(),
        Ctl::Res => RES.map(|r| r.0.to_string()).to_vec(),
        Ctl::Codec => u.codecs.iter().map(|c| c.label().to_string()).collect(),
        Ctl::Sys => SYS_MODES.map(String::from).to_vec(),
        Ctl::Tab => ["録画", "一覧", "設定"].map(String::from).to_vec(),
        _ => Vec::new(),
    }
}

fn seg_value(u: &Ui, c: Ctl) -> usize {
    match c {
        Ctl::Quality => u.quality,
        Ctl::Fps => u.fps,
        Ctl::Res => u.res,
        Ctl::Codec => u.codec,
        Ctl::Sys => u.sys,
        Ctl::Tab => TABS.iter().position(|&t| t == u.tab).unwrap_or(0),
        _ => 0,
    }
}

fn enabled(u: &Ui, c: Ctl) -> bool {
    let busy = matches!(u.status, Status::Starting | Status::Saving);
    match c {
        Ctl::Tab => true,
        Ctl::ListRefresh | Ctl::ListOpen | Ctl::ListShow | Ctl::ListDel => u.tab == Tab::List,
        Ctl::SetDir => u.tab == Tab::Set && !busy && u.status != Status::Recording,
        Ctl::SetDirOpen | Ctl::SetKey | Ctl::SetKeyReset | Ctl::TogTray | Ctl::TogNotify | Ctl::TogUpd | Ctl::UpdBtn => u.tab == Tab::Set && !busy,
        _ if u.tab != Tab::Rec => false,
        _ => match u.status {
            Status::Starting | Status::Saving => c == Ctl::Folder,
            Status::Recording => matches!(c, Ctl::Rec | Ctl::Folder),
            _ => true,
        },
    }
}

/// 一覧の行 i（表示中の何番目か）の範囲
fn list_row_rect(i: usize) -> R {
    R::new(LIST.x, LIST.y + i as f32 * LROW, LIST.w, LROW)
}

/// 行の右にある 3 つのボタン（再生, 場所, 削除）
fn list_btn_rect(i: usize, k: usize) -> R {
    let r = list_row_rect(i);
    R::new(r.right() - 16.0 - 40.0 * (3 - k) as f32 - 4.0 * (2 - k) as f32, r.y + 8.0, 40.0, 40.0)
}

fn list_hit(u: &Ui, x: f32, y: f32) -> Option<Hit> {
    for i in 0..LVIS {
        let idx = u.list_off + i;
        if idx >= u.list.len() {
            break;
        }
        if list_row_rect(i).contains(x, y) {
            let ctl = if list_btn_rect(i, 2).contains(x, y) {
                Ctl::ListDel
            } else if list_btn_rect(i, 1).contains(x, y) {
                Ctl::ListShow
            } else {
                Ctl::ListOpen
            };
            return Some(Hit { ctl, idx });
        }
    }
    None
}

fn hit_main(u: &Ui, x: f32, y: f32) -> Option<Hit> {
    if u.tab == Tab::List {
        if let Some(h) = list_hit(u, x, y) {
            return Some(h);
        }
    }
    let c = *ORDER.iter().find(|&&c| enabled(u, c) && hit_rect(c).contains(x, y))?;
    let idx = if is_seg(c) {
        let n = seg_labels(u, c).len().max(1);
        let r = ctl_rect(c);
        (((x - r.x) / r.w * n as f32).max(0.0) as usize).min(n - 1)
    } else {
        0
    };
    Some(Hit { ctl: c, idx })
}

fn display_name(name: &str) -> &str {
    name.strip_prefix("ウィンドウ: ").unwrap_or(name)
}

fn source_glyph(s: Source) -> u16 {
    match s {
        Source::Monitor(_) => glyph::MONITOR,
        Source::Window(_) => glyph::WINDOW,
    }
}

// ---- 描画 ----

#[derive(Clone, Copy)]
struct St {
    hover: bool,
    press: bool,
    focus: bool,
    on: bool,
}

fn st(u: &Ui, c: Ctl, idx: usize) -> St {
    let h = Some(Hit { ctl: c, idx });
    St { hover: u.hover == h, press: u.press == h && u.hover == h, focus: u.kb && u.focus == Some(c), on: enabled(u, c) }
}

fn ring(p: &Painter, r: R, radius: f32) {
    p.stroke(r.inset(-3.0, -3.0), radius + 3.0, tok::FOCUS, 2.0);
}

fn card(p: &Painter, r: R, title: Option<&str>) {
    p.fill(r, tok::R_CARD, tok::CARD);
    p.stroke(r, tok::R_CARD, tok::HAIR, 1.0);
    if let Some(t) = title {
        p.text(t, R::new(r.x + 16.0, r.y + 8.0, r.w - 32.0, 32.0), Font::strong(13.0), tok::TEXT);
    }
}

fn label(p: &Painter, card: R, i: usize, w: f32, s: &str, on: bool) {
    let r = row(card, i);
    p.text(s, R::new(r.x + 16.0, r.y, w, r.h), Font::text(13.0), if on { tok::TEXT_2 } else { tok::TEXT_3 });
}

fn key_name(vk: u32) -> String {
    match vk {
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(String::from).unwrap_or_default(),
        0x20 => "Space".into(),
        0x0D => "Enter".into(),
        0x09 => "Tab".into(),
        0x1B => "Esc".into(),
        0x13 => "Pause".into(),
        0x2C => "PrtSc".into(),
        0x2D => "Ins".into(),
        0x2E => "Del".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "PgUp".into(),
        0x22 => "PgDn".into(),
        0x25 => "←".into(),
        0x26 => "↑".into(),
        0x27 => "→".into(),
        0x28 => "↓".into(),
        _ => unsafe {
            let sc = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
            let mut buf = [0u16; 64];
            let n = GetKeyNameTextW((sc << 16) as i32, &mut buf);
            if n > 0 { String::from_utf16_lossy(&buf[..n as usize]) } else { format!("0x{vk:02X}") }
        },
    }
}

/// ショートカットを、キーの札ひとつずつの文字にする（例: Ctrl, Shift, R）
fn key_parts(hk: (u32, u32)) -> Vec<String> {
    let mut v = Vec::new();
    for (bit, name) in [(MOD_CONTROL.0, "Ctrl"), (MOD_ALT.0, "Alt"), (MOD_SHIFT.0, "Shift"), (MOD_WIN.0, "Win")] {
        if hk.0 & bit != 0 {
            v.push(name.to_string());
        }
    }
    v.push(key_name(hk.1));
    v
}

fn key_text(hk: (u32, u32)) -> String {
    key_parts(hk).join("+")
}

fn cap_w(p: &Painter, s: &str) -> f32 {
    (p.measure(s, Font::strong(11.0)) + 16.0).max(30.0).ceil()
}

fn caps_width(p: &Painter, parts: &[String]) -> f32 {
    parts.iter().map(|s| cap_w(p, s)).sum::<f32>() + 4.0 * parts.len().saturating_sub(1) as f32
}

/// キーの札を x から並べ、終わりの x を返す
fn keycaps(p: &Painter, x: f32, cy: f32, parts: &[String], fill: C, edge: Option<C>, fg: C) -> f32 {
    let mut x = x;
    for s in parts {
        let w = cap_w(p, s);
        let r = R::new(x, cy - 10.0, w, 20.0);
        p.fill(r, 5.0, fill);
        if let Some(e) = edge {
            p.stroke(r, 5.0, e, 1.0);
        }
        p.text(s, r, Font::strong(11.0).at(Align::Center), fg);
        x += w + 4.0;
    }
    x - 4.0
}

/// アイコン。取れなかったアプリは頭文字の札にする
fn app_icon(p: &Painter, icon: Option<&IWICBitmap>, exe: &str, name: &str, r: R) {
    match icon {
        Some(b) => p.image(&exe.to_lowercase(), b, r),
        None => {
            let rad = (r.w / 4.5).round();
            p.fill(r, rad, tok::SEG_ON);
            p.stroke(r, rad, tok::ICON_EDGE, 1.0);
            let first: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
            p.text(&first, r, Font::strong((r.h * 0.5).round()).at(Align::Center), tok::TEXT_2);
        }
    }
}

fn draw_seg(u: &Ui, p: &Painter, c: Ctl) {
    let r = ctl_rect(c);
    let labels = seg_labels(u, c);
    let v = seg_value(u, c);
    let on = enabled(u, c);
    p.fill(r, tok::R_CTRL, tok::CTRL);
    for (i, s) in segs(r, labels.len()).enumerate() {
        let si = st(u, c, i);
        if i == v {
            p.fill(s, tok::R_SEG, if on { tok::SEG_ON } else { tok::CTRL_HOVER });
        } else if si.press {
            // ホバーは文字だけ明るくし、選択中の地と見分けやすくする
            p.fill(s, tok::R_SEG, tok::CTRL_HOVER);
        }
        let (f, col) = if !on {
            (Font::text(12.5), tok::TEXT_3)
        } else if i == v {
            (Font::strong(12.5), tok::TEXT)
        } else {
            (Font::text(12.5), if si.hover { tok::TEXT } else { tok::TEXT_2 })
        };
        p.text(&labels[i], s, f.at(Align::Center), col);
    }
    if st(u, c, 0).focus {
        ring(p, r, tok::R_CTRL);
    }
}

/// 一覧を開く部品の地と矢印。中身を描く範囲を返す
fn select_base(u: &Ui, p: &Painter, c: Ctl) -> (R, St) {
    let r = ctl_rect(c);
    let s = st(u, c, 0);
    let fill = if !s.on {
        tok::CTRL
    } else if u.open == Some(c) || s.press {
        tok::CTRL_PRESS
    } else if s.hover {
        tok::CTRL_HOVER
    } else {
        tok::CTRL
    };
    p.fill(r, tok::R_CTRL, fill);
    p.glyph(glyph::CHEVRON, R::new(r.right() - 32.0, r.y, 32.0, r.h), 10.0, if s.on { tok::TEXT_2 } else { tok::TEXT_3 });
    if s.focus {
        ring(p, r, tok::R_CTRL);
    }
    (R::new(r.x + 12.0, r.y, r.w - 44.0, r.h), s)
}

fn draw_source(u: &Ui, p: &Painter) {
    let (r, s) = select_base(u, p, Ctl::Source);
    let fg = if s.on { tok::TEXT } else { tok::TEXT_3 };
    let Some(info) = u.sources.get(u.src) else {
        p.text("録画できる画面が見つかりません", r, Font::text(13.0), tok::TEXT_2);
        return;
    };
    p.glyph(source_glyph(info.source), R::new(r.x, r.y, 20.0, r.h), 16.0, if s.on { tok::TEXT_2 } else { tok::TEXT_3 });
    let lf = Font::text(12.0);
    let lw = p.measure("録画対象", lf);
    p.text("録画対象", R::new(r.x + 30.0, r.y, lw + 2.0, r.h), lf, tok::TEXT_3);
    let x = r.x + 30.0 + lw + 14.0;
    p.text(display_name(&info.name), R::new(x, r.y, r.right() - x, r.h), Font::strong(13.0), fg);
}

fn bitrate_text(i: usize) -> String {
    match BITRATES[i.min(BITRATES.len() - 1)] {
        0 => "自動（画質に合わせる）".to_string(),
        m => format!("{m} Mbps"),
    }
}

fn draw_text_select(u: &Ui, p: &Painter, c: Ctl, text: &str) {
    let (r, s) = select_base(u, p, c);
    p.text(text, r, Font::text(13.0), if s.on { tok::TEXT } else { tok::TEXT_3 });
}

fn app_name(u: &Ui, exe: &str) -> String {
    u.names.iter().find(|(k, _)| k.eq_ignore_ascii_case(exe)).map_or_else(|| apps::short_name(exe), |(_, n)| n.clone())
}

fn draw_apps(u: &Ui, p: &Painter) {
    let (r, s) = select_base(u, p, Ctl::Apps);
    if u.apps.is_empty() {
        p.text("アプリを選ぶ", r, Font::text(13.0), if s.on { tok::TEXT_2 } else { tok::TEXT_3 });
        return;
    }
    let mut x = r.x;
    for exe in u.apps.iter().take(3) {
        let icon = u.icons.iter().find(|(k, _)| k.eq_ignore_ascii_case(exe)).map(|(_, b)| b);
        let name = app_name(u, exe);
        app_icon(p, icon, exe, &name, R::new(x, r.y + 8.0, 16.0, 16.0));
        x += 20.0;
    }
    let names: Vec<String> = u.apps.iter().map(|a| app_name(u, a)).collect();
    let x = x + 4.0;
    let f = Font::text(13.0);
    let mut text = names.join(", ");
    // 入りきらなければ、先頭のアプリと残りの数だけにする
    if names.len() > 1 && p.measure(&text, f) > r.right() - x {
        text = format!("{} ほか {} 件", names[0], names.len() - 1);
    }
    p.text(&text, R::new(x, r.y, r.right() - x, r.h), f, if s.on { tok::TEXT } else { tok::TEXT_3 });
}

fn draw_toggle(u: &Ui, p: &Painter) {
    let r = ctl_rect(Ctl::Cursor);
    let s = st(u, Ctl::Cursor, 0);
    let t = R::new(r.x, r.y + 6.0, 36.0, 20.0);
    let track = match (u.cursor, s.on) {
        (true, true) => tok::TEXT,
        (true, false) => tok::TEXT_3,
        (false, _) if s.hover => tok::SEG_ON,
        _ => tok::CTRL_HOVER,
    };
    p.fill(t, 10.0, track);
    let kx = if u.cursor { t.right() - 10.0 } else { t.x + 10.0 };
    let knob = if u.cursor { tok::BG } else if s.on { tok::TEXT_2 } else { tok::TEXT_3 };
    p.dot(kx, t.y + 10.0, 7.0, knob);
    let text = if u.cursor { "録画する" } else { "録画しない" };
    p.text(text, R::new(t.right() + 12.0, r.y, 120.0, r.h), Font::text(13.0), if s.on { tok::TEXT } else { tok::TEXT_3 });
    if s.focus {
        ring(p, t, 10.0);
    }
}

fn draw_rec(u: &Ui, p: &Painter) {
    let r = ctl_rect(Ctl::Rec);
    let s = st(u, Ctl::Rec, 0);
    let busy = match u.status {
        Status::Starting => Some("開始しています…"),
        Status::Saving => Some("保存しています…"),
        _ => None,
    };
    if let Some(t) = busy {
        p.fill(r, tok::R_CTRL, tok::CTRL);
        p.text(t, r, Font::text(13.0).at(Align::Center), tok::TEXT_2);
        return;
    }
    let recording = u.status == Status::Recording;
    if s.press {
        p.scale(r, 0.96);
    }
    let (fill, fg, cap) = if recording {
        (if s.press { tok::CTRL_PRESS } else if s.hover { tok::SEG_ON_HOVER } else { tok::SEG_ON }, tok::TEXT, tok::CTRL_HOVER)
    } else {
        (if s.press { tok::REC_PRESS } else if s.hover { tok::REC_HOVER } else { tok::REC }, tok::ON_ACCENT, tok::ON_REC)
    };
    p.fill(r, tok::R_CTRL, fill);
    let text = if recording { "停止" } else { "録画開始" };
    let f = Font::strong(13.5);
    let tw = p.measure(text, f);
    let (cx, cy) = r.center();
    let x0 = (cx - (10.0 + 8.0 + tw) / 2.0).round();
    if recording {
        p.fill(R::new(x0, cy - 5.0, 10.0, 10.0), 2.0, tok::REC);
    } else {
        p.dot(x0 + 5.0, cy, 5.0, tok::ON_ACCENT);
    }
    p.text(text, R::new(x0 + 18.0, r.y, tw + 4.0, r.h), f, fg);
    let parts = key_parts(u.hotkey);
    let cw = caps_width(p, &parts);
    if u.hotkey_ok && cw <= 64.0 {
        keycaps(p, r.right() - 12.0 - cw, cy, &parts, cap, None, fg);
    }
    if s.press {
        p.unscale();
    }
    if s.focus {
        ring(p, r, tok::R_CTRL);
    }
}

fn draw_panel(u: &Ui, p: &Painter) {
    card(p, PANEL, None);
    let line = R::new(PANEL.x + 16.0, PANEL.y + 14.0, PANEL.w - 32.0, 18.0);
    let small = Font::text(12.0);
    let (cx, cy) = (line.x + 4.0, line.y + line.h / 2.0);
    let tx = R::new(line.x + 16.0, line.y, line.w - 100.0, line.h);
    match &u.status {
        Status::Recording => {
            p.dot(cx, cy, 4.0, if u.blink { tok::REC } else { tok::REC_DIM });
            p.text("録画中", tx, small, tok::TEXT);
            let mb = format!("{:.1} MB", u.size_mb);
            p.text(&mb, R::new(line.right() - 100.0, line.y, 100.0, line.h), small.at(Align::Right), tok::TEXT_2);
        }
        Status::Starting | Status::Saving | Status::Ready => {
            p.dot(cx, cy, 4.0, tok::TEXT_3);
            let t = match u.status {
                Status::Starting => "開始しています",
                Status::Saving => "保存しています",
                _ => "待機中",
            };
            p.text(t, tx, small, tok::TEXT_2);
        }
        Status::Saved(_) => {
            p.glyph(glyph::CHECK, R::new(line.x - 2.0, line.y, 12.0, line.h), 11.0, tok::TEXT_2);
            p.text("保存しました", tx, small, tok::TEXT_2);
        }
        Status::Failed(title, _) => {
            p.glyph(glyph::ERROR, R::new(line.x - 2.0, line.y, 12.0, line.h), 11.0, tok::REC);
            p.text(title, R::new(tx.x, tx.y, line.w - 16.0, tx.h), small, tok::TEXT);
        }
    }
    let s = u.elapsed;
    let time = format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
    let active = u.status == Status::Recording;
    p.text(&time, R::new(PANEL.x + 16.0, PANEL.y + 32.0, PANEL.w - 32.0, 30.0), Font::strong(24.0), if active { tok::TEXT } else { tok::TEXT_3 });
    draw_rec(u, p);
}

fn draw_folder(u: &Ui, p: &Painter) {
    let r = ctl_rect(Ctl::Folder);
    let s = st(u, Ctl::Folder, 0);
    if s.press {
        p.scale(r, 0.96);
    }
    if s.hover {
        p.fill(r, tok::R_CTRL, if s.press { tok::CTRL_PRESS } else { tok::CTRL_HOVER });
    }
    let fg = if s.hover { tok::TEXT } else { tok::TEXT_2 };
    p.glyph(glyph::FOLDER, R::new(r.x + 12.0, r.y, 18.0, r.h), 14.0, fg);
    p.text("保存先を開く", R::new(r.x + 38.0, r.y, r.w - 46.0, r.h), Font::text(12.5), fg);
    if s.press {
        p.unscale();
    }
    if s.focus {
        ring(p, r, tok::R_CTRL);
    }
}

fn draw_main(u: &Ui, p: &Painter) {
    // 見出し
    p.text("LightCaptureX", R::new(PAD, 18.0, 150.0, 28.0), Font::strong(15.0), tok::TEXT);
    draw_seg(u, p, Ctl::Tab);
    let hr = R::new(W - PAD - 330.0, 18.0, 330.0, 28.0);
    let small = Font::text(12.0);
    if u.hotkey_ok {
        let t = "で録画を開始・停止";
        let tw = p.measure(t, small);
        p.text(t, R::new(hr.right() - tw - 2.0, hr.y, tw + 2.0, hr.h), small, tok::TEXT_3);
        let parts = key_parts(u.hotkey);
        let cw = caps_width(p, &parts);
        keycaps(p, hr.right() - tw - 10.0 - cw, hr.y + 14.0, &parts, tok::CTRL, Some(tok::EDGE), tok::TEXT_2);
    } else {
        let t = format!("{} はほかのアプリが使用中", key_text(u.hotkey));
        let tw = p.measure(&t, small);
        p.text(&t, R::new(hr.right() - tw - 2.0, hr.y, tw + 2.0, hr.h), small, tok::TEXT_2);
        p.glyph(glyph::WARNING, R::new(hr.right() - tw - 24.0, hr.y, 16.0, hr.h), 12.0, tok::TEXT_2);
    }
    match u.tab {
        Tab::Rec => draw_rec_tab(u, p),
        Tab::List => draw_list(u, p),
        Tab::Set => draw_set(u, p),
    }
}

fn draw_rec_tab(u: &Ui, p: &Painter) {
    draw_source(u, p);

    let live = u.status == Status::Recording;
    if live {
        draw_live(u, p);
        draw_meters(u, p);
        draw_panel(u, p);
        draw_footer(u, p);
        return;
    }
    // 映像
    card(p, VIDEO, Some("映像"));
    let on = enabled(u, Ctl::Quality);
    for (i, s) in ["画質", "フレームレート", "解像度", "コーデック", "GPU", "ビットレート", "カーソル"].iter().enumerate() {
        label(p, VIDEO, i, LABEL_V, s, on);
    }
    for c in [Ctl::Quality, Ctl::Fps, Ctl::Res, Ctl::Codec] {
        draw_seg(u, p, c);
    }
    let gpu = if u.gpu == 0 { "自動".to_string() } else { u.gpus.get(u.gpu - 1).cloned().unwrap_or_default() };
    draw_text_select(u, p, Ctl::Gpu, &gpu);
    draw_text_select(u, p, Ctl::Bitrate, &bitrate_text(u.bitrate));
    draw_toggle(u, p);

    // 音声
    card(p, AUDIO, Some("音声"));
    for (i, s) in ["PC の音", "アプリ", "マイク"].iter().enumerate() {
        label(p, AUDIO, i, LABEL_A, s, on);
    }
    draw_seg(u, p, Ctl::Sys);
    draw_apps(u, p);
    let mic = if u.mic == 0 { "なし".to_string() } else { u.mics.get(u.mic - 1).map(|m| m.1.clone()).unwrap_or_default() };
    draw_text_select(u, p, Ctl::Mic, &mic);

    draw_panel(u, p);
    draw_footer(u, p);
}

fn draw_footer(u: &Ui, p: &Painter) {
    let small = Font::text(12.0);
    // 下の行: 録画中はエンコーダ、終わったらファイル名かエラーの詳細
    let (text, col) = match &u.status {
        Status::Recording => (u.info.clone(), tok::TEXT_3),
        Status::Saved(name) => (name.clone(), tok::TEXT_2),
        Status::Failed(_, detail) => (detail.clone(), tok::TEXT_2),
        _ => (format!("保存先  {}", u.out_dir), tok::TEXT_3),
    };
    p.text(&text, R::new(FOOTER.x, FOOTER.y, FOOTER.w - 160.0, FOOTER.h), small, col);
    draw_folder(u, p);
}

// ---- リアルタイム表示 ----

/// 長いパスは先頭を省いて幅に収める
fn fit(p: &Painter, s: &str, f: Font, w: f32) -> String {
    if p.measure(s, f) <= w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    for skip in 1..chars.len() {
        let t: String = std::iter::once('…').chain(chars[skip..].iter().copied()).collect();
        if p.measure(&t, f) <= w {
            return t;
        }
    }
    String::new()
}

fn tile(p: &Painter, r: R, label: &str, value: &str, unit: &str, hot: bool) {
    p.fill(r, tok::R_CTRL, tok::CTRL);
    p.text(label, R::new(r.x + 12.0, r.y + 8.0, r.w - 24.0, 16.0), Font::text(11.5), tok::TEXT_3);
    let vf = Font::strong(20.0);
    let col = if hot { tok::REC } else { tok::TEXT };
    p.text(value, R::new(r.x + 12.0, r.y + 28.0, r.w - 24.0, 30.0), vf, col);
    if !unit.is_empty() {
        let vw = p.measure(value, vf);
        p.text(unit, R::new(r.x + 14.0 + vw, r.y + 33.0, r.w - 24.0 - vw, 22.0), Font::text(12.0), tok::TEXT_3);
    }
}

fn pct(v: Option<u32>) -> (String, &'static str) {
    match v {
        Some(v) => (v.to_string(), "%"),
        None => ("—".into(), ""),
    }
}

fn draw_live(u: &Ui, p: &Painter) {
    card(p, VIDEO, Some("リアルタイム"));
    let target = FPS[u.fps.min(FPS.len() - 1)] as f32;
    let tw = (VIDEO.w - 32.0 - 16.0) / 3.0;
    let tr = |col: usize, rw: usize| R::new(VIDEO.x + 16.0 + col as f32 * (tw + 8.0), VIDEO.y + 44.0 + rw as f32 * 72.0, tw, 64.0);
    tile(p, tr(0, 0), "ファイルサイズ", &format!("{:.1}", u.size_mb), "MB", false);
    tile(p, tr(1, 0), "実 fps", &format!("{:.0}", u.fps_now), &format!("/ {target:.0}"), u.fps_now > 0.0 && u.fps_now < target * 0.9);
    tile(p, tr(2, 0), "ドロップ", &u.dropped.to_string(), "枚", u.dropped > 0);
    let (c, cu) = pct(u.cpu);
    let (g, gu) = pct(u.gpu_load);
    tile(p, tr(0, 1), "CPU", &c, cu, u.cpu.is_some_and(|v| v >= 90));
    tile(p, tr(1, 1), "GPU", &g, gu, u.gpu_load.is_some_and(|v| v >= 95));
    tile(p, tr(2, 1), "ビットレート", &format!("{:.1}", u.mbps), "Mbps", false);

    // fps の推移
    let top = VIDEO.y + 44.0 + 144.0 + 4.0;
    p.text("fps の推移（直近 60 秒）", R::new(VIDEO.x + 16.0, top, 240.0, 18.0), Font::text(11.5), tok::TEXT_3);
    let g = R::new(VIDEO.x + 16.0, top + 22.0, VIDEO.w - 32.0, VIDEO.bottom() - 16.0 - (top + 22.0));
    p.fill(g, tok::R_CTRL, tok::CTRL);
    let top_v = (target * 1.1).max(u.fps_hist.iter().copied().fold(0.0, f32::max));
    let pad = 8.0;
    let (gx, gy, gw, gh) = (g.x + pad, g.y + pad, g.w - pad * 2.0, g.h - pad * 2.0);
    let yv = |v: f32| gy + gh - (v / top_v).clamp(0.0, 1.0) * gh;
    p.line(gx, yv(target), gx + gw, yv(target), tok::HAIR, 1.0);
    let n = u.fps_hist.len();
    if n >= 2 {
        let step = gw / 119.0;
        let x0 = gx + gw - step * (n - 1) as f32;
        for i in 1..n {
            let (a, b) = (u.fps_hist[i - 1], u.fps_hist[i]);
            let col = if b < target * 0.9 { tok::REC } else { tok::TEXT_2 };
            p.line(x0 + step * (i - 1) as f32, yv(a), x0 + step * i as f32, yv(b), col, 1.5);
        }
        p.dot(gx + gw, yv(u.fps_hist[n - 1]), 3.0, tok::TEXT);
    } else {
        p.text("計測しています…", g, Font::text(12.0).at(Align::Center), tok::TEXT_3);
    }
}

/// -60 dB..0 dB を 0..1 にする
fn db_level(peak: f32) -> f32 {
    if peak <= 0.000_001 {
        0.0
    } else {
        ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
    }
}

fn draw_meters(u: &Ui, p: &Painter) {
    card(p, AUDIO, Some("音量"));
    let sys_on = match u.sys {
        0 => false,
        SYS_APPS => !u.apps.is_empty(),
        _ => true,
    };
    let rows = [("PC の音", sys_on, 0usize), ("マイク", u.mic != 0, 1usize)];
    for (i, (name, on, k)) in rows.into_iter().enumerate() {
        let y = AUDIO.y + 52.0 + i as f32 * 60.0;
        let col = if on { tok::TEXT } else { tok::TEXT_3 };
        p.text(name, R::new(AUDIO.x + 16.0, y, 140.0, 18.0), Font::text(13.0), col);
        let tr = R::new(AUDIO.x + 16.0, y + 26.0, AUDIO.w - 32.0, 8.0);
        p.fill(tr, 4.0, tok::CTRL);
        if !on {
            p.text("オフ", R::new(AUDIO.right() - 16.0 - 60.0, y, 60.0, 18.0), Font::text(12.0).at(Align::Right), tok::TEXT_3);
            continue;
        }
        let lv = u.meter[k];
        if lv > 0.01 {
            let w = (tr.w * lv).max(8.0);
            let c = if lv > 0.93 { tok::REC } else if lv > 0.75 { tok::TEXT } else { tok::TEXT_2 };
            p.fill(R::new(tr.x, tr.y, w, tr.h), 4.0, c);
        }
        let h = u.hold[k];
        if h > 0.02 {
            let x = tr.x + (tr.w - 2.0) * h;
            p.fill(R::new(x, tr.y - 3.0, 2.0, tr.h + 6.0), 1.0, if h > 0.93 { tok::REC } else { tok::TEXT });
        }
    }
}

// ---- 設定タブ ----

fn draw_btn(u: &Ui, p: &Painter, c: Ctl, icon: Option<u16>, text: &str) {
    let r = ctl_rect(c);
    let s = st(u, c, 0);
    if s.press {
        p.scale(r, 0.96);
    }
    let fill = if !s.on {
        tok::CTRL
    } else if s.press {
        tok::CTRL_PRESS
    } else if s.hover {
        tok::CTRL_HOVER
    } else {
        tok::CTRL
    };
    p.fill(r, tok::R_CTRL, fill);
    let fg = if !s.on { tok::TEXT_3 } else if s.hover { tok::TEXT } else { tok::TEXT_2 };
    let f = Font::text(12.5);
    let tw = p.measure(text, f);
    let iw = if icon.is_some() { 22.0 } else { 0.0 };
    let x0 = (r.x + (r.w - iw - tw) / 2.0).round();
    if let Some(g) = icon {
        p.glyph(g, R::new(x0, r.y, 16.0, r.h), 13.0, fg);
    }
    p.text(text, R::new(x0 + iw, r.y, tw + 4.0, r.h), f, fg);
    if s.press {
        p.unscale();
    }
    if s.focus {
        ring(p, r, tok::R_CTRL);
    }
}

fn draw_switch(u: &Ui, p: &Painter, c: Ctl, on_state: bool, title: &str, sub: &str) {
    let r = ctl_rect(c);
    let s = st(u, c, 0);
    if s.hover && s.on {
        p.fill(r, tok::R_CTRL, tok::CTRL);
    }
    let fg = if s.on { tok::TEXT } else { tok::TEXT_3 };
    p.text(title, R::new(r.x + 8.0, r.y + 3.0, r.w - 80.0, 18.0), Font::text(13.0), fg);
    p.text(sub, R::new(r.x + 8.0, r.y + 21.0, r.w - 80.0, 16.0), Font::text(11.5), tok::TEXT_3);
    let t = R::new(r.right() - 8.0 - 36.0, r.y + 10.0, 36.0, 20.0);
    let track = match (on_state, s.on) {
        (true, true) => tok::TEXT,
        (true, false) => tok::TEXT_3,
        (false, _) if s.hover => tok::SEG_ON,
        _ => tok::CTRL_HOVER,
    };
    p.fill(t, 10.0, track);
    let kx = if on_state { t.right() - 10.0 } else { t.x + 10.0 };
    let knob = if on_state { tok::BG } else if s.on { tok::TEXT_2 } else { tok::TEXT_3 };
    p.dot(kx, t.y + 10.0, 7.0, knob);
    if s.focus {
        ring(p, t, 10.0);
    }
}

fn draw_set(u: &Ui, p: &Painter) {
    // 保存先
    card(p, SET_DIR, Some("保存先"));
    let fr = R::new(SET_DIR.x + 16.0, SET_DIR.y + 52.0, ctl_rect(Ctl::SetDir).x - 8.0 - (SET_DIR.x + 16.0), 32.0);
    p.fill(fr, tok::R_CTRL, tok::CTRL);
    p.glyph(glyph::FOLDER, R::new(fr.x + 12.0, fr.y, 18.0, fr.h), 14.0, tok::TEXT_2);
    let f = Font::text(12.5);
    let path = fit(p, &u.out_dir, f, fr.w - 52.0);
    p.text(&path, R::new(fr.x + 38.0, fr.y, fr.w - 46.0, fr.h), f, tok::TEXT);
    draw_btn(u, p, Ctl::SetDir, Some(glyph::EDIT), "変更…");
    draw_btn(u, p, Ctl::SetDirOpen, Some(glyph::FOLDER), "開く");
    let free = u.free.map_or_else(|| "空き容量を調べられません".to_string(), |b| format!("空き容量  {}", recs::fmt_size(b)));
    p.text(&free, R::new(SET_DIR.x + 16.0, SET_DIR.y + 88.0, SET_DIR.w - 32.0, 20.0), Font::text(12.0), tok::TEXT_3);

    // ショートカット
    card(p, SET_KEY, Some("ショートカット"));
    let cy = SET_KEY.y + 44.0 + 16.0;
    p.text("録画の開始・停止", R::new(SET_KEY.x + 16.0, cy - 16.0, 150.0, 32.0), Font::text(13.0), tok::TEXT);
    let kx = SET_KEY.x + 16.0 + 160.0;
    if u.capturing {
        p.dot(kx + 4.0, cy, 4.0, tok::REC);
        p.text("キーを押してください（Esc で取り消し）", R::new(kx + 16.0, cy - 16.0, 300.0, 32.0), Font::text(13.0), tok::TEXT);
    } else {
        keycaps(p, kx, cy, &key_parts(u.hotkey), tok::CTRL, Some(tok::EDGE), tok::TEXT);
    }
    draw_btn(u, p, Ctl::SetKey, None, if u.capturing { "取り消し" } else { "変更…" });
    draw_btn(u, p, Ctl::SetKeyReset, None, "F9 に戻す");
    let (msg, col) = match (&u.note, u.hotkey_ok) {
        (Some(n), _) => (n.clone(), tok::TEXT_2),
        (None, false) => ("このキーはほかのアプリが使用中です".to_string(), tok::TEXT_2),
        _ => ("Ctrl・Alt・Shift・Win と組み合わせることもできます".to_string(), tok::TEXT_3),
    };
    p.text(&msg, R::new(SET_KEY.x + 16.0, SET_KEY.y + 76.0, SET_KEY.w - 32.0, 16.0), Font::text(11.5), col);

    // 動作
    card(p, SET_ACT, Some("動作"));
    draw_switch(u, p, Ctl::TogTray, u.tray_close, "×で閉じたときトレーに残す", "オフにすると、×を押したときアプリを終了します");
    draw_switch(u, p, Ctl::TogNotify, u.notify, "トレーの通知", "録画の開始・保存・失敗をトレーから知らせます");
    draw_switch(u, p, Ctl::TogUpd, u.auto_update, "起動時に更新を確認", "新しい版があれば通知します。更新は確認してから行います");
    let (msg, btn) = match update::state() {
        update::State::Checking => ("確認しています…".to_string(), "確認中…"),
        update::State::Latest => (format!("v{} · 最新です", update::VERSION), "更新を確認"),
        update::State::Available(v) => (format!("v{} → v{v} に更新できます", update::VERSION), "更新する"),
        update::State::Downloading => ("ダウンロードしています…".to_string(), "更新中…"),
        update::State::Failed(m) => (m, "更新を確認"),
        update::State::Idle => (format!("v{}", update::VERSION), "更新を確認"),
    };
    p.text(&msg, R::new(SET_ACT.x + 72.0, SET_ACT.y + 12.0, SET_ACT.w - 72.0 - 16.0 - 128.0 - 8.0, 20.0), Font::text(12.0), tok::TEXT_3);
    draw_btn(u, p, Ctl::UpdBtn, None, btn);
}

// ---- 録画一覧タブ ----

fn draw_list(u: &Ui, p: &Painter) {
    let total: u64 = u.list.iter().map(|e| e.size).sum();
    let head = if u.list.is_empty() { "録画".to_string() } else { format!("{} 件  ·  合計 {}", u.list.len(), recs::fmt_size(total)) };
    p.text(&head, R::new(PAD + 4.0, 54.0, 400.0, 32.0), Font::text(12.5), tok::TEXT_2);
    draw_btn(u, p, Ctl::ListRefresh, Some(glyph::REFRESH), "更新");
    card(p, LIST, None);
    if u.list.is_empty() {
        let c = R::new(LIST.x, LIST.y + LIST.h / 2.0 - 30.0, LIST.w, 28.0);
        p.text("録画はまだありません", c, Font::strong(14.0).at(Align::Center), tok::TEXT_2);
        let hint = if u.hotkey_ok { format!("{} を押すと録画を始めます", key_text(u.hotkey)) } else { "録画タブの「録画開始」から始められます".to_string() };
        p.text(&hint, R::new(c.x, c.bottom() + 2.0, c.w, 22.0), Font::text(12.5).at(Align::Center), tok::TEXT_3);
    }
    let shown = u.list.len().saturating_sub(u.list_off).min(LVIS);
    for i in 0..shown {
        let idx = u.list_off + i;
        let e = &u.list[idx];
        let rr = list_row_rect(i);
        let row_hot = matches!(u.hover, Some(Hit { ctl: Ctl::ListOpen | Ctl::ListShow | Ctl::ListDel, idx: h }) if h == idx);
        if row_hot {
            let pressed = matches!(u.press, Some(Hit { ctl: Ctl::ListOpen, idx: h }) if h == idx) && u.hover == u.press;
            p.fill(rr.inset(6.0, 4.0), tok::R_CTRL, if pressed { tok::CTRL_PRESS } else { tok::CTRL });
        } else if i + 1 < shown {
            p.line(rr.x + 16.0, rr.bottom(), rr.right() - 16.0, rr.bottom(), tok::HAIR, 1.0);
        }
        let ic = R::new(rr.x + 16.0, rr.y + 8.0, 72.0, 40.0);
        match u.thumbs.iter().find(|t| Path::new(&t.0) == e.path) {
            Some((_, b)) => {
                p.image(&format!("th:{}", e.path.display()), b, ic);
                p.stroke(ic, 6.0, tok::ICON_EDGE, 1.0);
            }
            None => {
                p.fill(ic, 6.0, if row_hot { tok::SEG_ON } else { tok::CTRL });
                p.glyph(glyph::VIDEO, ic, 14.0, tok::TEXT_2);
            }
        }
        let nw = list_btn_rect(i, 0).x - (rr.x + 104.0) - 8.0;
        let nf = Font::strong(13.0);
        p.text(&fit(p, &e.name, nf, nw), R::new(rr.x + 104.0, rr.y + 9.0, nw, 20.0), nf, tok::TEXT);
        let (y, mo, d, h, mi) = e.when;
        let mut meta = Vec::new();
        if y != 0 {
            meta.push(format!("{y}/{mo:02}/{d:02} {h:02}:{mi:02}"));
        }
        if let Some(s) = e.secs {
            meta.push(recs::fmt_dur(s));
        }
        meta.push(recs::fmt_size(e.size));
        p.text(&meta.join("  ·  "), R::new(rr.x + 104.0, rr.y + 30.0, nw, 16.0), Font::text(11.5), tok::TEXT_3);
        for (k, g) in [glyph::PLAY, glyph::FOLDER, glyph::DELETE].into_iter().enumerate() {
            let ctl = [Ctl::ListOpen, Ctl::ListShow, Ctl::ListDel][k];
            let br = list_btn_rect(i, k);
            let hov = u.hover == Some(Hit { ctl, idx }) && k > 0;
            let down = u.press == Some(Hit { ctl, idx }) && hov;
            if hov {
                p.fill(br.inset(2.0, 2.0), tok::R_CTRL, if down { tok::CTRL_PRESS } else { tok::SEG_ON });
            }
            let col = if !row_hot { tok::TEXT_3 } else if hov && k == 2 { tok::REC } else if hov { tok::TEXT } else { tok::TEXT_2 };
            p.glyph(g, br, 14.0, col);
        }
    }
    if u.list.len() > LVIS {
        let track = R::new(LIST.right() - 7.0, LIST.y + 10.0, 3.0, LIST.h - 20.0);
        let n = u.list.len() as f32;
        let th = (track.h * LVIS as f32 / n).max(24.0);
        let ty = track.y + (track.h - th) * u.list_off as f32 / (n - LVIS as f32);
        p.fill(R::new(track.x, ty, track.w, th), 1.5, tok::SEG_ON);
    }
    let (t, col) = match &u.note {
        Some(n) => (n.clone(), tok::TEXT_2),
        None => (format!("保存先  {}", u.out_dir), tok::TEXT_3),
    };
    p.text(&fit(p, &t, Font::text(12.0), FOOTER.w - 8.0), R::new(FOOTER.x + 4.0, FOOTER.y, FOOTER.w - 8.0, FOOTER.h), Font::text(12.0), col);
}

// ---- 一覧（ドロップダウン）----

const ITEM: f32 = 34.0;
const PANE_W: f32 = 264.0;
const PREV_H: f32 = 212.0;
const VIS: usize = 8;

struct Pop {
    hwnd: HWND,
    dpi: u32,
    w: f32,
    items: Vec<(Option<u16>, String)>,
    sel: usize,
    hover: Option<usize>,
    top: usize,
    done: Option<Option<usize>>,
    /// 右に出すプレビューの幅（0 なら出さない）
    pane: f32,
    /// 項目ごとの録画対象（プレビュー用）と、作ったサムネイル
    prev: Vec<Source>,
    cache: Vec<Option<Option<IWICBitmap>>>,
}

fn to_wic(t: &thumb::Thumb) -> Option<IWICBitmap> {
    use windows::Win32::Graphics::Imaging::GUID_WICPixelFormat32bppBGR;
    unsafe { crate::gfx::gfx().wic.CreateBitmapFromMemory(t.w, t.h, &GUID_WICPixelFormat32bppBGR, t.w * 4, &t.bgra).ok() }
}

impl Pop {
    /// ホバー中の項目のプレビューを（まだなら）作る
    fn ensure(&mut self) {
        let Some(i) = self.hover.filter(|&i| i < self.prev.len()) else { return };
        if self.cache.len() < self.prev.len() {
            self.cache.resize(self.prev.len(), None);
        }
        if self.cache[i].is_none() {
            self.cache[i] = Some(thumb::source(self.prev[i], 480, 270).and_then(|t| to_wic(&t)));
        }
    }
    fn vis(&self) -> usize {
        self.items.len().clamp(1, VIS)
    }
    fn height(&self) -> f32 {
        let h = self.vis() as f32 * ITEM + 8.0;
        if self.pane > 0.0 { h.max(PREV_H) } else { h }
    }
    fn index_at(&self, x: f32, y: f32) -> Option<usize> {
        if x < 0.0 || x >= self.w || y < 4.0 {
            return None;
        }
        let k = ((y - 4.0) / ITEM) as usize;
        (k < self.vis()).then_some(self.top + k).filter(|&i| i < self.items.len())
    }
    fn show(&mut self, i: usize) {
        if i < self.top {
            self.top = i;
        } else if i >= self.top + self.vis() {
            self.top = i + 1 - self.vis();
        }
    }
}

fn draw_pop(pp: &Pop, p: &Painter, r: R) {
    p.fill(r, tok::R_POP, tok::CTRL);
    p.stroke(r, tok::R_POP, tok::EDGE, 1.0);
    let n = pp.items.len();
    let bar = if n > VIS { 8.0 } else { 0.0 };
    for k in 0..pp.vis().min(n) {
        let i = pp.top + k;
        let (g, text) = &pp.items[i];
        let ir = R::new(r.x + 4.0, r.y + 4.0 + k as f32 * ITEM, pp.w - 8.0 - bar, ITEM);
        if pp.hover == Some(i) {
            p.fill(ir, tok::R_POP - 4.0, tok::CTRL_HOVER);
        }
        if i == pp.sel {
            p.glyph(glyph::CHECK, R::new(ir.x + 4.0, ir.y, 20.0, ITEM), 12.0, tok::TEXT);
        }
        let mut x = ir.x + 30.0;
        if let Some(g) = g {
            p.glyph(*g, R::new(x, ir.y, 18.0, ITEM), 14.0, tok::TEXT_2);
            x += 26.0;
        }
        p.text(text, R::new(x, ir.y, ir.right() - x - 8.0, ITEM), Font::text(13.0), tok::TEXT);
    }
    if n > VIS {
        let track = r.h - 12.0;
        let th = (track * VIS as f32 / n as f32).max(20.0);
        let ty = r.y + 6.0 + (track - th) * pp.top as f32 / (n - VIS) as f32;
        p.fill(R::new(r.x + pp.w - 8.0, ty, 3.0, th), 1.5, tok::TEXT_3);
    }
    if pp.pane > 0.0 {
        p.line(r.x + pp.w, r.y + 10.0, r.x + pp.w, r.bottom() - 10.0, tok::HAIR, 1.0);
        let area = R::new(r.x + pp.w + 12.0, r.y + 12.0, pp.pane - 24.0, (pp.pane - 24.0) * 9.0 / 16.0);
        p.fill(area, 8.0, tok::BG);
        let img = pp.hover.and_then(|i| pp.cache.get(i)).and_then(|c| c.as_ref()).and_then(|c| c.as_ref().map(|b| (b, pp.hover.unwrap_or(0))));
        if let Some((b, i)) = img {
            let (mut bw, mut bh) = (0u32, 0u32);
            unsafe { let _ = b.GetSize(&mut bw, &mut bh); }
            let k = (area.w / bw.max(1) as f32).min(area.h / bh.max(1) as f32);
            let (iw, ih) = (bw as f32 * k, bh as f32 * k);
            let ir = R::new(area.x + (area.w - iw) / 2.0, area.y + (area.h - ih) / 2.0, iw, ih);
            p.image(&format!("pv{i}"), b, ir);
        } else {
            let msg = if pp.hover.is_some_and(|i| i < pp.prev.len()) { "プレビューを取得できません" } else { "" };
            p.text(msg, area, Font::text(12.0).at(Align::Center), tok::TEXT_3);
        }
        if let Some((_, text)) = pp.hover.and_then(|i| pp.items.get(i)) {
            let f = Font::text(12.0);
            p.text(&fit(p, text, f, area.w), R::new(area.x, area.bottom() + 8.0, area.w, 18.0), f, tok::TEXT_2);
        }
    }
}

fn pop_finish(v: Option<usize>) {
    POP.with(|s| {
        if let Some(pp) = s.borrow_mut().as_mut() {
            if pp.done.is_none() {
                pp.done = Some(v);
            }
        }
    });
}

fn pop_update(f: impl FnOnce(&mut Pop)) {
    let h = POP.with(|s| {
        s.borrow_mut().as_mut().map(|pp| {
            f(pp);
            pp.hwnd
        })
    });
    if let Some(h) = h {
        unsafe { let _ = InvalidateRect(Some(h), None, false); }
    }
}

fn pop_key(vk: VIRTUAL_KEY) {
    if vk == VK_UP || vk == VK_DOWN {
        pop_update(|pp| {
            let n = pp.items.len();
            let cur = pp.hover.unwrap_or(pp.sel);
            let i = if vk == VK_UP { cur.saturating_sub(1) } else { (cur + 1).min(n.saturating_sub(1)) };
            pp.hover = Some(i);
            pp.show(i);
        });
    } else if vk == VK_RETURN || vk == VK_SPACE {
        let v = POP.with(|s| s.borrow().as_ref().and_then(|pp| pp.hover));
        pop_finish(v);
    } else if vk == VK_ESCAPE || vk == VK_TAB || vk == VK_MENU {
        pop_finish(None);
    }
}

extern "system" fn pop_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            POP.with(|s| {
                if let Some(pp) = s.borrow_mut().as_mut() {
                    pp.ensure();
                }
            });
            paint(hwnd, &POP_P, tok::CTRL, |p| {
                POP.with(|s| {
                    if let Some(pp) = s.borrow().as_ref() {
                        draw_pop(pp, p, R::new(0.0, 0.0, pp.w + pp.pane, pp.height()));
                    }
                })
            });
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            let dpi = POP.with(|s| s.borrow().as_ref().map_or(96, |pp| pp.dpi));
            let (x, y) = pt(lp, dpi);
            let changed = POP.with(|s| {
                let mut s = s.borrow_mut();
                let Some(pp) = s.as_mut() else { return false };
                let i = pp.index_at(x, y);
                let c = i.is_some() && pp.hover != i;
                if c {
                    pp.hover = i;
                }
                c
            });
            if changed {
                unsafe { let _ = InvalidateRect(Some(hwnd), None, false); }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            // 一覧の外を押したら閉じる
            let dpi = POP.with(|s| s.borrow().as_ref().map_or(96, |pp| pp.dpi));
            let (x, y) = pt(lp, dpi);
            let inside = POP.with(|s| s.borrow().as_ref().is_some_and(|pp| x >= 0.0 && x < pp.w + pp.pane && y >= 0.0 && y < pp.height()));
            if !inside {
                pop_finish(None);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let dpi = POP.with(|s| s.borrow().as_ref().map_or(96, |pp| pp.dpi));
            let (x, y) = pt(lp, dpi);
            let i = POP.with(|s| s.borrow().as_ref().and_then(|pp| pp.index_at(x, y)));
            if i.is_some() {
                pop_finish(i);
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            pop_finish(None);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// 部品の下（入らなければ上）に一覧を出し、選んだ番号を返す。閉じるまで戻らない
fn dropdown(ctl: Ctl, items: Vec<(Option<u16>, String)>, sel: usize) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let (main, dpi) = UI.with(|u| {
        let u = u.borrow();
        (u.hwnd, u.dpi)
    });
    let a = ctl_rect(ctl);
    let mut pop = Pop { hwnd: HWND::default(), dpi, w: a.w, items, sel, hover: Some(sel), top: 0, done: None, pane: 0.0, prev: Vec::new(), cache: Vec::new() };
    if ctl == Ctl::Source {
        pop.prev = UI.with(|u| u.borrow().sources.iter().map(|s| s.source).collect());
        pop.pane = PANE_W;
        pop.w = a.w - PANE_W;
    }
    pop.show(sel);
    let s = dpi as f32 / 96.0;
    let (pw, ph) = (((a.w + pop.pane) * s).round() as i32, (pop.height() * s).round() as i32);
    unsafe {
        let mut below = POINT { x: (a.x * s).round() as i32, y: ((a.bottom() + 4.0) * s).round() as i32 };
        let mut above = POINT { x: below.x, y: ((a.y - 4.0) * s).round() as i32 - ph };
        let _ = ClientToScreen(main, &mut below);
        let _ = ClientToScreen(main, &mut above);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromWindow(main, MONITOR_DEFAULTTONEAREST), &mut mi);
        let at = if below.y + ph > mi.rcWork.bottom && above.y >= mi.rcWork.top { above } else { below };
        let inst = GetModuleHandleW(None).unwrap_or_default();
        let Ok(h) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("LightCaptureXPopup"),
            w!(""),
            WS_POPUP,
            at.x,
            at.y,
            pw,
            ph,
            Some(main),
            None,
            Some(inst.into()),
            None,
        ) else {
            return None;
        };
        // Windows 11 の角丸（それ以前は四角のまま）
        let round = 2u32;
        let _ = DwmSetWindowAttribute(h, DWMWINDOWATTRIBUTE(33), &round as *const u32 as _, 4);
        pop.hwnd = h;
        POP.with(|s| *s.borrow_mut() = Some(pop));
        UI.with(|u| {
            let mut u = u.borrow_mut();
            u.open = Some(ctl);
            u.hover = None;
        });
        let _ = InvalidateRect(Some(main), None, false);
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        SetCapture(h);

        // メニューと同じく、キーとホイールはこちらで受ける
        let mut msg = MSG::default();
        loop {
            if POP.with(|s| s.borrow().as_ref().is_none_or(|pp| pp.done.is_some())) {
                break;
            }
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 == 0 {
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
            if r.0 == -1 {
                break;
            }
            match msg.message {
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    pop_key(VIRTUAL_KEY(msg.wParam.0 as u16));
                    continue;
                }
                WM_KEYUP | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR => continue,
                WM_MOUSEWHEEL => {
                    let d = (msg.wParam.0 >> 16) as u16 as i16 as i32 / 120;
                    pop_update(|pp| {
                        let max = pp.items.len().saturating_sub(pp.vis()) as i32;
                        pp.top = (pp.top as i32 - d * 3).clamp(0, max) as usize;
                    });
                    continue;
                }
                _ => {}
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if GetCapture() == h {
            let _ = ReleaseCapture();
        }
        POP_P.with(|s| *s.borrow_mut() = None);
        let _ = DestroyWindow(h);
    }
    let res = POP.with(|s| s.borrow_mut().take()).and_then(|pp| pp.done.flatten());
    UI.with(|u| u.borrow_mut().open = None);
    invalidate_main();
    res
}

// ---- 録音するアプリを選ぶウィンドウ ----

const PW: f32 = 440.0;
const PH: f32 = 520.0;
const PROW: f32 = 52.0;
const PLIST: R = R::new(12.0, 52.0, PW - 24.0, PH - 52.0 - 76.0);
const PREFRESH: R = R::new(16.0, PH - 56.0, 140.0, 40.0);
const PDONE: R = R::new(PW - 16.0 - 104.0, PH - 56.0, 104.0, 40.0);

#[derive(Clone, Copy, PartialEq, Debug)]
enum PHit {
    Row(usize),
    Refresh,
    Done,
}

struct Pick {
    hwnd: HWND,
    dpi: u32,
    entries: Vec<AppEntry>,
    hover: Option<PHit>,
    press: Option<PHit>,
    focus: usize,
    kb: bool,
    scroll: f32,
    tracking: bool,
}

impl Pick {
    fn max_scroll(&self) -> f32 {
        (self.entries.len() as f32 * PROW - PLIST.h).max(0.0)
    }
    fn hit(&self, x: f32, y: f32, recording: bool) -> Option<PHit> {
        if PREFRESH.contains(x, y) {
            return Some(PHit::Refresh);
        }
        if PDONE.contains(x, y) {
            return Some(PHit::Done);
        }
        if recording || !PLIST.contains(x, y) {
            return None;
        }
        let i = ((y - PLIST.y + self.scroll) / PROW) as usize;
        (i < self.entries.len()).then_some(PHit::Row(i))
    }
    fn show(&mut self, i: usize) {
        let top = i as f32 * PROW;
        if top < self.scroll {
            self.scroll = top;
        } else if top + PROW > self.scroll + PLIST.h {
            self.scroll = top + PROW - PLIST.h;
        }
    }
}

fn draw_picker(pk: &Pick, sel: &[String], recording: bool, p: &Painter) {
    let desc = if recording { "録画中は変更できません。" } else { "チェックしたアプリの音だけを録音します。" };
    p.text(desc, R::new(20.0, 14.0, PW - 40.0, 28.0), Font::text(12.5), tok::TEXT_2);
    p.clip(PLIST);
    for (i, e) in pk.entries.iter().enumerate() {
        let r = R::new(PLIST.x, PLIST.y + i as f32 * PROW - pk.scroll, PLIST.w, PROW);
        if r.bottom() < PLIST.y || r.y > PLIST.bottom() {
            continue;
        }
        let checked = sel.iter().any(|a| a.eq_ignore_ascii_case(&e.exe));
        let box_ = r.inset(0.0, 2.0);
        if !recording && pk.hover == Some(PHit::Row(i)) {
            let press = pk.press == pk.hover;
            p.fill(box_, tok::R_CTRL, if press { tok::CTRL_PRESS } else { tok::CTRL_HOVER });
        }
        app_icon(p, e.icon.as_ref(), &e.exe, &e.name, R::new(r.x + 12.0, r.y + (PROW - 28.0) / 2.0, 28.0, 28.0));
        let tw = r.w - 52.0 - 132.0;
        let (c1, c2) = if recording { (tok::TEXT_3, tok::TEXT_3) } else { (tok::TEXT, tok::TEXT_3) };
        p.text(&e.name, R::new(r.x + 52.0, r.y + 8.0, tw, 20.0), Font::text(13.0), c1);
        p.text(&e.exe, R::new(r.x + 52.0, r.y + 27.0, tw, 16.0), Font::text(11.0), c2);
        let sr = R::new(r.right() - 48.0 - 80.0, r.y, 80.0, PROW);
        if e.playing {
            let f = Font::text(12.0);
            let w = p.measure("再生中", f);
            p.text("再生中", R::new(sr.right() - w - 2.0, sr.y, w + 2.0, sr.h), f, tok::TEXT_2);
            p.glyph(glyph::VOLUME, R::new(sr.right() - w - 22.0, sr.y, 16.0, sr.h), 12.0, tok::TEXT_2);
        } else if !e.running {
            p.text("未起動", sr, Font::text(12.0).at(Align::Right), tok::TEXT_3);
        }
        let cb = R::new(r.right() - 16.0 - 20.0, r.y + (PROW - 20.0) / 2.0, 20.0, 20.0);
        if checked {
            p.fill(cb, 5.0, if recording { tok::TEXT_3 } else { tok::TEXT });
            p.glyph(glyph::CHECK, cb, 12.0, tok::BG);
        } else {
            p.fill(cb, 5.0, tok::CTRL);
            p.stroke(cb, 5.0, tok::EDGE, 1.5);
        }
        if pk.kb && pk.focus == i && !recording {
            p.stroke(box_.inset(1.0, 1.0), tok::R_CTRL - 1.0, tok::FOCUS, 2.0);
        }
    }
    p.unclip();
    let total = pk.entries.len() as f32 * PROW;
    if total > PLIST.h {
        let th = (PLIST.h * PLIST.h / total).max(24.0);
        let ty = PLIST.y + (PLIST.h - th) * pk.scroll / pk.max_scroll();
        p.fill(R::new(PW - 7.0, ty, 3.0, th), 1.5, tok::TEXT_3);
    }
    p.line(0.0, PH - 72.5, PW, PH - 72.5, tok::HAIR, 1.0);

    let btn = |h: PHit| (pk.hover == Some(h), pk.hover == Some(h) && pk.press == Some(h));
    let (hv, pr) = btn(PHit::Refresh);
    if pr {
        p.scale(PREFRESH, 0.96);
    }
    if hv {
        p.fill(PREFRESH, tok::R_CTRL, if pr { tok::CTRL_PRESS } else { tok::CTRL_HOVER });
    }
    let fg = if hv { tok::TEXT } else { tok::TEXT_2 };
    p.glyph(glyph::REFRESH, R::new(PREFRESH.x + 12.0, PREFRESH.y, 18.0, PREFRESH.h), 13.0, fg);
    p.text("一覧を更新", R::new(PREFRESH.x + 38.0, PREFRESH.y, PREFRESH.w - 44.0, PREFRESH.h), Font::text(13.0), fg);
    if pr {
        p.unscale();
    }
    let (hv, pr) = btn(PHit::Done);
    if pr {
        p.scale(PDONE, 0.96);
    }
    p.fill(PDONE, tok::R_CTRL, if pr { tok::CTRL_PRESS } else if hv { tok::SEG_ON_HOVER } else { tok::SEG_ON });
    p.text("完了", PDONE, Font::strong(13.0).at(Align::Center), tok::TEXT);
    if pr {
        p.unscale();
    }
}

fn icon_px(dpi: u32) -> i32 {
    if dpi >= 168 { 64 } else { 48 }
}

/// 一覧から取れたアイコンを、メイン画面でも使えるように覚えておく
fn remember_icons(entries: &[AppEntry]) {
    UI.with(|u| {
        let mut u = u.borrow_mut();
        for e in entries {
            let k = e.exe.to_lowercase();
            if e.running {
                match u.names.iter_mut().find(|(x, _)| *x == k) {
                    Some(slot) => slot.1 = e.name.clone(),
                    None => u.names.push((k.clone(), e.name.clone())),
                }
            }
            if let Some(b) = &e.icon {
                match u.icons.iter_mut().find(|(x, _)| *x == k) {
                    Some(slot) => slot.1 = b.clone(),
                    None => u.icons.push((k, b.clone())),
                }
            }
        }
    });
}

fn pick_refresh() {
    let (sel, dpi) = UI.with(|u| (u.borrow().apps.clone(), u.borrow().dpi));
    let entries = apps::gather(&sel, icon_px(dpi));
    remember_icons(&entries);
    let h = PICK.with(|s| {
        s.borrow_mut().as_mut().map(|pk| {
            pk.entries = entries;
            pk.focus = pk.focus.min(pk.entries.len().saturating_sub(1));
            pk.scroll = pk.scroll.min(pk.max_scroll());
            pk.hwnd
        })
    });
    if let Some(h) = h {
        unsafe { let _ = InvalidateRect(Some(h), None, false); }
    }
}

fn pick_toggle(i: usize) {
    if UI.with(|u| u.borrow().status != Status::Ready && !matches!(u.borrow().status, Status::Saved(_) | Status::Failed(..))) {
        return;
    }
    let Some(exe) = PICK.with(|s| s.borrow().as_ref().and_then(|pk| pk.entries.get(i).map(|e| e.exe.clone()))) else { return };
    UI.with(|u| {
        let mut u = u.borrow_mut();
        match u.apps.iter().position(|x| x.eq_ignore_ascii_case(&exe)) {
            Some(k) => {
                u.apps.remove(k);
            }
            None => u.apps.push(exe),
        }
        if !u.apps.is_empty() {
            u.sys = SYS_APPS;
        }
    });
    save_config();
    invalidate_all();
}

fn close_picker() {
    let Some(h) = PICK.with(|s| s.borrow().as_ref().map(|pk| pk.hwnd)) else { return };
    let main = UI.with(|u| u.borrow().hwnd);
    unsafe {
        // 先に親を有効に戻さないと、別のアプリが前面に出てしまう
        let _ = EnableWindow(main, true);
        PICK_P.with(|s| *s.borrow_mut() = None);
        let _ = DestroyWindow(h);
    }
    PICK.with(|s| *s.borrow_mut() = None);
}

fn pick_activate(h: PHit) {
    match h {
        PHit::Row(i) => {
            PICK.with(|s| {
                if let Some(pk) = s.borrow_mut().as_mut() {
                    pk.focus = i;
                }
            });
            pick_toggle(i);
        }
        PHit::Refresh => pick_refresh(),
        PHit::Done => close_picker(),
    }
}

fn pick_invalidate() {
    if let Some(h) = PICK.with(|s| s.borrow().as_ref().map(|pk| pk.hwnd)) {
        unsafe { let _ = InvalidateRect(Some(h), None, false); }
    }
}

fn recording_now() -> bool {
    UI.with(|u| matches!(u.borrow().status, Status::Recording | Status::Starting | Status::Saving))
}

extern "system" fn pick_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let dpi = || PICK.with(|s| s.borrow().as_ref().map_or(96, |pk| pk.dpi));
    match msg {
        WM_PAINT => {
            let (sel, rec) = UI.with(|u| (u.borrow().apps.clone(), recording_now_of(&u.borrow())));
            paint(hwnd, &PICK_P, tok::BG, |p| {
                PICK.with(|s| {
                    if let Some(pk) = s.borrow().as_ref() {
                        draw_picker(pk, &sel, rec, p);
                    }
                })
            });
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            PICK_P.with(|s| {
                if let Some(p) = s.borrow().as_ref() {
                    p.resize((lp.0 & 0xffff) as u32, ((lp.0 >> 16) & 0xffff) as u32);
                }
            });
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let d = (wp.0 & 0xffff) as u32;
            PICK.with(|s| {
                if let Some(pk) = s.borrow_mut().as_mut() {
                    pk.dpi = d;
                }
            });
            PICK_P.with(|s| {
                if let Some(p) = s.borrow().as_ref() {
                    p.set_dpi(d as f32);
                }
            });
            let r = unsafe { &*(lp.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                // 縮むときは WM_PAINT が来ないので、倍率を変えた絵を描き直させる
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = pt(lp, dpi());
            let rec = recording_now();
            let (changed, track) = PICK.with(|s| {
                let mut s = s.borrow_mut();
                let Some(pk) = s.as_mut() else { return (false, false) };
                let h = pk.hit(x, y, rec);
                let c = pk.hover != h;
                pk.hover = h;
                let t = !pk.tracking;
                pk.tracking = true;
                (c, t)
            });
            if track {
                track_leave(hwnd);
            }
            if changed {
                pick_invalidate();
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            PICK.with(|s| {
                if let Some(pk) = s.borrow_mut().as_mut() {
                    pk.hover = None;
                    pk.tracking = false;
                }
            });
            pick_invalidate();
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let d = (wp.0 >> 16) as u16 as i16 as f32 / 120.0;
            PICK.with(|s| {
                if let Some(pk) = s.borrow_mut().as_mut() {
                    pk.scroll = (pk.scroll - d * PROW * 2.0).clamp(0.0, pk.max_scroll());
                    pk.hover = None;
                }
            });
            pick_invalidate();
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = pt(lp, dpi());
            let rec = recording_now();
            PICK.with(|s| {
                if let Some(pk) = s.borrow_mut().as_mut() {
                    pk.press = pk.hit(x, y, rec);
                    pk.kb = false;
                }
            });
            unsafe { SetCapture(hwnd) };
            pick_invalidate();
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = pt(lp, dpi());
            let rec = recording_now();
            let (pressed, now) = PICK.with(|s| {
                let mut s = s.borrow_mut();
                s.as_mut().map_or((None, None), |pk| (pk.press.take(), pk.hit(x, y, rec)))
            });
            unsafe { let _ = ReleaseCapture(); }
            if let (Some(a), Some(b)) = (pressed, now) {
                if a == b {
                    pick_activate(a);
                }
            }
            pick_invalidate();
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            PICK.with(|s| {
                if let Some(pk) = s.borrow_mut().as_mut() {
                    pk.press = None;
                }
            });
            pick_invalidate();
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let vk = VIRTUAL_KEY(wp.0 as u16);
            if vk == VK_ESCAPE || vk == VK_RETURN {
                close_picker();
            } else if vk == VK_F5 {
                pick_refresh();
            } else if vk == VK_UP || vk == VK_DOWN {
                PICK.with(|s| {
                    if let Some(pk) = s.borrow_mut().as_mut() {
                        let n = pk.entries.len();
                        pk.focus = if !pk.kb {
                            pk.focus
                        } else if vk == VK_UP {
                            pk.focus.saturating_sub(1)
                        } else {
                            (pk.focus + 1).min(n.saturating_sub(1))
                        };
                        pk.kb = true;
                        let f = pk.focus;
                        pk.show(f);
                    }
                });
                pick_invalidate();
            } else if vk == VK_SPACE {
                let f = PICK.with(|s| {
                    let mut s = s.borrow_mut();
                    s.as_mut().map(|pk| {
                        pk.kb = true;
                        pk.focus
                    })
                });
                if let Some(f) = f {
                    pick_toggle(f);
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            close_picker();
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn recording_now_of(u: &Ui) -> bool {
    matches!(u.status, Status::Recording | Status::Starting | Status::Saving)
}

/// アプリの一覧ウィンドウを出す。閉じるまで戻らない（メイン画面は操作できなくする）
fn open_picker() {
    if PICK.with(|s| s.borrow().is_some()) {
        return;
    }
    let (main, dpi, sel) = UI.with(|u| {
        let mut u = u.borrow_mut();
        u.hover = None;
        (u.hwnd, u.dpi, u.apps.clone())
    });
    let entries = apps::gather(&sel, icon_px(dpi));
    remember_icons(&entries);
    unsafe {
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let s = dpi as f32 / 96.0;
        let mut rc = RECT { left: 0, top: 0, right: (PW * s).round() as i32, bottom: (PH * s).round() as i32 };
        let _ = AdjustWindowRectExForDpi(&mut rc, style, false, WINDOW_EX_STYLE(0), dpi);
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        let mut mr = RECT::default();
        let _ = GetWindowRect(main, &mut mr);
        let x = (mr.left + mr.right - w) / 2;
        let y = (mr.top + mr.bottom - h) / 2;
        let inst = GetModuleHandleW(None).unwrap_or_default();
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("LightCaptureXPicker"),
            w!("録音するアプリ"),
            style,
            x,
            y,
            w,
            h,
            Some(main),
            None,
            Some(inst.into()),
            None,
        ) else {
            return;
        };
        dark_title(hwnd);
        PICK.with(|s| {
            *s.borrow_mut() = Some(Pick { hwnd, dpi, entries, hover: None, press: None, focus: 0, kb: false, scroll: 0.0, tracking: false })
        });
        let _ = EnableWindow(main, false);
        let _ = ShowWindow(hwnd, SW_SHOW);
        update::cleanup();
        if UI.with(|u| u.borrow().auto_update) {
            update::check(hwnd);
        }
        let mut msg = MSG::default();
        while PICK.with(|s| s.borrow().is_some()) {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 == 0 {
                close_picker();
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
            if r.0 == -1 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    invalidate_main();
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
    let text = UI.with(|u| {
        let u = u.borrow();
        let src = u.preferred.clone().unwrap_or_default();
        let mic = if u.mic == 0 { String::new() } else { u.mics.get(u.mic - 1).map(|x| x.0.clone()).unwrap_or_default() };
        let codec = u.codecs.get(u.codec).map(|c| c.label()).unwrap_or_default();
        let gpu = if u.gpu == 0 { String::new() } else { u.gpus.get(u.gpu - 1).cloned().unwrap_or_default() };
        format!(
            "source={src}\nquality={}\nfps={}\nres={}\nmic={mic}\nsysmode={}\napps={}\ncursor={}\ncodec={codec}\ngpu={gpu}\ndir={}\nhotkey={},{}\ntray_close={}\nnotify={}\nauto_update={}\nbitrate={}\n",
            u.quality,
            u.fps,
            u.res,
            u.sys,
            u.apps.join("|"),
            u.cursor as u8,
            u.out_dir,
            u.hotkey.0,
            u.hotkey.1,
            u.tray_close as u8,
            u.notify as u8,
            u.auto_update as u8,
            u.bitrate
        )
    });
    let p = config_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let _ = std::fs::write(p, text);
}

// ---- 一覧の更新 ----

/// keep に挙げた名前のうち、最初に見つかったものを選択する
fn refresh_sources(keep: &[Option<String>]) {
    let hwnd = UI.with(|u| u.borrow().hwnd);
    let list = list_sources(hwnd);
    let sel = keep.iter().flatten().find_map(|k| list.iter().position(|s| &s.name == k)).unwrap_or(0);
    UI.with(|u| {
        let mut u = u.borrow_mut();
        u.sources = list;
        u.src = sel;
    });
}

fn refresh_mics(keep: Option<String>) {
    let mics = audio::list_mics();
    let sel = keep.and_then(|k| mics.iter().position(|m| m.0 == k)).map(|i| i + 1).unwrap_or(0);
    UI.with(|u| {
        let mut u = u.borrow_mut();
        u.mics = mics;
        u.mic = sel;
    });
}

fn selected_source_name() -> Option<String> {
    UI.with(|u| {
        let u = u.borrow();
        u.sources.get(u.src).map(|s| s.name.clone())
    })
}

fn selected_mic_id() -> Option<String> {
    UI.with(|u| {
        let u = u.borrow();
        if u.mic == 0 { None } else { u.mics.get(u.mic - 1).map(|x| x.0.clone()) }
    })
}

// ---- 録画 ----

fn default_dir() -> PathBuf {
    let videos = unsafe {
        SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None).ok().and_then(|p| {
            let s = p.to_string().ok();
            windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _));
            s
        })
    };
    videos.map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("LightCaptureX")
}

/// 録画の保存先（設定したフォルダ。未設定ならビデオ\LightCaptureX）
fn output_dir() -> PathBuf {
    let d = UI.with(|u| u.borrow().out_dir.clone());
    if d.is_empty() { default_dir() } else { PathBuf::from(d) }
}

fn new_file_path() -> PathBuf {
    let t = unsafe { GetLocalTime() };
    output_dir().join(format!(
        "LightCaptureX_{:04}{:02}{:02}_{:02}{:02}{:02}.mp4",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    ))
}

fn set_status(s: Status) {
    UI.with(|u| u.borrow_mut().status = s);
}

/// 時間のかかる処理の前に、いまの状態を描いておく
fn repaint_now() {
    let h = UI.with(|u| u.borrow().hwnd);
    unsafe {
        let _ = InvalidateRect(Some(h), None, false);
        let _ = UpdateWindow(h);
    }
}

fn toggle() {
    let s = UI.with(|u| u.borrow().status.clone());
    match s {
        Status::Recording => stop_recording(),
        Status::Starting | Status::Saving => {}
        _ => start_recording(),
    }
}

fn start_recording() {
    // 起動後に開いたゲームのウィンドウも拾えるよう、開始直前に一覧を取り直す
    let preferred = UI.with(|u| u.borrow().preferred.clone());
    refresh_sources(&[preferred, selected_source_name()]);
    let path = new_file_path();
    let settings = UI.with(|u| {
        let u = u.borrow();
        let source = u.sources.get(u.src)?.source;
        Some(Settings {
            source,
            fps: FPS[u.fps.min(FPS.len() - 1)],
            quality: u.quality as u32,
            max_height: RES[u.res.min(RES.len() - 1)].1,
            cursor: u.cursor,
            system_audio: match u.sys {
                0 => false,
                SYS_APPS => !u.apps.is_empty(),
                _ => true,
            },
            apps: if u.sys == SYS_APPS { u.apps.clone() } else { Vec::new() },
            mic: if u.mic == 0 { None } else { u.mics.get(u.mic - 1).map(|x| x.0.clone()) },
            codec: u.codecs.get(u.codec).copied().unwrap_or(Codec::H264),
            gpu: if u.gpu == 0 { None } else { u.gpus.get(u.gpu - 1).cloned() },
        bitrate_mbps: BITRATES[u.bitrate.min(BITRATES.len() - 1)],
            path: path.to_string_lossy().into_owned(),
        })
    });
    let Some(s) = settings else {
        set_status(Status::Failed("開始できませんでした".into(), "録画できる画面が見つかりません".into()));
        invalidate_all();
        return;
    };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    save_config();
    set_status(Status::Starting);
    repaint_now();
    match recorder::start(s) {
        Ok(r) => {
            let hwnd = UI.with(|u| {
                let mut u = u.borrow_mut();
                u.info = r.info.clone();
                u.rec = Some((r, Instant::now(), path));
                u.status = Status::Recording;
                u.elapsed = 0;
                u.size_mb = 0.0;
                u.blink = true;
                u.hwnd
            });
            UI.with(|u| {
                let mut u = u.borrow_mut();
                u.fps_now = 0.0;
                u.dropped = 0;
                u.mbps = 0.0;
                u.cpu = None;
                u.gpu_load = None;
                u.fps_hist.clear();
                u.last_fresh = 0;
                u.last_bytes = 0;
                u.last_t = None;
                u.meter = [0.0; 2];
                u.hold = [0.0; 2];
                u.hold_age = [0; 2];
                u.note = None;
            });
            sysmon::start();
            unsafe {
                SetTimer(Some(hwnd), TIMER, 500, None);
                SetTimer(Some(hwnd), METER, 50, None);
            }
            update_tray(true);
            notify("録画を開始しました", "");
        }
        Err(e) => {
            log(&format!("start: {e}"));
            let m = e.message();
            notify("開始できませんでした", &m);
            set_status(Status::Failed("開始できませんでした".into(), m));
        }
    }
    invalidate_all();
}

fn stop_recording() {
    let Some((r, _, path)) = UI.with(|u| u.borrow_mut().rec.take()) else { return };
    let hwnd = UI.with(|u| u.borrow().hwnd);
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER);
        let _ = KillTimer(Some(hwnd), METER);
    }
    sysmon::stop();
    set_status(Status::Saving);
    repaint_now();
    let res = r.stop();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match res {
        Ok(()) => {
            notify("録画を保存しました", &name);
            set_status(Status::Saved(name));
        }
        Err(e) => {
            log(&format!("stop: {e}"));
            let m = e.message();
            notify("保存中にエラーが発生しました", &m);
            set_status(Status::Failed("保存中にエラーが発生しました".into(), m));
        }
    }
    UI.with(|u| {
        let mut u = u.borrow_mut();
        u.last_file = Some(path);
        u.meter = [0.0; 2];
        u.hold = [0.0; 2];
    });
    update_tray(false);
    if UI.with(|u| u.borrow().tab) == Tab::List {
        refresh_list();
    }
    invalidate_all();
}

const TIP: &str = "LightCaptureX";

/// トレーのアイコンと説明を、いまの状態に合わせる。red は点滅の明暗
fn update_tray(red: bool) {
    let (hwnd, on, rec, secs) = UI.with(|u| {
        let u = u.borrow();
        (u.hwnd, u.tray_on, u.status == Status::Recording, u.elapsed)
    });
    if !on {
        return;
    }
    let tip = if rec { format!("{TIP} — 録画中 {}", recs::fmt_dur(secs)) } else { TIP.to_string() };
    tray::set(hwnd, rec && red, &tip);
}

/// トレーから通知を出す（設定でオフなら出さない）
fn notify(title: &str, text: &str) {
    let (hwnd, on) = UI.with(|u| (u.borrow().hwnd, u.borrow().notify && u.borrow().tray_on));
    if on {
        tray::balloon(hwnd, title, text);
    }
}

fn window_visible() -> bool {
    let h = UI.with(|u| u.borrow().hwnd);
    unsafe { IsWindowVisible(h).as_bool() && !IsIconic(h).as_bool() }
}

fn tick() {
    let fresh = stats::FRESH.load(Ordering::Relaxed);
    let dropped = stats::DROPPED.load(Ordering::Relaxed);
    let (cpu, gpu) = sysmon::read();
    let now = Instant::now();
    let blink = UI.with(|u| {
        let mut u = u.borrow_mut();
        let (secs, path) = match &u.rec {
            Some((_, t, p)) => (t.elapsed().as_secs(), p.clone()),
            None => return None,
        };
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let dt = u.last_t.map_or(0.5, |l| now.duration_since(l).as_secs_f32()).max(0.05);
        u.fps_now = fresh.saturating_sub(u.last_fresh) as f32 / dt;
        u.mbps = size.saturating_sub(u.last_bytes) as f32 * 8.0 / 1_000_000.0 / dt;
        u.last_fresh = fresh;
        u.last_bytes = size;
        u.last_t = Some(now);
        u.dropped = dropped;
        u.cpu = cpu;
        u.gpu_load = gpu;
        let f = u.fps_now;
        u.fps_hist.push(f);
        if u.fps_hist.len() > 120 {
            u.fps_hist.remove(0);
        }
        u.elapsed = secs;
        u.size_mb = size as f64 / 1_048_576.0;
        u.blink = !u.blink;
        Some(u.blink)
    });
    if let Some(b) = blink {
        update_tray(b);
    }
    if window_visible() {
        invalidate_main();
    }
}

/// 音量メーター（録画中だけ約 50ms ごと）
fn meter_tick() {
    let (s, m) = stats::take_peaks();
    let show = UI.with(|u| {
        let mut u = u.borrow_mut();
        for (k, pk) in [s, m].into_iter().enumerate() {
            let lv = db_level(pk);
            u.meter[k] = lv.max(u.meter[k] * 0.86);
            if lv >= u.hold[k] {
                u.hold[k] = lv;
                u.hold_age[k] = 0;
            } else {
                u.hold_age[k] += 1;
                if u.hold_age[k] > 24 {
                    u.hold[k] = (u.hold[k] - 0.02).max(lv);
                }
            }
        }
        u.tab == Tab::Rec && u.status == Status::Recording
    });
    if show && window_visible() {
        invalidate_main();
    }
}

fn open_path(p: &Path) {
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(p.as_os_str()), None, None, SW_SHOWNORMAL);
    }
}

fn show_in_folder(f: &Path) {
    unsafe {
        ShellExecuteW(None, w!("open"), w!("explorer.exe"), &HSTRING::from(format!("/select,\"{}\"", f.display())), None, SW_SHOWNORMAL);
    }
}

fn refresh_list() {
    let dir = output_dir();
    let skip = UI.with(|u| u.borrow().rec.as_ref().map(|r| r.2.clone()));
    let list = recs::scan(&dir, skip.as_deref());
    let free = recs::free_bytes(&dir);
    UI.with(|u| {
        let mut u = u.borrow_mut();
        u.list = list;
        u.free = free;
        u.list_off = u.list_off.min(u.list.len().saturating_sub(LVIS));
    });
    request_thumbs();
}

static THUMB_DONE: std::sync::Mutex<Vec<(String, Option<thumb::Thumb>)>> = std::sync::Mutex::new(Vec::new());
const WM_THUMB: u32 = WM_APP + 5;

/// まだ無い録画のサムネイルを、別スレッドで作らせる
fn request_thumbs() {
    let (hwnd, todo) = UI.with(|u| {
        let mut u = u.borrow_mut();
        let todo: Vec<String> = u.list.iter().map(|e| e.path.to_string_lossy().into_owned()).filter(|p| !u.thumbs.iter().any(|t| &t.0 == p) && !u.thumb_busy.contains(p)).collect();
        u.thumb_busy.extend(todo.iter().cloned());
        (u.hwnd.0 as isize, todo)
    });
    if todo.is_empty() || hwnd == 0 {
        return;
    }
    std::thread::spawn(move || {
        unsafe { let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED); }
        for path in todo {
            let t = thumb::file(&path, 160, 90);
            if let Ok(mut q) = THUMB_DONE.lock() {
                q.push((path, t));
            }
            unsafe { let _ = PostMessageW(Some(HWND(hwnd as *mut core::ffi::c_void)), WM_THUMB, WPARAM(0), LPARAM(0)); }
        }
    });
}

fn take_thumbs() {
    let done: Vec<_> = THUMB_DONE.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
    UI.with(|u| {
        let mut u = u.borrow_mut();
        for (path, t) in done {
            u.thumb_busy.retain(|p| p != &path);
            if let Some(b) = t.and_then(|t| to_wic(&t)) {
                u.thumbs.push((path, b));
            }
        }
    });
}

/// フォルダーを選ぶダイアログ
fn pick_folder(owner: HWND, start: &str) -> Option<PathBuf> {
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    unsafe {
        let d: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let o = d.GetOptions().ok()?;
        d.SetOptions(o | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = d.SetTitle(w!("録画の保存先を選択"));
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(start), None) {
            let _ = d.SetFolder(&item);
        }
        d.Show(Some(owner)).ok()?;
        let r = d.GetResult().ok()?;
        let name = r.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as _));
        s.map(PathBuf::from)
    }
}

// ---- ショートカット ----

fn hotkey_reg(hwnd: HWND, hk: (u32, u32)) -> bool {
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY);
        RegisterHotKey(Some(hwnd), HOTKEY, HOT_KEY_MODIFIERS(hk.0) | MOD_NOREPEAT, hk.1).is_ok()
    }
}

fn start_capture() {
    let hwnd = UI.with(|u| {
        let mut u = u.borrow_mut();
        u.capturing = true;
        u.note = None;
        u.hwnd
    });
    // 入力待ちのあいだに、いまのキーが録画を始めてしまわないよう外しておく
    unsafe { let _ = UnregisterHotKey(Some(hwnd), HOTKEY); }
}

/// 入力待ちをやめて、いまのショートカットを登録し直す
fn cancel_capture() {
    if !UI.with(|u| u.borrow().capturing) {
        return;
    }
    let (hwnd, hk) = UI.with(|u| (u.borrow().hwnd, u.borrow().hotkey));
    let ok = hotkey_reg(hwnd, hk);
    UI.with(|u| {
        let mut u = u.borrow_mut();
        u.capturing = false;
        u.hotkey_ok = ok;
    });
    invalidate_main();
}

fn apply_hotkey(hk: (u32, u32)) {
    let (hwnd, old) = UI.with(|u| (u.borrow().hwnd, u.borrow().hotkey));
    if hotkey_reg(hwnd, hk) {
        UI.with(|u| {
            let mut u = u.borrow_mut();
            u.hotkey = hk;
            u.hotkey_ok = true;
            u.capturing = false;
            u.note = Some(format!("{} に変更しました", key_text(hk)));
        });
        save_config();
    } else {
        let ok = hotkey_reg(hwnd, old);
        UI.with(|u| {
            let mut u = u.borrow_mut();
            u.hotkey_ok = ok;
            u.capturing = false;
            u.note = Some("そのキーはほかのアプリが使っています".into());
        });
    }
    invalidate_main();
}

/// 入力待ちのとき、押されたキーをショートカットにする。処理したら true
fn capture_key(vk: u32) -> bool {
    if !UI.with(|u| u.borrow().capturing) {
        return false;
    }
    // 修飾キーだけでは決めない
    if matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5) {
        return true;
    }
    if vk == VK_ESCAPE.0 as u32 {
        cancel_capture();
        return true;
    }
    let down = |k: VIRTUAL_KEY| unsafe { GetKeyState(k.0 as i32) } < 0;
    let mut mods = 0u32;
    if down(VK_CONTROL) { mods |= MOD_CONTROL.0; }
    if down(VK_MENU) { mods |= MOD_ALT.0; }
    if down(VK_SHIFT) { mods |= MOD_SHIFT.0; }
    if down(VK_LWIN) || down(VK_RWIN) { mods |= MOD_WIN.0; }
    if mods == 0 && !(0x70..=0x87).contains(&vk) {
        UI.with(|u| u.borrow_mut().note = Some("Ctrl・Alt・Shift のどれかと組み合わせてください（F1〜F24 は単独で使えます）".into()));
        invalidate_main();
        return true;
    }
    apply_hotkey((mods, vk));
    true
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

// ---- 操作 ----

fn save_and_repaint() {
    save_config();
    invalidate_main();
}

fn set_value(c: Ctl, v: usize) {
    UI.with(|u| {
        let mut u = u.borrow_mut();
        let n = seg_labels(&u, c).len();
        if n == 0 {
            return;
        }
        let v = v.min(n - 1);
        match c {
            Ctl::Quality => u.quality = v,
            Ctl::Fps => u.fps = v,
            Ctl::Res => u.res = v,
            Ctl::Codec => u.codec = v,
            Ctl::Sys => u.sys = v,
            Ctl::Tab => {
                u.tab = TABS[v];
                u.hover = None;
                u.note = None;
            }
            _ => {}
        }
    });
    if c == Ctl::Tab {
        cancel_capture();
        if UI.with(|u| u.borrow().tab) != Tab::Rec {
            refresh_list();
        }
    }
    save_and_repaint();
}

fn activate(h: Hit) {
    match h.ctl {
        Ctl::Source => {
            // 開くたびに一覧を取り直す（新しく起動したゲームも選べるように）
            refresh_sources(&[selected_source_name()]);
            let (items, sel) = UI.with(|u| {
                let u = u.borrow();
                (u.sources.iter().map(|s| (Some(source_glyph(s.source)), display_name(&s.name).to_string())).collect(), u.src)
            });
            if let Some(i) = dropdown(Ctl::Source, items, sel).filter(|_| UI.with(|u| enabled(&u.borrow(), Ctl::Source))) {
                UI.with(|u| {
                    let mut u = u.borrow_mut();
                    u.src = i;
                    u.preferred = u.sources.get(i).map(|s| s.name.clone());
                });
            }
            save_and_repaint();
        }
        Ctl::Gpu => {
            let (items, sel) = UI.with(|u| {
                let u = u.borrow();
                let items = std::iter::once("自動".to_string()).chain(u.gpus.iter().cloned()).map(|g| (None, g)).collect();
                (items, u.gpu)
            });
            if let Some(i) = dropdown(Ctl::Gpu, items, sel).filter(|_| UI.with(|u| enabled(&u.borrow(), Ctl::Gpu))) {
                UI.with(|u| u.borrow_mut().gpu = i);
            }
            save_and_repaint();
        }
        Ctl::Bitrate => {
            let (items, sel) = UI.with(|u| (BITRATES.iter().enumerate().map(|(i, _)| (None, bitrate_text(i))).collect::<Vec<_>>(), u.borrow().bitrate));
            if let Some(i) = dropdown(Ctl::Bitrate, items, sel).filter(|_| UI.with(|u| enabled(&u.borrow(), Ctl::Bitrate))) {
                UI.with(|u| u.borrow_mut().bitrate = i);
            }
            save_and_repaint();
        }
        Ctl::Mic => {
            refresh_mics(selected_mic_id());
            let (items, sel) = UI.with(|u| {
                let u = u.borrow();
                let items = std::iter::once("なし".to_string()).chain(u.mics.iter().map(|m| m.1.clone())).map(|m| (None, m)).collect();
                (items, u.mic)
            });
            if let Some(i) = dropdown(Ctl::Mic, items, sel).filter(|_| UI.with(|u| enabled(&u.borrow(), Ctl::Mic))) {
                UI.with(|u| u.borrow_mut().mic = i);
            }
            save_and_repaint();
        }
        Ctl::Cursor => {
            UI.with(|u| {
                let mut u = u.borrow_mut();
                u.cursor = !u.cursor;
            });
            save_and_repaint();
        }
        Ctl::Apps => open_picker(),
        Ctl::SetDir => {
            let (hwnd, cur) = UI.with(|u| (u.borrow().hwnd, u.borrow().out_dir.clone()));
            if let Some(d) = pick_folder(hwnd, &cur) {
                UI.with(|u| {
                    let mut u = u.borrow_mut();
                    u.out_dir = d.display().to_string();
                    u.list_off = 0;
                    u.note = None;
                });
                refresh_list();
            }
            save_and_repaint();
        }
        Ctl::SetDirOpen => {
            let dir = output_dir();
            let _ = std::fs::create_dir_all(&dir);
            open_path(&dir);
        }
        Ctl::SetKey => {
            if UI.with(|u| u.borrow().capturing) {
                cancel_capture();
            } else {
                start_capture();
            }
            invalidate_main();
        }
        Ctl::SetKeyReset => {
            cancel_capture();
            apply_hotkey((0, VK_F9.0 as u32));
        }
        Ctl::TogTray => {
            UI.with(|u| {
                let mut u = u.borrow_mut();
                u.tray_close = !u.tray_close;
            });
            save_and_repaint();
        }
        Ctl::TogNotify => {
            UI.with(|u| {
                let mut u = u.borrow_mut();
                u.notify = !u.notify;
            });
            save_and_repaint();
        }
        Ctl::TogUpd => {
            UI.with(|u| {
                let mut u = u.borrow_mut();
                u.auto_update = !u.auto_update;
            });
            save_and_repaint();
        }
        Ctl::UpdBtn => {
            let hwnd = UI.with(|u| u.borrow().hwnd);
            match update::state() {
                update::State::Available(_) => {
                    if UI.with(|u| u.borrow().status == Status::Recording) {
                        notify("録画中は更新できません", "録画を止めてからもう一度押してください");
                    } else {
                        update::apply(hwnd);
                    }
                }
                _ => update::check(hwnd),
            }
            invalidate_main();
        }
        Ctl::ListRefresh => {
            refresh_list();
            invalidate_main();
        }
        Ctl::ListOpen | Ctl::ListShow | Ctl::ListDel => {
            let Some(e) = UI.with(|u| u.borrow().list.get(h.idx).map(|e| e.path.clone())) else { return };
            match h.ctl {
                Ctl::ListOpen => open_path(&e),
                Ctl::ListShow => show_in_folder(&e),
                _ => {
                    let ok = recs::recycle(&e);
                    UI.with(|u| u.borrow_mut().note = Some(if ok { "ごみ箱に移動しました".into() } else { "削除できませんでした".into() }));
                    refresh_list();
                }
            }
            invalidate_main();
        }
        Ctl::Rec => toggle(),
        Ctl::Folder => open_folder(),
        c => {
            set_value(c, h.idx);
            // アプリがまだ選ばれていなければ、そのまま選ぶ画面を出す
            if c == Ctl::Sys && h.idx == SYS_APPS && UI.with(|u| u.borrow().apps.is_empty()) {
                open_picker();
            }
        }
    }
}

fn on_key(vk: VIRTUAL_KEY) {
    let (focus, hotkey_ok, hk) = UI.with(|u| (u.borrow().focus, u.borrow().hotkey_ok, u.borrow().hotkey));
    if vk == VK_TAB {
        let back = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        UI.with(|u| {
            let mut u = u.borrow_mut();
            let list: Vec<Ctl> = ORDER.iter().copied().filter(|&c| enabled(&u, c)).collect();
            if list.is_empty() {
                return;
            }
            let cur = u.focus.and_then(|f| list.iter().position(|&c| c == f));
            let n = list.len();
            let next = match (cur, back) {
                (None, false) => 0,
                (None, true) => n - 1,
                (Some(i), false) => (i + 1) % n,
                (Some(i), true) => (i + n - 1) % n,
            };
            u.focus = Some(list[next]);
            u.kb = true;
        });
        invalidate_main();
        return;
    }
    // ショートカットが登録できなかったときは、ウィンドウが前面のあいだだけキーで代用する
    if hk.0 == 0 && vk.0 as u32 == hk.1 && !hotkey_ok {
        toggle();
        return;
    }
    let Some(c) = focus.filter(|&c| UI.with(|u| enabled(&u.borrow(), c))) else { return };
    UI.with(|u| u.borrow_mut().kb = true);
    if vk == VK_LEFT || vk == VK_RIGHT || vk == VK_UP || vk == VK_DOWN {
        if is_seg(c) {
            let v = UI.with(|u| seg_value(&u.borrow(), c));
            let v = if vk == VK_LEFT || vk == VK_UP { v.saturating_sub(1) } else { v + 1 };
            set_value(c, v);
        } else if matches!(c, Ctl::Source | Ctl::Gpu | Ctl::Mic | Ctl::Bitrate) && (vk == VK_DOWN || vk == VK_UP) {
            activate(Hit { ctl: c, idx: 0 });
        }
    } else if (vk == VK_SPACE || vk == VK_RETURN) && !is_seg(c) {
        activate(Hit { ctl: c, idx: 0 });
    }
    invalidate_main();
}

// ---- ウィンドウ ----

fn pt(lp: LPARAM, dpi: u32) -> (f32, f32) {
    let s = 96.0 / dpi.max(1) as f32;
    let x = (lp.0 & 0xffff) as u16 as i16 as f32;
    let y = ((lp.0 >> 16) & 0xffff) as u16 as i16 as f32;
    (x * s, y * s)
}

fn track_leave(hwnd: HWND) {
    let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
    unsafe { let _ = TrackMouseEvent(&mut t); }
}

fn invalidate_main() {
    let h = UI.with(|u| u.borrow().hwnd);
    if !h.is_invalid() {
        unsafe { let _ = InvalidateRect(Some(h), None, false); }
    }
}

fn invalidate_all() {
    invalidate_main();
    pick_invalidate();
}

fn paint(hwnd: HWND, slot: &'static Slot, bg: C, draw: impl FnOnce(&Painter)) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        BeginPaint(hwnd, &mut ps);
        let lost = slot.with(|s| {
            let mut s = s.borrow_mut();
            if s.is_none() {
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                *s = Painter::for_hwnd(hwnd, rc.right as u32, rc.bottom as u32, GetDpiForWindow(hwnd) as f32).ok();
            }
            let failed = match s.as_ref() {
                Some(p) => {
                    p.begin(bg);
                    draw(p);
                    p.end().is_err()
                }
                None => false,
            };
            // GPU のリセットなどで描画先が失われたら、次の描画で作り直す
            if failed {
                *s = None;
            }
            failed
        });
        let _ = EndPaint(hwnd, &ps);
        if lost {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

fn colorref(c: C) -> COLORREF {
    let b = |v: f32| (v * 255.0).round() as u32;
    COLORREF(b(c.0) | b(c.1) << 8 | b(c.2) << 16)
}

/// タイトルバーを暗い色にする（Windows 11。古い Windows では無視される）
fn dark_title(hwnd: HWND) {
    unsafe {
        let on = 1i32;
        let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(20), &on as *const i32 as _, 4);
        let cap = colorref(tok::BG);
        let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(35), &cap as *const COLORREF as _, 4);
    }
}

fn init_values() {
    let c = load_config();
    let num = |k: &str, d: usize| cfg(&c, k).and_then(|v| v.parse().ok()).unwrap_or(d);
    let preferred = cfg(&c, "source").filter(|s| !s.is_empty()).map(String::from);
    UI.with(|u| u.borrow_mut().preferred = preferred.clone());
    refresh_sources(&[preferred]);
    let codecs = encoder::available_codecs();
    let codecs = if codecs.is_empty() { vec![Codec::H264] } else { codecs };
    let gpus = d3d::adapter_names();
    let dflt = default_dir();
    let apps: Vec<String> = cfg(&c, "apps").unwrap_or("").split('|').filter(|a| !a.is_empty()).map(String::from).collect();
    UI.with(|u| {
        let mut u = u.borrow_mut();
        u.quality = num("quality", 2).min(QUALITIES.len() - 1);
        u.fps = num("fps", 1).min(FPS.len() - 1);
        u.res = num("res", 0).min(RES.len() - 1);
        u.codec = cfg(&c, "codec").and_then(|v| codecs.iter().position(|x| x.label() == v)).unwrap_or(0);
        u.gpu = cfg(&c, "gpu").and_then(|v| gpus.iter().position(|x| x == v)).map(|i| i + 1).unwrap_or(0);
        u.codecs = codecs;
        u.gpus = gpus;
        // 旧設定（system=0/1）からも引き継ぐ
        u.sys = num("sysmode", num("system", 1)).min(SYS_MODES.len() - 1);
        u.cursor = num("cursor", 1) != 0;
        u.apps = apps.clone();
        u.out_dir = cfg(&c, "dir").filter(|d| !d.is_empty()).map(String::from).unwrap_or_else(|| dflt.display().to_string());
        let hk = cfg(&c, "hotkey").and_then(|v| v.split_once(',')).and_then(|(m, k)| Some((m.parse::<u32>().ok()?, k.parse::<u32>().ok()?)));
        u.hotkey = hk.filter(|h| h.1 != 0).unwrap_or((0, VK_F9.0 as u32));
        u.tray_close = num("tray_close", 1) != 0;
        u.notify = num("notify", 1) != 0;
        u.auto_update = num("auto_update", 1) != 0;
        u.bitrate = num("bitrate", 0).min(BITRATES.len() - 1);
    });
    refresh_mics(cfg(&c, "mic").filter(|s| !s.is_empty()).map(String::from));
    if !apps.is_empty() {
        let dpi = UI.with(|u| u.borrow().dpi);
        remember_icons(&apps::gather(&apps, icon_px(dpi)));
    }
}

// ---- タスクトレー ----

static TB_MSG: AtomicU32 = AtomicU32::new(0);
static HINTED: AtomicBool = AtomicBool::new(false);

fn show_window() {
    let h = UI.with(|u| u.borrow().hwnd);
    unsafe {
        let _ = ShowWindow(h, SW_SHOW);
        let _ = ShowWindow(h, SW_RESTORE);
        let _ = SetForegroundWindow(h);
    }
    invalidate_main();
}

fn esc(s: &str) -> String {
    s.replace('&', "&&")
}

fn tray_menu() {
    let hwnd = UI.with(|u| u.borrow().hwnd);
    if UI.with(|u| u.borrow().list.is_empty()) || UI.with(|u| u.borrow().status != Status::Recording) {
        refresh_list();
    }
    let idle = !recording_now();
    if idle {
        refresh_sources(&[selected_source_name()]);
    }
    let items = UI.with(|u| {
        use tray::Item;
        let u = u.borrow();
        let rec = u.status == Status::Recording;
        let busy = matches!(u.status, Status::Starting | Status::Saving);
        let hk = if u.hotkey_ok { format!("\t{}", key_text(u.hotkey)) } else { String::new() };
        let mut v = vec![
            Item::cmd(1, format!("{}{hk}", if rec { "録画を停止" } else { "録画を開始" })).on(!busy),
            Item::cmd(2, "ウィンドウを開く"),
            Item::Sep,
        ];
        let recent: Vec<Item> = if u.list.is_empty() {
            vec![Item::cmd(0, "（まだありません）").on(false)]
        } else {
            u.list.iter().take(6).enumerate().map(|(i, e)| Item::cmd(100 + i as u32, esc(&e.name))).collect()
        };
        v.push(Item::sub("最近の録画", recent));
        v.push(Item::cmd(3, "録画一覧を開く"));
        v.push(Item::cmd(4, "保存先のフォルダーを開く"));
        v.push(Item::Sep);
        let srcs: Vec<Item> = u
            .sources
            .iter()
            .enumerate()
            .take(60)
            .map(|(i, s)| Item::cmd(200 + i as u32, esc(display_name(&s.name))).checked(i == u.src).on(idle))
            .collect();
        v.push(Item::sub("録画対象", srcs).on(idle));
        let seg = |c: Ctl, base: u32, cur: usize| -> Vec<Item> {
            seg_labels(&u, c).iter().enumerate().map(|(i, l)| Item::cmd(base + i as u32, l.to_string()).checked(i == cur).on(idle)).collect()
        };
        let settings = vec![
            Item::sub("画質", seg(Ctl::Quality, 300, u.quality)),
            Item::sub("フレームレート", seg(Ctl::Fps, 400, u.fps)),
            Item::sub("解像度", seg(Ctl::Res, 500, u.res)),
            Item::cmd(600, "カーソルを録画に含める").checked(u.cursor).on(idle),
        ];
        v.push(Item::sub("録画の設定", settings).on(idle));
        let mut mic = vec![Item::cmd(720, "なし").checked(u.mic == 0).on(idle)];
        mic.extend(u.mics.iter().enumerate().take(30).map(|(i, m)| Item::cmd(721 + i as u32, esc(&m.1)).checked(u.mic == i + 1).on(idle)));
        let audio = vec![Item::sub("PC の音", seg(Ctl::Sys, 710, u.sys)), Item::sub("マイク", mic)];
        v.push(Item::sub("音声", audio).on(idle));
        v.push(Item::Sep);
        v.push(Item::cmd(800, "トレーから通知する").checked(u.notify));
        v.push(Item::cmd(801, "×で閉じたらトレーに残す").checked(u.tray_close));
        v.push(Item::Sep);
        if let update::State::Available(ver) = update::state() {
            v.push(Item::cmd(850, format!("v{ver} に更新する")));
        }
        v.push(Item::cmd(900, "終了"));
        v
    });
    let Some(id) = tray::popup(hwnd, &items) else { return };
    tray_cmd(hwnd, id);
}

fn tray_cmd(hwnd: HWND, id: u32) {
    match id {
        1 => toggle(),
        2 => show_window(),
        3 => {
            show_window();
            set_value(Ctl::Tab, 1);
        }
        4 => {
            let dir = output_dir();
            let _ = std::fs::create_dir_all(&dir);
            open_path(&dir);
        }
        100..=105 => {
            if let Some(f) = UI.with(|u| u.borrow().list.get((id - 100) as usize).map(|e| e.path.clone())) {
                open_path(&f);
            }
        }
        200..=299 => UI.with(|u| {
            let mut u = u.borrow_mut();
            let i = (id - 200) as usize;
            if i < u.sources.len() {
                u.src = i;
                u.preferred = u.sources.get(i).map(|s| s.name.clone());
            }
            drop(u);
            save_and_repaint();
        }),
        300..=399 => set_value(Ctl::Quality, (id - 300) as usize),
        400..=499 => set_value(Ctl::Fps, (id - 400) as usize),
        500..=599 => set_value(Ctl::Res, (id - 500) as usize),
        600 => activate(Hit { ctl: Ctl::Cursor, idx: 0 }),
        710..=719 => {
            let i = (id - 710) as usize;
            set_value(Ctl::Sys, i);
            // アプリが未選択なら、選ぶ画面のあるウィンドウを出す
            if i == SYS_APPS && UI.with(|u| u.borrow().apps.is_empty()) {
                show_window();
            }
        }
        720..=799 => {
            UI.with(|u| u.borrow_mut().mic = (id - 720) as usize);
            save_and_repaint();
        }
        800 => activate(Hit { ctl: Ctl::TogNotify, idx: 0 }),
        801 => activate(Hit { ctl: Ctl::TogTray, idx: 0 }),
        850 => activate(Hit { ctl: Ctl::UpdBtn, idx: 0 }),
        900 => quit(hwnd),
        _ => {}
    }
}

/// 録画を止めて保存し、トレーのアイコンを消して終わる
fn quit(hwnd: HWND) {
    tray::remove(hwnd);
    stop_recording();
    save_config();
    unsafe { let _ = DestroyWindow(hwnd); }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let dpi = || UI.with(|u| u.borrow().dpi);
    match msg {
        WM_CREATE => {
            let d = unsafe { GetDpiForWindow(hwnd) }.max(96);
            UI.with(|u| {
                let mut u = u.borrow_mut();
                u.hwnd = hwnd;
                u.dpi = d;
            });
            init_values();
            let hk = UI.with(|u| u.borrow().hotkey);
            let ok = hotkey_reg(hwnd, hk);
            UI.with(|u| {
                let mut u = u.borrow_mut();
                u.hotkey_ok = ok;
                u.tray_on = true;
            });
            tray::add(hwnd, TIP);
            LRESULT(0)
        }
        WM_PAINT => {
            paint(hwnd, &MAIN_P, tok::BG, |p| UI.with(|u| draw_main(&u.borrow(), p)));
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            MAIN_P.with(|s| {
                if let Some(p) = s.borrow().as_ref() {
                    p.resize((lp.0 & 0xffff) as u32, ((lp.0 >> 16) & 0xffff) as u32);
                }
            });
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let d = (wp.0 & 0xffff) as u32;
            UI.with(|u| u.borrow_mut().dpi = d);
            MAIN_P.with(|s| {
                if let Some(p) = s.borrow().as_ref() {
                    p.set_dpi(d as f32);
                }
            });
            let r = unsafe { &*(lp.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                // 縮むときは WM_PAINT が来ないので、倍率を変えた絵を描き直させる
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = pt(lp, dpi());
            let (changed, track) = UI.with(|u| {
                let mut u = u.borrow_mut();
                let h = hit_main(&u, x, y);
                let c = u.hover != h;
                u.hover = h;
                let t = !u.tracking;
                u.tracking = true;
                (c, t)
            });
            if track {
                track_leave(hwnd);
            }
            if changed {
                invalidate_main();
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            UI.with(|u| {
                let mut u = u.borrow_mut();
                u.hover = None;
                u.tracking = false;
            });
            invalidate_main();
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = pt(lp, dpi());
            UI.with(|u| {
                let mut u = u.borrow_mut();
                let h = hit_main(&u, x, y);
                u.press = h;
                u.kb = false;
                if let Some(h) = h {
                    u.focus = Some(h.ctl);
                }
            });
            unsafe { SetCapture(hwnd) };
            invalidate_main();
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = pt(lp, dpi());
            let pressed = UI.with(|u| u.borrow_mut().press.take());
            unsafe { let _ = ReleaseCapture(); }
            let now = UI.with(|u| hit_main(&u.borrow(), x, y));
            invalidate_main();
            if let Some(h) = pressed.filter(|p| Some(*p) == now) {
                activate(h);
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            UI.with(|u| u.borrow_mut().press = None);
            invalidate_main();
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = (wp.0 >> 16) as u16 as i16 as i32;
            let moved = UI.with(|u| {
                let mut u = u.borrow_mut();
                if u.tab != Tab::List || u.list.len() <= LVIS {
                    return false;
                }
                let max = u.list.len() - LVIS;
                let old = u.list_off;
                u.list_off = if delta > 0 { old.saturating_sub(1) } else { (old + 1).min(max) };
                u.hover = None;
                u.list_off != old
            });
            if moved {
                invalidate_main();
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if !capture_key(wp.0 as u32) && msg == WM_KEYDOWN {
                on_key(VIRTUAL_KEY(wp.0 as u16));
            } else if !UI.with(|u| u.borrow().capturing) && msg == WM_SYSKEYDOWN {
                return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
            }
            LRESULT(0)
        }
        WM_ACTIVATEAPP if wp.0 == 0 => {
            pop_finish(None);
            cancel_capture();
            LRESULT(0)
        }
        m if m == update::WM_UPDATE => {
            match update::state() {
                update::State::Available(v) if !ANNOUNCED.swap(true, Ordering::Relaxed) => notify("新しいバージョンがあります", &format!("v{v} · 設定タブから更新できます")),
                update::State::Failed(m) if UI.with(|u| u.borrow().tab == Tab::Set) => notify("更新できませんでした", &m),
                _ => {}
            }
            invalidate_main();
            LRESULT(0)
        }
        m if m == update::WM_UPDATE_QUIT => {
            quit(hwnd);
            LRESULT(0)
        }
        WM_THUMB => {
            take_thumbs();
            invalidate_main();
            LRESULT(0)
        }
        WM_SHOWME => {
            show_window();
            LRESULT(0)
        }
        m if m == tray::WM_TRAY => {
            match (lp.0 & 0xffff) as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_window(),
                WM_RBUTTONUP | WM_CONTEXTMENU => tray_menu(),
                // 通知を押したら保存先を開く
                x if x == WM_USER + 5 => {
                    let dir = output_dir();
                    open_path(&dir);
                }
                _ => {}
            }
            LRESULT(0)
        }
        m if m != 0 && m == TB_MSG.load(Ordering::Relaxed) => {
            // エクスプローラーが再起動したら、トレーのアイコンを付け直す
            tray::add(hwnd, TIP);
            update_tray(true);
            LRESULT(0)
        }
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wp.0 != 0 {
                tray::remove(hwnd);
                stop_recording();
                save_config();
            }
            LRESULT(0)
        }
        WM_HOTKEY if wp.0 as i32 == HOTKEY => {
            // 一覧を開いたまま録画を始めると、古い一覧の番号が設定に入るので先に閉じる
            pop_finish(None);
            toggle();
            LRESULT(0)
        }
        WM_TIMER => {
            if wp.0 == METER {
                meter_tick();
            } else {
                tick();
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let to_tray = UI.with(|u| u.borrow().tray_close && u.borrow().tray_on);
            if to_tray {
                pop_finish(None);
                cancel_capture();
                close_picker();
                unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
                // 初めて隠したときだけ、残っていることと終わらせ方を知らせる
                if !HINTED.swap(true, Ordering::Relaxed) {
                    tray::balloon(hwnd, "LightCaptureX はトレーで動いています", "終了するには、トレーのアイコンを右クリックして「終了」を選んでください");
                }
            } else {
                quit(hwnd);
            }
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

const STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0);

fn register(name: PCWSTR, proc_: WNDPROC, style: WNDCLASS_STYLES, bg: C, icon: bool) {
    unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style,
            lpfnWndProc: proc_,
            hInstance: inst.into(),
            hIcon: if icon { LoadIconW(Some(inst.into()), PCWSTR(1 as _)).unwrap_or_default() } else { HICON::default() },
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: CreateSolidBrush(colorref(bg)),
            lpszClassName: name,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

pub fn run() {
    unsafe {
        // すでに起動していたら、そのウィンドウを出して終わる
        // 更新直後は、古いほうが終わるのを少し待つ
        let updated = std::env::args().nth(1).as_deref() == Some("--updated");
        let mut m = windows::Win32::System::Threading::CreateMutexW(None, true, w!("Local\\LightCaptureX.single"));
        for _ in 0..50 {
            if !(updated && m.is_ok() && GetLastError() == ERROR_ALREADY_EXISTS) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            m = windows::Win32::System::Threading::CreateMutexW(None, true, w!("Local\\LightCaptureX.single"));
        }
        if m.is_ok() && GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(h) = FindWindowW(w!("LightCaptureX"), None) {
                let _ = PostMessageW(Some(h), WM_SHOWME, WPARAM(0), LPARAM(0));
            }
            return;
        }
        TB_MSG.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
    }
    tray::dark_menus();
    register(w!("LightCaptureX"), Some(wndproc), WNDCLASS_STYLES(0), tok::BG, true);
    register(w!("LightCaptureXPopup"), Some(pop_proc), CS_DROPSHADOW, tok::CTRL, false);
    register(w!("LightCaptureXPicker"), Some(pick_proc), WNDCLASS_STYLES(0), tok::BG, true);
    unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("LightCaptureX"),
            w!("LightCaptureX"),
            STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            W as i32,
            H as i32,
            None,
            None,
            Some(inst.into()),
            None,
        ) else {
            return;
        };
        let dpi = GetDpiForWindow(hwnd).max(96);
        let s = dpi as f32 / 96.0;
        let mut rc = RECT { left: 0, top: 0, right: (W * s).round() as i32, bottom: (H * s).round() as i32 };
        let _ = AdjustWindowRectExForDpi(&mut rc, STYLE, false, WINDOW_EX_STYLE(0), dpi);
        let _ = SetWindowPos(hwnd, None, 0, 0, rc.right - rc.left, rc.bottom - rc.top, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        dark_title(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// ---- 動作確認用: 画面を PNG に書き出す（ウィンドウは出さず、録画・録音も始めない）----

pub fn render_ui(dir: &str) {
    let dir = PathBuf::from(dir);
    let _ = std::fs::create_dir_all(&dir);
    UI.with(|u| {
        let mut u = u.borrow_mut();
        u.dpi = 96;
        u.hotkey_ok = true;
    });
    init_values();
    let (base_sys, base_apps) = UI.with(|u| (u.borrow().sys, u.borrow().apps.clone()));
    let demo_apps = vec!["VALORANT-Win64-Shipping.exe".to_string(), "Discord.exe".to_string()];
    let entries = apps::gather(&demo_apps, 64);
    remember_icons(&entries);
    // 実際のフォルダーは読まず、見本の一覧と保存先を使う
    let demo_list: Vec<RecEntry> = (0..9)
        .map(|i| RecEntry {
            path: PathBuf::from(format!("C:\\Videos\\LightCaptureX\\LightCaptureX_20261020_{:02}0000.mp4", 22 - i)),
            name: format!("LightCaptureX_20261020_{:02}0000.mp4", 22 - i),
            size: 900_000_000 / (i as u64 + 1),
            secs: if i == 3 { None } else { Some(2400 / (i as u64 + 1)) },
            when: (2026, 10, 20, 22 - i as u16, 0),
        })
        .collect();
    let demo_dir = "D:\\Recordings\\Games\\LightCaptureX".to_string();

    type Setup = Box<dyn Fn(&mut Ui)>;
    let scenes: Vec<(&str, Setup, Option<Ctl>)> = vec![
        ("idle", Box::new(|_| {}), None),
        (
            "focus",
            Box::new(|u| {
                u.focus = Some(Ctl::Res);
                u.kb = true;
                u.hover = Some(Hit { ctl: Ctl::Quality, idx: 3 });
            }),
            None,
        ),
        ("hover-rec", Box::new(|u| u.hover = Some(Hit { ctl: Ctl::Rec, idx: 0 })), None),
        (
            "apps",
            Box::new(|u| {
                u.sys = SYS_APPS;
                u.apps = vec!["VALORANT-Win64-Shipping.exe".into(), "Discord.exe".into()];
            }),
            None,
        ),
        (
            "recording",
            Box::new(|u| {
                u.status = Status::Recording;
                u.elapsed = 754;
                u.size_mb = 1284.6;
                u.fps = 3;
                u.blink = true;
                u.info = "GeForce RTX 4070 / H.264 Encoder".into();
                u.hover = Some(Hit { ctl: Ctl::Rec, idx: 0 });
            }),
            None,
        ),
        (
            "live",
            Box::new(|u| {
                u.status = Status::Recording;
                u.elapsed = 754;
                u.size_mb = 1284.6;
                u.fps = 3;
                u.blink = true;
                u.info = "GeForce RTX 4070 / H.264 Encoder".into();
                u.fps_now = 144.0;
                u.dropped = 0;
                u.mbps = 48.2;
                u.cpu = Some(4);
                u.gpu_load = Some(12);
                u.fps_hist = (0..90).map(|i| 143.0 + ((i * 7) % 5) as f32 * 0.4).collect();
                u.meter = [0.72, 0.38];
                u.hold = [0.85, 0.52];
            }),
            None,
        ),
        ("list", Box::new(|u| u.tab = Tab::List), None),
        ("list-hover", Box::new(|u| { u.tab = Tab::List; u.hover = Some(Hit { ctl: Ctl::ListDel, idx: 1 }); u.list_off = 0; }), None),
        ("list-empty", Box::new(|u| { u.tab = Tab::List; u.list.clear(); }), None),
        ("settings", Box::new(|u| u.tab = Tab::Set), None),
        ("settings-capture", Box::new(|u| { u.tab = Tab::Set; u.capturing = true; u.hover = Some(Hit { ctl: Ctl::SetKey, idx: 0 }); }), None),
        ("settings-note", Box::new(|u| { u.tab = Tab::Set; u.tray_close = false; u.note = Some("そのキーはほかのアプリが使っています".into()); u.hotkey = (MOD_CONTROL.0 | MOD_SHIFT.0, 0x52); }), None),
        ("starting", Box::new(|u| u.status = Status::Starting), None),
        (
            "error",
            Box::new(|u| u.status = Status::Failed("開始できませんでした".into(), "（エラーの詳細）".into())),
            None,
        ),
        ("saved", Box::new(|u| u.status = Status::Saved("LightCaptureX_20261020_120000.mp4".into())), None),
        ("dropdown-gpu", Box::new(|_| {}), Some(Ctl::Gpu)),
        ("dropdown-source", Box::new(|_| {}), Some(Ctl::Source)),
    ];

    for dpi in [96u32, 144] {
        let s = dpi as f32 / 96.0;
        for (name, setup, open) in &scenes {
            UI.with(|u| {
                let mut u = u.borrow_mut();
                u.status = Status::Ready;
                u.hover = None;
                u.press = None;
                u.focus = None;
                u.kb = false;
                u.elapsed = 0;
                u.sys = base_sys;
                u.apps = base_apps.clone();
                u.open = *open;
                u.tab = Tab::Rec;
                u.capturing = false;
                u.note = None;
                u.tray_close = true;
                u.hotkey = (0, VK_F9.0 as u32);
                u.out_dir = demo_dir.clone();
                u.free = Some(412_000_000_000);
                u.list = demo_list.clone();
                u.list_off = 0;
                u.fps_hist.clear();
                u.meter = [0.0; 2];
                u.hold = [0.0; 2];
                u.dropped = 0;
                u.cpu = None;
                u.gpu_load = None;
                u.mics = vec![("demo".into(), "マイク (USB Microphone)".into())];
                u.mic = 1;
                setup(&mut u);
            });
            let pop = open.map(|c| {
                let (items, sel): (Vec<(Option<u16>, String)>, usize) = UI.with(|u| {
                    let u = u.borrow();
                    match c {
                        Ctl::Source => (u.sources.iter().map(|s| (Some(source_glyph(s.source)), display_name(&s.name).to_string())).collect(), u.src),
                        _ => (std::iter::once("自動".to_string()).chain(u.gpus.iter().cloned()).map(|g| (None, g)).collect(), u.gpu),
                    }
                });
                let a = ctl_rect(c);
                let mut pp = Pop { hwnd: HWND::default(), dpi, w: a.w, items, sel, hover: Some(sel), top: 0, done: None, pane: 0.0, prev: Vec::new(), cache: Vec::new() };
                if c == Ctl::Source {
            pp.pane = PANE_W;
            pp.w = a.w - PANE_W;
            pp.prev = vec![Source::Monitor(0); pp.items.len()];
        }
        pp.show(sel);
                if pp.items.len() > 1 {
                    pp.hover = Some((sel + 1) % pp.items.len());
                }
                (pp, a)
            });
            let path = dir.join(format!("{name}@{dpi}.png"));
            let r = crate::gfx::render_png(&path.to_string_lossy(), (W * s).round() as u32, (H * s).round() as u32, dpi as f32, |p| {
                p.begin(tok::BG);
                UI.with(|u| draw_main(&u.borrow(), p));
                if let Some((pp, a)) = &pop {
                    let r = R::new(a.x, a.bottom() + 4.0, pp.w + pp.pane, pp.height());
                    p.fill(R::new(r.x, r.y + 4.0, r.w, r.h), tok::R_POP, tok::SHADE);
                    draw_pop(pp, p, r);
                }
                let _ = p.end();
            });
            if let Err(e) = r {
                log(&format!("render {name}: {e}"));
            }
        }
        // アプリの一覧
        for (name, rec) in [("picker", false), ("picker-recording", true)] {
            let pk = Pick {
                hwnd: HWND::default(),
                dpi,
                entries: entries.clone(),
                hover: Some(PHit::Row(1)),
                press: None,
                focus: 0,
                kb: true,
                scroll: 0.0,
                tracking: false,
            };
            let path = dir.join(format!("{name}@{dpi}.png"));
            let r = crate::gfx::render_png(&path.to_string_lossy(), (PW * s).round() as u32, (PH * s).round() as u32, dpi as f32, |p| {
                p.begin(tok::BG);
                draw_picker(&pk, &demo_apps[..1], rec, p);
                let _ = p.end();
            });
            if let Err(e) = r {
                log(&format!("render {name}: {e}"));
            }
        }
    }
}
