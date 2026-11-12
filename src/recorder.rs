//! 録画全体の制御。画面と音の取り込みは 1 本（Pipeline）にまとめ、
//! そこから「録画ファイル」と「リプレイバッファ」の 2 つの出力へ書き込む。
//! 時刻はどちらも Pipeline の開始時刻を 0 とする 1 本の時間軸で数える（映像はフレーム番号、音声はサンプル位置）。
use crate::capture::{Capture, Source};
use crate::convert::Converter;
use crate::encoder::{audio_pos, frame_time, Codec, Encoder};
use crate::replay::{Replay, Saver};
use crate::{audio, d3d, log};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{Result, HSTRING};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIAdapter1;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::*;

#[derive(Clone, PartialEq, Debug)]
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
    /// 目標ビットレート（kbps）。0 なら画質から決める
    pub bitrate_kbps: u32,
}

impl Settings {
    pub fn audio(&self) -> bool {
        self.system_audio || self.mic.is_some()
    }
}

/// スレッド間で共有するための D3D デバイス（マルチスレッド保護を有効にして作っている）
#[derive(Clone)]
pub struct Dev(pub ID3D11Device);
unsafe impl Send for Dev {}
unsafe impl Sync for Dev {}

/// エンコーダの設定（録画ファイルとリプレイで共通）
#[derive(Clone, Copy, Debug)]
pub struct EncParams {
    pub codec: Codec,
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    pub bitrate: u32,
}

impl EncParams {
    pub fn encoder(&self, path: &str, dev: &Dev, audio: bool, replay: bool) -> Result<Encoder> {
        Encoder::new(path, &dev.0, self.codec, self.w, self.h, self.fps, self.bitrate, audio, replay)
    }
}

/// 録画ファイルへの出力。v0 番のフレームを時刻 0 として書く
struct FileOut {
    enc: Encoder,
    v0: u64,
    a0: u64,
    /// 止めるときに決める終端（このフレーム番号・サンプル位置の手前まで書く）
    end_v: Option<u64>,
    end_a: Option<u64>,
}

pub struct Shared {
    pub dev: Dev,
    pub params: EncParams,
    pub audio: bool,
    /// 次に書くフレーム番号（これより前は書き終えている）
    pub vidx: AtomicU64,
    /// 書き終えた音声のサンプル位置
    pub apos: AtomicU64,
    /// 映像スレッドが終わった（エラーで止まった場合も含む）
    pub dead: AtomicBool,
    pub error: Mutex<Option<String>>,
    file: Mutex<Option<FileOut>>,
    replay: Mutex<Option<Arc<Replay>>>,
}

impl Shared {
    fn fail(&self, msg: String) {
        log(&format!("pipeline error: {msg}"));
        self.error.lock().unwrap().get_or_insert(msg);
    }

    /// cond が成り立つか、映像スレッドが止まるか、limit が過ぎるまで待つ
    pub fn wait(&self, limit: Duration, cond: impl Fn() -> bool) -> bool {
        let t = Instant::now();
        while !cond() {
            if self.dead.load(SeqCst) || t.elapsed() > limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }
}

pub struct Pipeline {
    stop: Arc<AtomicBool>,
    video: Option<JoinHandle<()>>,
    audio: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    pub settings: Settings,
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
                std::thread::sleep(Duration::from_nanos((remain as i128 * 1_000_000_000 / freq as i128) as u64));
            }
        }
    }
}
impl Drop for Sleeper {
    fn drop(&mut self) {
        unsafe { let _ = CloseHandle(self.0); }
    }
}

/// MMCSS（マルチメディア用のスレッドスケジューリング）にスレッドを登録する。
/// ゲームが CPU を使い切っていても、短い処理を時間どおりに回してもらえる
pub struct MmThread(HANDLE);
impl MmThread {
    pub fn new(task: &str) -> Option<Self> {
        let mut idx = 0u32;
        match unsafe { AvSetMmThreadCharacteristicsW(&HSTRING::from(task), &mut idx) } {
            Ok(h) => Some(Self(h)),
            Err(e) => {
                log(&format!("mmcss '{task}': {e}"));
                None
            }
        }
    }
}
impl Drop for MmThread {
    fn drop(&mut self) {
        unsafe { let _ = AvRevertMmThreadCharacteristics(self.0); }
    }
}

/// COM（MTA）を初期化した別スレッドで f を実行する。
/// Media Foundation のオブジェクトを UI スレッド（STA）で作らないようにするため
pub fn mta<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|sc| {
        sc.spawn(|| {
            unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
            f()
        })
        .join()
        .unwrap()
    })
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

type Ready = mpsc::Sender<Result<(Arc<Shared>, i64, String)>>;

