//! Direct2D で自前描画する設定画面。F9 で録画の開始/停止。
//! 配置は `ctl_rect` の1か所で決め、描画・当たり判定・画像出力（--render-ui）で共有する。
use crate::apps::{self, AppEntry};
use crate::capture::{list_sources, Source, SourceInfo};
use crate::encoder::{self, Codec};
use crate::gfx::{glyph, tok, Align, Font, Painter, C, R};
use crate::recorder::{self, Recorder, Settings};
use crate::{audio, d3d, log};
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

const QUALITIES: [&str; 4] = ["低", "中", "高", "最高"];
const FPS: [u32; 2] = [30, 60];
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
    Cursor,
    Sys,
    Apps,
    Mic,
    Rec,
    Folder,
}
const ORDER: [Ctl; 12] = [
    Ctl::Source,
    Ctl::Quality,
    Ctl::Fps,
    Ctl::Res,
    Ctl::Codec,
    Ctl::Gpu,
    Ctl::Cursor,
    Ctl::Sys,
    Ctl::Apps,
    Ctl::Mic,
    Ctl::Rec,
    Ctl::Folder,
];

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
const H: f32 = 492.0;
const PAD: f32 = 20.0;
const HEAD: f32 = 44.0;
const ROW: f32 = 44.0;
const SOURCE: R = R::new(PAD, 56.0, W - PAD * 2.0, 40.0);
const VIDEO: R = R::new(PAD, 112.0, 412.0, 320.0);
const AUDIO: R = R::new(448.0, 112.0, 292.0, 184.0);
const PANEL: R = R::new(448.0, 312.0, 292.0, 120.0);
const FOOTER: R = R::new(PAD, 448.0, W - PAD * 2.0, 32.0);
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
        Ctl::Cursor => field(VIDEO, 5, LABEL_V),
        Ctl::Sys => field(AUDIO, 0, LABEL_A),
        Ctl::Apps => field(AUDIO, 1, LABEL_A),
        Ctl::Mic => field(AUDIO, 2, LABEL_A),
        Ctl::Rec => R::new(PANEL.x + 16.0, PANEL.bottom() - 56.0, PANEL.w - 32.0, 40.0),
        Ctl::Folder => R::new(FOOTER.right() - 140.0, FOOTER.y, 140.0, 32.0),
    }
}

/// 押せる範囲（高さ 40 以上。カーソルの行は見出しごと押せる）
fn hit_rect(c: Ctl) -> R {
    match c {
        Ctl::Cursor => row(VIDEO, 5).inset(8.0, 2.0),
        Ctl::Source | Ctl::Rec => ctl_rect(c),
        _ => ctl_rect(c).inset(0.0, -4.0),
    }
}

fn segs(r: R, n: usize) -> impl Iterator<Item = R> {
    let w = (r.w - tok::SEG_PAD * 2.0) / n.max(1) as f32;
    (0..n).map(move |i| R::new(r.x + tok::SEG_PAD + i as f32 * w, r.y + tok::SEG_PAD, w, r.h - tok::SEG_PAD * 2.0))
}

fn is_seg(c: Ctl) -> bool {
    matches!(c, Ctl::Quality | Ctl::Fps | Ctl::Res | Ctl::Codec | Ctl::Sys)
}

fn seg_labels(u: &Ui, c: Ctl) -> Vec<String> {
    match c {
        Ctl::Quality => QUALITIES.map(String::from).to_vec(),
        Ctl::Fps => FPS.map(|f| f.to_string()).to_vec(),
        Ctl::Res => RES.map(|r| r.0.to_string()).to_vec(),
        Ctl::Codec => u.codecs.iter().map(|c| c.label().to_string()).collect(),
        Ctl::Sys => SYS_MODES.map(String::from).to_vec(),
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
        _ => 0,
    }
}

fn enabled(u: &Ui, c: Ctl) -> bool {
    match u.status {
        Status::Starting | Status::Saving => c == Ctl::Folder,
        Status::Recording => matches!(c, Ctl::Rec | Ctl::Folder),
        _ => true,
    }
}

