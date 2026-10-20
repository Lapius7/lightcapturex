//! LightCaptureX インストーラー。本体の exe を埋め込み、ユーザー領域に展開する（管理者権限は不要）。
//! `lightcapturex-installer.exe`          … インストール（`/S` で確認なし）
//! `uninstall.exe`（同じ exe のコピー）   … アンインストール（`--uninstall`）
#![windows_subsystem = "windows"]

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use windows::core::{w, Interface, HSTRING, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegDeleteKeyW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::*;

static APP: &[u8] = include_bytes!("../../target/x86_64-pc-windows-gnu/release/lightcapturex.exe");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const UNINSTALL_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\LightCaptureX");
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn msg(text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("LightCaptureX"), flags) }
}

fn install_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Programs").join("LightCaptureX")
}

fn shortcut_path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Microsoft/Windows/Start Menu/Programs/LightCaptureX.lnk")
}

fn reg_sz(name: PCWSTR, value: &str) {
    let v: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let _ = RegSetKeyValueW(HKEY_CURRENT_USER, UNINSTALL_KEY, name, REG_SZ.0, Some(v.as_ptr().cast()), (v.len() * 2) as u32);
    }
}

fn reg_dword(name: PCWSTR, value: u32) {
    unsafe {
        let _ = RegSetKeyValueW(HKEY_CURRENT_USER, UNINSTALL_KEY, name, REG_DWORD.0, Some((&value as *const u32).cast()), 4);
    }
}

fn make_shortcut(target: &Path, lnk: &Path) -> windows::core::Result<()> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(target.as_os_str()))?;
        if let Some(dir) = target.parent() {
            link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
        }
        link.SetDescription(w!("LightCaptureX"))?;
        link.cast::<IPersistFile>()?.Save(&HSTRING::from(lnk.as_os_str()), true)
    }
}

fn install(silent: bool) -> Result<PathBuf, String> {
    let dir = install_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("フォルダーを作れませんでした: {e}"))?;
    let exe = dir.join("lightcapturex.exe");
    // 実行中でも置き換えられるよう、既存の exe は .old に退避する（自動アップデートと同じ方法）
    let old = dir.join("lightcapturex.exe.old");
    if exe.exists() {
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&exe, &old);
    }
    std::fs::write(&exe, APP).map_err(|e| format!("lightcapturex.exe を書き込めませんでした。\nLightCaptureX が起動中なら、タスクトレイのメニューから終了してください。\n({e})"))?;
    let _ = std::fs::remove_file(&old);

    let uninst = dir.join("uninstall.exe");
    if let Ok(me) = std::env::current_exe() {
        let _ = std::fs::remove_file(&uninst);
        let _ = std::fs::copy(me, &uninst);
    }

    let lnk = shortcut_path();
    if let Some(p) = lnk.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = make_shortcut(&exe, &lnk);

    reg_sz(w!("DisplayName"), "LightCaptureX");
    reg_sz(w!("DisplayVersion"), VERSION);
    reg_sz(w!("Publisher"), "Lapius7");
    reg_sz(w!("InstallLocation"), &dir.to_string_lossy());
    reg_sz(w!("DisplayIcon"), &exe.to_string_lossy());
    reg_sz(w!("UninstallString"), &format!("\"{}\" --uninstall", uninst.display()));
    reg_dword(w!("NoModify"), 1);
    reg_dword(w!("NoRepair"), 1);
    reg_dword(w!("EstimatedSize"), (APP.len() / 1024) as u32);
    let _ = silent;
    Ok(exe)
}

fn uninstall(silent: bool) {
    if !silent
        && msg("LightCaptureX を削除しますか？\n（録画したファイルと設定は残ります）", MB_YESNO | MB_ICONQUESTION) != IDYES
    {
        return;
    }
    let dir = install_dir();
    let _ = std::fs::remove_file(shortcut_path());
    unsafe { let _ = RegDeleteKeyW(HKEY_CURRENT_USER, UNINSTALL_KEY); }
    let exe = dir.join("lightcapturex.exe");
    if std::fs::remove_file(&exe).is_err() && exe.exists() {
        msg("LightCaptureX が起動中です。タスクトレイのメニューから終了してから、もう一度実行してください。", MB_OK | MB_ICONWARNING);
        return;
    }
    let _ = std::fs::remove_file(dir.join("lightcapturex.exe.old"));
    // 実行中の自分自身は消せないので、終了後にコマンドで片付ける
    let cmd = format!("ping -n 3 127.0.0.1 >nul & del /f /q \"{0}\\uninstall.exe\" & rmdir \"{0}\"", dir.display());
    let _ = std::process::Command::new("cmd").args(["/c", &cmd]).creation_flags(CREATE_NO_WINDOW).spawn();
    if !silent {
        msg("削除しました。", MB_OK | MB_ICONINFORMATION);
    }
}

fn main() {
    unsafe { let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let silent = args.iter().any(|a| a.eq_ignore_ascii_case("/S") || a == "--silent");
    if args.iter().any(|a| a == "--uninstall") {
        uninstall(silent);
        return;
    }
    if !silent
        && msg(
            &format!("LightCaptureX {VERSION} をインストールします。\n\n場所: {}", install_dir().display()),
            MB_OKCANCEL | MB_ICONINFORMATION,
        ) != IDOK
    {
        return;
    }
    match install(silent) {
        Ok(exe) => {
            if !silent && msg("インストールが完了しました。今すぐ起動しますか？", MB_YESNO | MB_ICONQUESTION) == IDYES {
                let _ = std::process::Command::new(exe).spawn();
            }
        }
        Err(e) => {
            msg(&e, MB_OK | MB_ICONERROR);
            std::process::exit(1);
        }
    }
}
