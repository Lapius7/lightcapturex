//! 録画全体の制御。映像スレッドと音声スレッドが 1 つの Encoder に書き込む。
use crate::capture::{Capture, Source};
use crate::convert::Converter;
use crate::encoder::{Codec, Encoder};
use crate::{audio, d3d, log};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use windows::core::Result;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIAdapter1;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::*;

#[derive(Clone)]
pub struct Settings {
    pub source: Source,
    pub fps: u32,
    /// 0=低 1=中 2=高 3=最高
    pub quality: u32,
    /// 出力の高さの上限（0 ならソースのまま）
    pub max_height: u32,
    pub cursor: bool,
    pub system_audio: bool,
    /// PC の音を録るアプリの exe 名（空なら PC の音すべて）
    pub apps: Vec<String>,
    /// マイクのデバイス ID（None なら録音しない）
    pub mic: Option<String>,
    pub codec: Codec,
    /// 使う GPU の名前（None なら録画対象のモニターを担当する GPU を自動で選ぶ）
    pub gpu: Option<String>,
    pub path: String,
}

pub struct Recorder {
    stop: Arc<AtomicBool>,
    video: Option<JoinHandle<Result<()>>>,
    audio: Option<JoinHandle<()>>,
    encoder: Arc<Mutex<Encoder>>,
    /// 表示用：「GPU 名 / エンコーダ名」
    pub info: String,
}

pub fn qpc() -> i64 {
    let mut v = 0;
    unsafe { let _ = QueryPerformanceCounter(&mut v); }
    v
}

pub fn qpf() -> i64 {
    let mut v = 0;
    unsafe { let _ = QueryPerformanceFrequency(&mut v); }
    v
}

/// 高精度タイマーで deadline(QPC) まで待つ
pub struct Sleeper(HANDLE);
impl Sleeper {
    pub fn new() -> Self {
        let h = unsafe {
            CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
                .or_else(|_| CreateWaitableTimerExW(None, None, 0, TIMER_ALL_ACCESS.0))
                .unwrap_or_default()
        };
        Self(h)
    }
    pub fn until(&self, deadline: i64, freq: i64) {
        let remain = deadline - qpc();
        if remain <= 0 {
            return;
        }
        let due = -(remain as i128 * 10_000_000 / freq as i128) as i64;
        unsafe {
            if SetWaitableTimer(self.0, &due, 0, None, None, false).is_ok() {
                WaitForSingleObject(self.0, INFINITE);
            } else {
                std::thread::sleep(std::time::Duration::from_nanos((remain as i128 * 1_000_000_000 / freq as i128) as u64));
            }
        }
    }
}
impl Drop for Sleeper {
    fn drop(&mut self) {
        unsafe { let _ = CloseHandle(self.0); }
    }
}

fn even(v: u32) -> u32 {
    (v & !1).max(2)
}

/// 出力解像度：高さ上限とハードウェアエンコーダの上限(4096)に収め、偶数にそろえる
fn output_size(w: u32, h: u32, max_height: u32) -> (u32, u32) {
    let mut s = 1.0f64;
    if max_height > 0 && h > max_height {
        s = max_height as f64 / h as f64;
    }
    s = s.min(4096.0 / w as f64).min(4096.0 / h as f64);
    (even((w as f64 * s).round() as u32), even((h as f64 * s).round() as u32))
}

fn bitrate(w: u32, h: u32, fps: u32, quality: u32, codec: Codec) -> u32 {
    // 1 ピクセル・1 フレームあたりのビット数（H.264 基準）
    let bpp = [0.04, 0.07, 0.10, 0.16][quality.min(3) as usize] * codec.efficiency();
    ((w * h) as f64 * fps as f64 * bpp).clamp(1_000_000.0, 100_000_000.0) as u32
}

pub fn start(s: Settings) -> Result<Recorder> {
    crate::stats::reset();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Result<(Arc<Mutex<Encoder>>, i64, String)>>();
    let stop_v = stop.clone();
    let sv = s.clone();
    let video = std::thread::spawn(move || video_thread(sv, stop_v, tx));
    let (encoder, t0, info) = match rx.recv() {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            let _ = video.join();
            return Err(e);
        }
        Err(_) => return Err(windows::core::Error::empty()),
    };
    let audio = if s.system_audio || s.mic.is_some() {
        let enc = encoder.clone();
        let stop_a = stop.clone();
        let (sys, apps, mic) = (s.system_audio, s.apps.clone(), s.mic.clone());
        Some(std::thread::spawn(move || audio::run(enc, t0, sys, apps, mic, stop_a)))
    } else {
        None
    };
    Ok(Recorder { stop, video: Some(video), audio, encoder, info })
}

