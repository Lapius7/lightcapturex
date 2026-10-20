//! 自動アップデート。GitHub の最新リリースを調べ、新しければ exe を差し替えて再起動する。
//! 通信は Windows 標準の curl.exe に任せる（TLS を自前で持たず、サイズを増やさないため）。
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const API: &str = "https://api.github.com/repos/Lapius7/lightcapturex/releases/latest";
/// 状態が変わったとき（画面を描き直す）
pub const WM_UPDATE: u32 = WM_APP + 3;
/// 差し替えが済んだので終了してよい
pub const WM_UPDATE_QUIT: u32 = WM_APP + 4;

#[derive(Clone, Default, PartialEq)]
pub enum State {
    #[default]
    Idle,
    Checking,
    Latest,
    /// 新しい版の番号
    Available(String),
    Downloading,
    Failed(String),
}

static ST: Mutex<State> = Mutex::new(State::Idle);
static URL: Mutex<String> = Mutex::new(String::new());

pub fn state() -> State {
    ST.lock().map(|s| s.clone()).unwrap_or_default()
}

fn set(s: State, hwnd: isize) {
    if let Ok(mut g) = ST.lock() {
        *g = s;
    }
    post(hwnd, WM_UPDATE);
}

fn post(hwnd: isize, msg: u32) {
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as *mut core::ffi::c_void)), msg, WPARAM(0), LPARAM(0));
    }
}

fn curl(args: &[&str]) -> Option<Vec<u8>> {
    let o = Command::new("curl.exe").args(["-sfL", "-H", "User-Agent: lightcapturex"]).args(args).creation_flags(0x0800_0000).output().ok()?;
    o.status.success().then_some(o.stdout)
}

/// `"key":"value"` の value を取り出す
fn json_str<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let i = s.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = s[i..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    Some(&rest[..rest.find('"')?])
}

fn parse(v: &str) -> Vec<u32> {
    v.trim_start_matches('v').split('.').map(|x| x.parse().unwrap_or(0)).collect()
}

/// 最新リリースを調べる（別スレッド）
pub fn check(hwnd: HWND) {
    if matches!(state(), State::Checking | State::Downloading) {
        return;
    }
    let h = hwnd.0 as isize;
    set(State::Checking, h);
    std::thread::spawn(move || {
        let Some(body) = curl(&["--max-time", "15", "-H", "Accept: application/vnd.github+json", API]) else {
            return set(State::Failed("更新を確認できませんでした".into()), h);
        };
        let body = String::from_utf8_lossy(&body);
        let tag = json_str(&body, "tag_name").unwrap_or("");
        // assets の中の exe の URL（ソースの zip などは対象外）
        let url = body.match_indices("\"browser_download_url\"").filter_map(|(i, _)| json_str(&body[i..], "browser_download_url")).find(|u| u.ends_with(".exe"));
        match url {
            Some(u) if !tag.is_empty() && parse(tag) > parse(VERSION) => {
                if let Ok(mut g) = URL.lock() {
                    *g = u.to_string();
                }
                set(State::Available(tag.trim_start_matches('v').to_string()), h);
            }
            _ => set(State::Latest, h),
        }
    });
}

/// 新しい exe を落として差し替え、再起動する（別スレッド）
pub fn apply(hwnd: HWND) {
    let h = hwnd.0 as isize;
    let url = URL.lock().map(|u| u.clone()).unwrap_or_default();
    if url.is_empty() || matches!(state(), State::Downloading) {
        return;
    }
    set(State::Downloading, h);
    std::thread::spawn(move || {
        let fail = |m: &str| set(State::Failed(m.to_string()), h);
        let Ok(exe) = std::env::current_exe() else { return fail("実行ファイルの場所が分かりません") };
        let (new, old) = (exe.with_extension("exe.new"), exe.with_extension("exe.old"));
        let ok = curl(&["--max-time", "300", "-o", &new.to_string_lossy(), &url]).is_some();
        let good = ok && std::fs::metadata(&new).map(|m| m.len() > 100_000).unwrap_or(false) && std::fs::read(&new).map(|b| b.starts_with(b"MZ")).unwrap_or(false);
        if !good {
            let _ = std::fs::remove_file(&new);
            return fail("ダウンロードに失敗しました");
        }
        // 動作中の exe も名前は変えられるので、退避してから新しいものを置く
        let _ = std::fs::remove_file(&old);
        if std::fs::rename(&exe, &old).is_err() {
            let _ = std::fs::remove_file(&new);
            return fail("この場所では置き換えられません");
        }
        if std::fs::rename(&new, &exe).is_err() {
            let _ = std::fs::rename(&old, &exe);
            return fail("置き換えに失敗しました");
        }
        if Command::new(&exe).arg("--updated").spawn().is_err() {
            return fail("再起動できませんでした");
        }
        post(h, WM_UPDATE_QUIT);
    });
}

/// 前回の更新で退避した古い exe を消す
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }
}