/// 取り込みを始める。file を渡すとすぐ録画ファイルにも書き、replay を渡すとリプレイバッファ（秒数）も持つ
pub fn start(s: Settings, file: Option<String>, replay: Option<u32>) -> Result<Pipeline> {
    crate::stats::reset();
    d3d::raise_gpu_priority();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let (stop_v, sv) = (stop.clone(), s.clone());
    let video = std::thread::spawn(move || video_thread(sv, file, replay, stop_v, tx));
    let (shared, t0, info) = match rx.recv() {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            let _ = video.join();
            return Err(e);
        }
        Err(_) => return Err(windows::core::Error::empty()),
    };
    let audio = if s.audio() {
        let sh = shared.clone();
        let stop_a = stop.clone();
        let (sys, apps, mic) = (s.system_audio, s.apps.clone(), s.mic.clone());
        Some(std::thread::spawn(move || audio::run(move |pcm, pos| write_audio(&sh, pcm, pos), t0, sys, apps, mic, stop_a)))
    } else {
        None
    };
    Ok(Pipeline { stop, video: Some(video), audio, shared, settings: s, info })
}

fn write_audio(sh: &Shared, pcm: &[i16], pos: u64) {
    let n = (pcm.len() / crate::encoder::AUDIO_CH as usize) as u64;
    if let Some(r) = sh.replay.lock().unwrap().clone() {
        r.push_pcm(pcm, pos);
    }
    if let Some(f) = sh.file.lock().unwrap().as_ref() {
        // 録画ファイルの範囲 [a0, end_a) に入る部分だけ書く
        let from = pos.max(f.a0);
        let to = (pos + n).min(f.end_a.unwrap_or(u64::MAX));
        if to > from {
            let ch = crate::encoder::AUDIO_CH as usize;
            let part = &pcm[(from - pos) as usize * ch..(to - pos) as usize * ch];
            if let Err(e) = f.enc.write_audio(part, from - f.a0) {
                log(&format!("audio write: {e}"));
            }
        }
    }
    sh.apos.store(pos + n, SeqCst);
}

impl Pipeline {
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn recording(&self) -> bool {
        self.shared.file.lock().unwrap().is_some()
    }

    /// 録画ファイルへの書き込みを始める（次に取り込むフレームが時刻 0 になる）
    pub fn start_file(&self, path: &str) -> Result<()> {
        let sh = &self.shared;
        let enc = mta(|| sh.params.encoder(path, &sh.dev, sh.audio, false))?;
        log(&format!("file: {path} ({})", enc.name));
        let mut f = sh.file.lock().unwrap();
        let v0 = sh.vidx.load(SeqCst);
        *f = Some(FileOut { enc, v0, a0: audio_pos(frame_time(v0, sh.params.fps)), end_v: None, end_a: None });
        Ok(())
    }

    /// 録画ファイルを閉じる。いまの時刻までの映像と音声を書き切ってから Finalize する
    pub fn stop_file(&self) -> Result<()> {
        let sh = &self.shared;
        let (end_v, end_a) = {
            let mut f = sh.file.lock().unwrap();
            let Some(fo) = f.as_mut() else { return Ok(()) };
            let end_v = sh.vidx.load(SeqCst).max(fo.v0 + 1);
            let end_a = audio_pos(frame_time(end_v, sh.params.fps));
            fo.end_v = Some(end_v);
            fo.end_a = Some(end_a);
            (end_v, end_a)
        };
        sh.wait(Duration::from_secs(2), || sh.vidx.load(SeqCst) >= end_v);
        if sh.audio && self.audio.is_some() {
            sh.wait(Duration::from_secs(1), || sh.apos.load(SeqCst) >= end_a);
        }
        let fo = sh.file.lock().unwrap().take();
        match fo {
            Some(fo) => fo.enc.finish(),
            None => Ok(()),
        }
    }

    /// リプレイバッファの秒数（None なら使っていない）
    pub fn replay_secs(&self) -> Option<u32> {
        self.shared.replay.lock().unwrap().as_ref().map(|r| r.secs())
    }

    /// リプレイバッファを付け外しする。秒数だけが変わる場合は溜まった分を保ったまま変える
    pub fn set_replay(&self, secs: Option<u32>) -> Result<()> {
        let sh = &self.shared;
        let cur = sh.replay.lock().unwrap().clone();
        match (cur, secs) {
            (Some(r), Some(n)) => r.set_secs(n),
            (None, Some(n)) => {
                let r = mta(|| Replay::new(sh.dev.clone(), sh.params, n, sh.audio))?;
                *sh.replay.lock().unwrap() = Some(r);
            }
            (_, None) => *sh.replay.lock().unwrap() = None,
        }
        Ok(())
    }

