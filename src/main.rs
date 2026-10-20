#![windows_subsystem = "windows"]

mod apps;
mod audio;
mod capture;
mod convert;
mod d3d;
mod encoder;
mod gfx;
mod gui;
mod recorder;
mod recs;
mod stats;
mod sysmon;
mod tray;

use std::io::Write;
use windows::Win32::Foundation::HWND;
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

/// 標準エラーと %TEMP%\lightcapturex.log に書く
pub fn log(msg: &str) {
    eprintln!("{msg}");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("lightcapturex.log")) {
        let _ = writeln!(f, "{msg}");
    }
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            log(&format!("MFStartup: {e}"));
            return;
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        gui::run();
    } else if args[0] == "--render-ui" && args.len() > 1 {
        // 画面を PNG に書き出すだけ（ウィンドウは出さない）
        gui::render_ui(&args[1]);
    } else {
        cli(&args);
    }
}

fn cpu_time() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut a, mut b, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { let _ = GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u); }
    let f = |t: FILETIME| ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) as f64 / 1e7;
    f(k) + f(u)
}

/// 動作確認用のヘッドレスモード
/// 例: lightcapturex --source 0 --seconds 5 --out C:\tmp\a.mp4 [--fps 60] [--quality 2] [--height 1080] [--mic N] [--no-sys] [--apps a.exe,b.exe] [--codec h264|hevc|av1] [--gpu 名前]
fn cli(args: &[String]) {
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let sources = capture::list_sources(HWND::default());
    let mics = audio::list_mics();
    if args.iter().any(|a| a == "--list") {
        for (i, s) in sources.iter().enumerate() {
            println!("source {i}: {}", s.name);
        }
        for (i, (_, n)) in mics.iter().enumerate() {
            println!("mic {i}: {n}");
        }
        for n in d3d::adapter_names() {
            println!("gpu: {n}");
        }
        for n in audio::audio_apps() {
            println!("audio app: {n}");
        }
        let codecs: Vec<_> = encoder::available_codecs().iter().map(|c| c.label()).collect();
        println!("codecs: {}", codecs.join(", "));
        return;
    }
    let idx: usize = get("--source").and_then(|v| v.parse().ok()).unwrap_or(0);
    let Some(src) = sources.get(idx) else {
        log("source not found");
        return;
    };
    let settings = recorder::Settings {
        source: src.source,
        fps: get("--fps").and_then(|v| v.parse().ok()).unwrap_or(60),
        quality: get("--quality").and_then(|v| v.parse().ok()).unwrap_or(2),
        max_height: get("--height").and_then(|v| v.parse().ok()).unwrap_or(0),
        cursor: true,
        system_audio: !args.iter().any(|a| a == "--no-sys"),
        apps: get("--apps").map(|v| v.split(',').map(String::from).collect()).unwrap_or_default(),
        mic: get("--mic").and_then(|v| v.parse::<usize>().ok()).and_then(|i| mics.get(i)).map(|m| m.0.clone()),
        codec: match get("--codec").as_deref() {
            Some("hevc") => encoder::Codec::Hevc,
            Some("av1") => encoder::Codec::Av1,
            _ => encoder::Codec::H264,
        },
        gpu: get("--gpu"),
        path: get("--out").unwrap_or_else(|| "capture.mp4".into()),
    };
    let secs: f64 = get("--seconds").and_then(|v| v.parse().ok()).unwrap_or(5.0);
    log(&format!("recording '{}' -> {}", src.name, settings.path));
    match recorder::start(settings) {
        Ok(r) => {
            let c0 = cpu_time();
            std::thread::sleep(std::time::Duration::from_secs_f64(secs));
            // 録画中のプロセス全体の CPU 使用量（1 コアを 100% とする）
            log(&format!("cpu: {:.2}% of one core", (cpu_time() - c0) / secs * 100.0));
            match r.stop() {
                Ok(()) => log("done"),
                Err(e) => log(&format!("stop error: {e}")),
            }
        }
        Err(e) => log(&format!("start error: {e}")),
    }
}