impl Recorder {
    pub fn stop(mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        let vr = self.video.take().map(|h| h.join().unwrap_or(Ok(()))).unwrap_or(Ok(()));
        if let Some(h) = self.audio.take() {
            let _ = h.join();
        }
        // 途中で止まっても MP4 として再生できるよう、必ず Finalize する
        let fr = self.encoder.lock().unwrap().finish();
        vr.and(fr)
    }
}

type Ready = mpsc::Sender<Result<(Arc<Mutex<Encoder>>, i64, String)>>;

fn video_thread(s: Settings, stop: Arc<AtomicBool>, ready: Ready) -> Result<()> {
    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
    let r = video_loop(s, stop, ready);
    unsafe { CoUninitialize() };
    r
}

type Setup = (Capture, Converter, Encoder, Option<ID3D11Texture2D>, String);

fn setup_on(s: &Settings, adapter: Option<&IDXGIAdapter1>) -> Result<Setup> {
    let (device, ctx) = d3d::create_device(adapter)?;
    let mut cap = Capture::new(&device, s.source, s.cursor, s.fps)?;
    let (sw, sh) = cap.size();
    let (ow, oh) = output_size(sw, sh, s.max_height);
    let mut conv = Converter::new(&device, &ctx, sw, sh, ow, oh, s.fps)?;
    let br = bitrate(ow, oh, s.fps, s.quality, s.codec);
    let enc = Encoder::new(&s.path, &device, s.codec, ow, oh, s.fps, br, s.system_audio || s.mic.is_some())?;
    // 最初のフレームが届くまで最大 1 秒待つ（届いた時点を録画の時刻 0 とする）
    let freq = qpf();
    let limit = qpc() + freq;
    let mut first = None;
    while first.is_none() && qpc() < limit {
        cap.latest(|t, c| first = conv.process(t, c).ok());
        if first.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    // 書き込めないエンコーダ（その GPU が非対応のコーデックなど）はここで失敗させ、次の GPU を試す
    if let Some(t) = &first {
        enc.write_video(t, 0)?;
    }
    let gpu = d3d::adapter_name(&device);
    log(&format!(
        "video: {sw}x{sh} -> {ow}x{oh} @{}fps {} {}kbps, gpu: {gpu}, encoder: {}, first frame: {}",
        s.fps,
        s.codec.label(),
        br / 1000,
        enc.name,
        first.is_some()
    ));
    let info = format!("{gpu} / {}", enc.name);
    Ok((cap, conv, enc, first, info))
}

fn video_loop(s: Settings, stop: Arc<AtomicBool>, ready: Ready) -> Result<()> {
    // 指定 GPU → 録画対象のモニターを担当する GPU → 既定 の順に試す
    let setup = || -> Result<_> {
        let mut last_err = windows::core::Error::empty();
        for adapter in d3d::candidates(s.gpu.as_deref(), s.source.monitor()) {
            match setup_on(&s, adapter.as_ref()) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    log(&format!("setup failed on {:?}: {e}", adapter.is_some()));
                    let _ = std::fs::remove_file(&s.path);
                    last_err = e;
                }
            }
        }
        Err(last_err)
    };
    let (mut cap, mut conv, enc, mut last, info) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };
    let encoder = Arc::new(Mutex::new(enc));
    let freq = qpf();
    let t0 = qpc();
    let _ = ready.send(Ok((encoder.clone(), t0, info)));

    // CPU を食い合うゲームの邪魔をしないよう HIGHEST は避ける。処理は短く、ほとんど待機している
    unsafe { let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL); }
    let sleeper = Sleeper::new();
    let fps = s.fps as i64;
    // 最初のフレームは setup で時刻 0 として書き込み済み
    let mut index: u64 = last.is_some() as u64;
    let (mut written, mut skipped) = (index, 0u64);
    sleeper.until(t0 + (index as i128 * freq as i128 / fps as i128) as i64, freq);
    while !stop.load(Ordering::SeqCst) {
        if cap.latest(|t, c| match conv.process(t, c) {
            Ok(t) => last = Some(t),
            Err(e) => log(&format!("convert failed: {e}")),
        }) {
            crate::stats::FRESH.fetch_add(1, Ordering::Relaxed);
        }
        // 新しいフレームが無ければ直前のフレームを繰り返して固定フレームレートを保つ
        if let Some(t) = &last {
            encoder.lock().unwrap().write_video(t, index)?;
            written += 1;
        }
        // 次のフレーム時刻まで待つ。処理が遅れた分はフレームを飛ばす
        let now_idx = ((qpc() - t0) as i128 * fps as i128 / freq as i128) as u64;
        let next = (index + 1).max(now_idx);
        skipped += next - index - 1;
        crate::stats::DROPPED.fetch_add(next - index - 1, Ordering::Relaxed);
        index = next;
        sleeper.until(t0 + (index as i128 * freq as i128 / fps as i128) as i64, freq);
    }
    log(&format!("video: {written} frames written, {skipped} skipped"));
    Ok(())
}