    /// 溜まっているリプレイの秒数
    pub fn replay_buffered(&self) -> f64 {
        let r = self.shared.replay.lock().unwrap().clone();
        r.map(|r| r.buffered(self.shared.vidx.load(SeqCst))).unwrap_or(0.0)
    }

    /// リプレイを保存する処理。重いので別スレッドで Saver::save を呼ぶ
    pub fn replay_saver(&self) -> Option<Saver> {
        let r = self.shared.replay.lock().unwrap().clone()?;
        Some(Saver { replay: r, shared: self.shared.clone() })
    }

    pub fn stop(mut self) -> Result<()> {
        self.stop.store(true, SeqCst);
        if let Some(h) = self.video.take() {
            let _ = h.join();
        }
        if let Some(h) = self.audio.take() {
            let _ = h.join();
        }
        *self.shared.replay.lock().unwrap() = None;
        // 途中で止まっても MP4 として再生できるよう、必ず Finalize する
        let fr = match self.shared.file.lock().unwrap().take() {
            Some(fo) => fo.enc.finish(),
            None => Ok(()),
        };
        match self.error() {
            Some(e) => Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e)),
            None => fr,
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, SeqCst);
    }
}

fn video_thread(s: Settings, file: Option<String>, replay: Option<u32>, stop: Arc<AtomicBool>, ready: Ready) {
    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
    video_loop(&s, file, replay, &stop, ready);
    unsafe { CoUninitialize() };
}

struct Setup {
    cap: Capture,
    conv: Converter,
    shared: Shared,
    first: Option<ID3D11Texture2D>,
    info: String,
}