fn hit_main(u: &Ui, x: f32, y: f32) -> Option<Hit> {
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

fn keycap(p: &Painter, r: R, fill: C, edge: Option<C>, fg: C) {
    p.fill(r, 5.0, fill);
    if let Some(e) = edge {
        p.stroke(r, 5.0, e, 1.0);
    }
    p.text("F9", r, Font::strong(11.0).at(Align::Center), fg);
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
    if u.hotkey_ok {
        keycap(p, R::new(r.right() - 42.0, cy - 10.0, 30.0, 20.0), cap, None, fg);
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
    p.text("LightCaptureX", R::new(PAD, 18.0, 300.0, 28.0), Font::strong(15.0), tok::TEXT);
    let hr = R::new(W - PAD - 320.0, 18.0, 320.0, 28.0);
    let small = Font::text(12.0);
    if u.hotkey_ok {
        let t = "で録画を開始・停止";
        let tw = p.measure(t, small);
        p.text(t, R::new(hr.right() - tw - 2.0, hr.y, tw + 2.0, hr.h), small, tok::TEXT_3);
        keycap(p, R::new(hr.right() - tw - 40.0, hr.y + 4.0, 30.0, 20.0), tok::CTRL, Some(tok::EDGE), tok::TEXT_2);
    } else {
        let t = "F9 はほかのアプリが使用中";
        let tw = p.measure(t, small);
        p.text(t, R::new(hr.right() - tw - 2.0, hr.y, tw + 2.0, hr.h), small, tok::TEXT_2);
        p.glyph(glyph::WARNING, R::new(hr.right() - tw - 24.0, hr.y, 16.0, hr.h), 12.0, tok::TEXT_2);
    }

    draw_source(u, p);

    // 映像
    card(p, VIDEO, Some("映像"));
    let on = enabled(u, Ctl::Quality);
    for (i, s) in ["画質", "フレームレート", "解像度", "コーデック", "GPU", "カーソル"].iter().enumerate() {
        label(p, VIDEO, i, LABEL_V, s, on);
    }
    for c in [Ctl::Quality, Ctl::Fps, Ctl::Res, Ctl::Codec] {
        draw_seg(u, p, c);
    }
    let gpu = if u.gpu == 0 { "自動".to_string() } else { u.gpus.get(u.gpu - 1).cloned().unwrap_or_default() };
    draw_text_select(u, p, Ctl::Gpu, &gpu);
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

    // 下の行: 録画中はエンコーダ、終わったらファイル名かエラーの詳細
    let (text, col) = match &u.status {
        Status::Recording => (u.info.clone(), tok::TEXT_3),
        Status::Saved(name) => (name.clone(), tok::TEXT_2),
        Status::Failed(_, detail) => (detail.clone(), tok::TEXT_2),
        _ => (format!("保存先  {}", u.out_dir), tok::TEXT_3),
    };
    p.text(&text, R::new(FOOTER.x + 4.0, FOOTER.y, FOOTER.w - 160.0, FOOTER.h), small, col);
    draw_folder(u, p);
}

// ---- 一覧（ドロップダウン）----

const ITEM: f32 = 34.0;
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
}

impl Pop {
    fn vis(&self) -> usize {
        self.items.len().clamp(1, VIS)
    }
    fn height(&self) -> f32 {
        self.vis() as f32 * ITEM + 8.0
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
        let ir = R::new(r.x + 4.0, r.y + 4.0 + k as f32 * ITEM, r.w - 8.0 - bar, ITEM);
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
        p.fill(R::new(r.right() - 8.0, ty, 3.0, th), 1.5, tok::TEXT_3);
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
            paint(hwnd, &POP_P, tok::CTRL, |p| {
                POP.with(|s| {
                    if let Some(pp) = s.borrow().as_ref() {
                        draw_pop(pp, p, R::new(0.0, 0.0, pp.w, pp.height()));
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
            let inside = POP.with(|s| s.borrow().as_ref().is_some_and(|pp| x >= 0.0 && x < pp.w && y >= 0.0 && y < pp.height()));
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
    let mut pop = Pop { hwnd: HWND::default(), dpi, w: a.w, items, sel, hover: Some(sel), top: 0, done: None };
    pop.show(sel);
    let s = dpi as f32 / 96.0;
    let (pw, ph) = ((a.w * s).round() as i32, (pop.height() * s).round() as i32);
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
            "source={src}\nquality={}\nfps={}\nres={}\nmic={mic}\nsysmode={}\napps={}\ncursor={}\ncodec={codec}\ngpu={gpu}\n",
            u.quality,
            u.fps,
            u.res,
            u.sys,
            u.apps.join("|"),
            u.cursor as u8
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

fn output_dir() -> PathBuf {
    let videos = unsafe {
        SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None).ok().and_then(|p| {
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
            unsafe { SetTimer(Some(hwnd), TIMER, 500, None) };
        }
        Err(e) => {
            log(&format!("start: {e}"));
            set_status(Status::Failed("開始できませんでした".into(), e.message()));
        }
    }
    invalidate_all();
}

fn stop_recording() {
    let Some((r, _, path)) = UI.with(|u| u.borrow_mut().rec.take()) else { return };
    let hwnd = UI.with(|u| u.borrow().hwnd);
    unsafe { let _ = KillTimer(Some(hwnd), TIMER); }
    set_status(Status::Saving);
    repaint_now();
    let res = r.stop();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match res {
        Ok(()) => set_status(Status::Saved(name)),
        Err(e) => {
            log(&format!("stop: {e}"));
            set_status(Status::Failed("保存中にエラーが発生しました".into(), e.message()));
        }
    }
    UI.with(|u| u.borrow_mut().last_file = Some(path));
    invalidate_all();
}

fn tick() {
    UI.with(|u| {
        let mut u = u.borrow_mut();
        let Some((_, t, path)) = &u.rec else { return };
        let (s, size) = (t.elapsed().as_secs(), std::fs::metadata(path).map(|m| m.len()).unwrap_or(0));
        u.elapsed = s;
        u.size_mb = size as f64 / 1_048_576.0;
        u.blink = !u.blink;
    });
    invalidate_main();
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
            _ => {}
        }
    });
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
    let (focus, hotkey_ok) = UI.with(|u| (u.borrow().focus, u.borrow().hotkey_ok));
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
    if vk == VK_F9 && !hotkey_ok {
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
        } else if matches!(c, Ctl::Source | Ctl::Gpu | Ctl::Mic) && (vk == VK_DOWN || vk == VK_UP) {
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
        u.out_dir = output_dir().display().to_string();
    });
    refresh_mics(cfg(&c, "mic").filter(|s| !s.is_empty()).map(String::from));
    if !apps.is_empty() {
        let dpi = UI.with(|u| u.borrow().dpi);
        remember_icons(&apps::gather(&apps, icon_px(dpi)));
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let dpi = || UI.with(|u| u.borrow().dpi);
    match msg {
        WM_CREATE => {
            let d = unsafe { GetDpiForWindow(hwnd) }.max(96);
            let ok = unsafe { RegisterHotKey(Some(hwnd), HOTKEY, MOD_NOREPEAT, VK_F9.0 as u32).is_ok() };
            UI.with(|u| {
                let mut u = u.borrow_mut();
                u.hwnd = hwnd;
                u.dpi = d;
                u.hotkey_ok = ok;
            });
            init_values();
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
        WM_KEYDOWN => {
            on_key(VIRTUAL_KEY(wp.0 as u16));
            LRESULT(0)
        }
        WM_ACTIVATEAPP if wp.0 == 0 => {
            pop_finish(None);
            LRESULT(0)
        }
        WM_HOTKEY if wp.0 as i32 == HOTKEY => {
            // 一覧を開いたまま録画を始めると、古い一覧の番号が設定に入るので先に閉じる
            pop_finish(None);
            toggle();
            LRESULT(0)
        }
        WM_TIMER => {
            tick();
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
                u.size_mb = 312.4;
                u.blink = true;
                u.info = "（エンコーダの表示）".into();
                u.hover = Some(Hit { ctl: Ctl::Rec, idx: 0 });
            }),
            None,
        ),
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
                let mut pp = Pop { hwnd: HWND::default(), dpi, w: a.w, items, sel, hover: Some(sel), top: 0, done: None };
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
                    let r = R::new(a.x, a.bottom() + 4.0, pp.w, pp.height());
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