fn setup_on(s: &Settings, file: Option<&str>, replay: Option<u32>, adapter: Option<&IDXGIAdapter1>) -> Result<Setup> {
    let (device, ctx) = d3d::create_device(adapter)?;
    let mut cap = Capture::new(&device, s.source, s.cursor, s.fps)?;
    let (sw, sh) = cap.size();
    let (ow, oh) = output_size(sw, sh, s.max_height);
    let mut conv = Converter::new(&device, &ctx, sw, sh, ow, oh, s.fps)?;
    let br = if s.bitrate_kbps > 0 { s.bitrate_kbps * 1000 } else { bitrate(ow, oh, s.fps, s.quality, s.codec) };
    let params = EncParams { codec: s.codec, w: ow, h: oh, fps: s.fps, bitrate: br };
    let dev = Dev(device.clone());
    let fout = match file {
        Some(p) => Some(params.encoder(p, &dev, s.audio(), false)?),
        None => None,
    };
    let rp = match replay {
        Some(n) => Some(Replay::new(dev.clone(), params, n, s.audio())?),
        None => None,
    };
    // 最初のフレームが届くまで最大 1 秒待つ（届いた時点を時刻 0 とする）
    let freq = qpf();
    let limit = qpc() + freq;
    let mut first = None;
    while first.is_none() && qpc() < limit {
        cap.latest(|t, c| first = conv.process(t, c).ok());
        if first.is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    // 書き込めないエンコーダ（その GPU が非対応のコーデックなど）はここで失敗させ、次の GPU を試す
    if let Some(t) = &first {
        if let Some(e) = &fout {
            e.write_video(t, 0)?;
        }
        if let Some(r) = &rp {
            r.write(t, 0)?;
        }
    }
    let gpu = d3d::adapter_name(&device);
    let name = fout.as_ref().map(|e| e.name.clone()).or_else(|| rp.as_ref().map(|r| r.encoder_name())).unwrap_or_default();
    log(&format!(
        "video: {sw}x{sh} -> {ow}x{oh} @{}fps {} {}kbps, gpu: {gpu}, encoder: {name}, first frame: {}, replay: {replay:?}",
        s.fps,
        s.codec.label(),
        br / 1000,
        first.is_some()
    ));
    let written = first.is_some() as u64;
    let shared = Shared {
        dev,
        params,
        audio: s.audio(),
        vidx: AtomicU64::new(written),
        apos: AtomicU64::new(0),
        dead: AtomicBool::new(false),
        error: Mutex::new(None),
        file: Mutex::new(fout.map(|enc| FileOut { enc, v0: 0, a0: 0, end_v: None, end_a: None })),
        replay: Mutex::new(rp),
    };
    Ok(Setup { cap, conv, shared, first, info: format!("{gpu} / {name}") })
}

/// i 番のフレームを両方の出力へ書く
fn write_frame(sh: &Shared, tex: &ID3D11Texture2D, i: u64) -> Result<()> {
    let rp = sh.replay.lock().unwrap().clone();
    if let Some(r) = rp {
        r.write(tex, i)?;
    }
    let f = sh.file.lock().unwrap();
    if let Some(fo) = f.as_ref() {
        if i >= fo.v0 && fo.end_v.is_none_or(|e| i < e) {
            fo.enc.write_video(tex, i - fo.v0)?;
        }
    }
    // ファイルのロック中に進めるので、start_file の v0 と食い違わない
    sh.vidx.store(i + 1, SeqCst);
    Ok(())
}

fn video_loop(s: &Settings, file: Option<String>, replay: Option<u32>, stop: &AtomicBool, ready: Ready) {
    // 指定 GPU → 録画対象のモニターを担当する GPU → 既定 の順に試す
    let setup = || -> Result<Setup> {
        let mut last_err = windows::core::Error::empty();
        for adapter in d3d::candidates(s.gpu.as_deref(), s.source.monitor()) {
            match setup_on(s, file.as_deref(), replay, adapter.as_ref()) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    log(&format!("setup failed on {:?}: {e}", adapter.is_some()));
                    if let Some(p) = &file {
                        let _ = std::fs::remove_file(p);
                    }
                    last_err = e;
                }
            }
        }
        Err(last_err)
    };
    let Setup { mut cap, mut conv, shared, first: mut last, info } = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let shared = Arc::new(shared);
    let freq = qpf();
    let t0 = qpc();
    let _ = ready.send(Ok((shared.clone(), t0, info)));

    // MMCSS に登録できなければ、通常より少し高い優先度にする（HIGHEST はゲームと CPU を食い合うので避ける）
    let mm = MmThread::new("Capture");
    if mm.is_none() {
        unsafe { let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL); }
    }
    let sleeper = Sleeper::new();
    let fps = s.fps as i64;
    let at = |i: u64| t0 + (i as i128 * freq as i128 / fps as i128) as i64;
    let ms = |q: i64| q as f64 * 1000.0 / freq as f64;
    // 最初のフレームは setup で時刻 0 として書き込み済み
    let mut index = shared.vidx.load(SeqCst);
    let (mut written, mut skipped, mut fresh, mut fails) = (index, 0u64, 0u64, 0u32);
    // 計測（定期的にログへ出す）：書き込みにかかった最長時間と、起床の遅れの最長
    let (mut max_write, mut max_late, mut next_report) = (0i64, 0i64, qpc() + freq * 60);
    sleeper.until(at(index), freq);
    let r = (|| -> std::result::Result<(), String> {
        while !stop.load(SeqCst) {
            let late = qpc() - at(index);
            max_late = max_late.max(late);
            let w0 = qpc();
            if cap.latest(|t, c| match conv.process(t, c) {
                Ok(t) => {
                    last = Some(t);
                    fails = 0;
                }
                Err(e) => {
                    if fails == 0 {
                        log(&format!("convert failed: {e}"));
                    }
                    fails += 1;
                }
            }) {
                crate::stats::FRESH.fetch_add(1, SeqCst);
                fresh += 1;
            }
            if fails >= 60 {
                return Err("画面の変換に続けて失敗しました".into());
            }
            // 新しいフレームが無ければ直前のフレームを繰り返して固定フレームレートを保つ
            if let Some(t) = &last {
                write_frame(&shared, t, index).map_err(|e| format!("書き込みに失敗しました: {e}"))?;
                written += 1;
            }
            // 次のフレーム時刻まで待つ。処理が遅れた分はフレームを飛ばす
            let now_idx = ((qpc() - t0) as i128 * fps as i128 / freq as i128) as u64;
            let next = (index + 1).max(now_idx);
            // 遅れた分は直前のフレームを重ねて書き、時刻に穴を空けない（映像が途切れて見えないように）
            if let Some(t) = &last {
                for i in (index + 1)..next.min(index + 9) {
                    if write_frame(&shared, t, i).is_ok() {
                        written += 1;
                    }
                }
            }
            max_write = max_write.max(qpc() - w0);
            let gap = next - index - 1;
            skipped += gap;
            crate::stats::DROPPED.fetch_add(gap, SeqCst);
            index = next;
            if qpc() >= next_report {
                next_report = qpc() + freq * 60;
                log(&format!("video: {written} written, {skipped} skipped, {fresh} captured, max write {:.1}ms, max late {:.1}ms", ms(max_write), ms(max_late)));
                (max_write, max_late) = (0, 0);
            }
            sleeper.until(at(index), freq);
        }
        Ok(())
    })();
    if let Err(e) = r {
        shared.fail(e);
    }
    shared.dead.store(true, SeqCst);
    drop(mm);
    log(&format!("video: {written} frames written, {skipped} skipped, {fresh} captured, max write {:.1}ms, max late {:.1}ms", ms(max_write), ms(max_late)));
}
