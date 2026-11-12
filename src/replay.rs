//! リプレイバッファ。直前の N 秒をいつでも MP4 に保存できるよう、常に録り続けておく。
//!
//! - 映像: 約 10 秒ごとの区切りファイルにエンコードし、一時フォルダで古いものから消していく。
//!   保存するときは区切りファイルを無変換でつなぎ直す（再エンコードしないので速く、画質も落ちない）
//! - 音声: 生の PCM をメモリに持ち、保存するときに 1 回だけ AAC にする（区切りの継ぎ目で音が途切れないように）
use crate::encoder::{self, audio_pos, audio_time, frame_time, Encoder, AUDIO_CH};
use crate::log;
use crate::recorder::{Dev, EncParams, Shared};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::core::{Result, HSTRING};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// 区切りファイル 1 本の長さ（秒）
pub const SEG_SECS: u64 = 10;
/// 設定できる最長の秒数
pub const MAX_SECS: u32 = 300;
pub const MIN_SECS: u32 = 5;
const CH: usize = AUDIO_CH as usize;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 区切りファイルの置き場所（プロセスごと）
fn seg_dir() -> PathBuf {
    std::env::temp_dir().join("LightCaptureX").join("replay").join(std::process::id().to_string())
}

/// 前回異常終了したときなどに残った、もう動いていないプロセスの区切りファイルを消す
fn clean_stale() {
    let Some(root) = seg_dir().parent().map(Path::to_path_buf) else { return };
    let Ok(rd) = std::fs::read_dir(&root) else { return };
    let alive: Vec<u32> = crate::audio::processes().into_iter().map(|p| p.0).collect();
    for e in rd.flatten() {
        let pid = e.file_name().to_string_lossy().parse::<u32>().ok();
        if pid.is_none_or(|p| !alive.contains(&p)) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// 最後の参照が消えたらファイルも消す（保存中に使っている区切りは消えない）
struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 書き込み中の区切り。enc を先に破棄してファイルを閉じてから、file が消す
struct Seg {
    enc: Encoder,
    file: Arc<FileGuard>,
    /// 最初に書いたフレーム番号（この区切りの時刻 0）
    v0: Option<u64>,
}

const FINALIZING: u8 = 0;
const OK: u8 = 1;
const FAILED: u8 = 2;

/// 書き終えた区切り。フレーム番号 [v0, end) を含む
#[derive(Clone)]
struct Done {
    file: Arc<FileGuard>,
    v0: u64,
    end: u64,
    state: Arc<AtomicU8>,
}

struct St {
    cur: Option<Seg>,
    done: VecDeque<Done>,
    /// 次に書くフレーム番号
    next_idx: u64,
}

/// PCM の輪。base は先頭のサンプル位置
struct Pcm {
    buf: VecDeque<i16>,
    base: u64,
}

pub struct Replay {
    dev: Dev,
    p: EncParams,
    audio: bool,
    secs: AtomicU32,
    st: Mutex<St>,
    /// 次の区切り用に先に作っておくエンコーダ（作るのに時間がかかり、映像スレッドを止めないため）
    next: Mutex<Option<Seg>>,
    preparing: AtomicBool,
    pcm: Mutex<Pcm>,
    save_lock: Mutex<()>,
    name: String,
}

fn com() {
    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
}

impl Replay {
    pub fn new(dev: Dev, p: EncParams, secs: u32, audio: bool) -> Result<Arc<Self>> {
        clean_stale();
        let _ = std::fs::create_dir_all(seg_dir());
        let first = new_seg(&dev, &p)?;
        let name = first.enc.name.clone();
        Ok(Arc::new(Self {
            dev,
            p,
            audio,
            secs: AtomicU32::new(secs.clamp(MIN_SECS, MAX_SECS)),
            st: Mutex::new(St { cur: Some(first), done: VecDeque::new(), next_idx: 0 }),
            next: Mutex::new(None),
            preparing: AtomicBool::new(false),
            pcm: Mutex::new(Pcm { buf: VecDeque::new(), base: 0 }),
            save_lock: Mutex::new(()),
            name,
        }))
    }

    pub fn encoder_name(&self) -> String {
        self.name.clone()
    }

    pub fn secs(&self) -> u32 {
        self.secs.load(SeqCst)
    }

    pub fn set_secs(&self, n: u32) {
        self.secs.store(n.clamp(MIN_SECS, MAX_SECS), SeqCst);
    }

    fn seg_frames(&self) -> u64 {
        self.p.fps as u64 * SEG_SECS
    }

    /// 溜まっている秒数（設定の秒数を上限とする）
    pub fn buffered(&self, vidx: u64) -> f64 {
        let st = self.st.lock().unwrap();
        let first = st.done.front().map(|d| d.v0).or(st.cur.as_ref().and_then(|c| c.v0));
        let n = first.map(|v| vidx.saturating_sub(v)).unwrap_or(0);
        (n as f64 / self.p.fps as f64).min(self.secs() as f64)
    }

    /// i 番のフレームを書く（映像スレッドから呼ぶ）
    pub fn write(self: &Arc<Self>, tex: &ID3D11Texture2D, i: u64) -> Result<()> {
        let seg_frames = self.seg_frames();
        let fps = self.p.fps as u64;
        let mut st = self.st.lock().unwrap();
        let elapsed = st.cur.as_ref().and_then(|c| c.v0).map(|v0| i.saturating_sub(v0));
        if let Some(n) = elapsed {
            // 切り替えの約 2 秒前に次のエンコーダを用意しはじめる
            if n + 2 * fps >= seg_frames {
                self.prepare();
            }
            // 用意できていれば差し替える。まだなら今の区切りに書き続ける
            if n >= seg_frames {
                let next = self.next.lock().unwrap().take();
                if let Some(next) = next {
                    let old = st.cur.replace(next);
                    let end = st.next_idx;
                    if let Some(old) = old {
                        self.retire(&mut st, old, end, true);
                    }
                }
            }
        }
        if st.cur.is_none() {
            let next = self.next.lock().unwrap().take();
            match next {
                Some(n) => st.cur = Some(n),
                None => {
                    self.prepare();
                    return Ok(());
                }
            }
        }
        let cur = st.cur.as_mut().unwrap();
        let v0 = *cur.v0.get_or_insert(i);
        if let Err(e) = cur.enc.write_video(tex, i - v0) {
            // 最初のフレームから書けないのは設定が合わないエンコーダなので、エラーにして GPU を切り替えさせる
            if i == v0 {
                return Err(e);
            }
            // 途中で壊れた区切りは捨てて作り直す（録画ファイルの方は止めない）
            log(&format!("replay write: {e}"));
            st.cur = None;
            self.prepare();
            return Ok(());
        }
        st.next_idx = i + 1;
        Ok(())
    }

    /// 書き終えた区切りを一覧に移し、Finalize する（background なら別スレッドで）
    fn retire(&self, st: &mut St, old: Seg, end: u64, background: bool) -> Option<(Seg, Arc<AtomicU8>)> {
        let v0 = old.v0?;
        let state = Arc::new(AtomicU8::new(FINALIZING));
        st.done.push_back(Done { file: old.file.clone(), v0, end, state: state.clone() });
        // 古い区切りは、保存に要る分だけ残して消す
        let keep = (self.secs() as u64 * self.p.fps as u64).div_ceil(self.seg_frames()) as usize + 1;
        while st.done.len() > keep {
            st.done.pop_front();
        }
        if background {
            std::thread::spawn(move || {
                com();
                finalize(old, &state);
            });
            None
        } else {
            Some((old, state))
        }
    }

    fn prepare(self: &Arc<Self>) {
        if self.next.lock().unwrap().is_some() || self.preparing.swap(true, SeqCst) {
            return;
        }
        let me = self.clone();
        std::thread::spawn(move || {
            com();
            match new_seg(&me.dev, &me.p) {
                Ok(s) => *me.next.lock().unwrap() = Some(s),
                Err(e) => {
                    log(&format!("replay: next segment: {e}"));
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            me.preparing.store(false, SeqCst);
        });
    }

    /// 音声スレッドから PCM を受け取る（pos は先頭からのサンプル位置）
    pub fn push_pcm(&self, mut pcm: &[i16], pos: u64) {
        if !self.audio {
            return;
        }
        let mut p = self.pcm.lock().unwrap();
        let end = p.base + (p.buf.len() / CH) as u64;
        if p.buf.is_empty() {
            p.base = pos;
        } else if pos > end {
            let gap = ((pos - end) as usize * CH).min(48_000 * CH * 10);
            p.buf.extend(std::iter::repeat_n(0, gap));
        } else if pos < end {
            let skip = (end - pos) as usize * CH;
            if skip >= pcm.len() {
                return;
            }
            pcm = &pcm[skip..];
        }
        p.buf.extend(pcm.iter().copied());
        let cap = (self.secs() as usize + SEG_SECS as usize * 2 + 5) * encoder::AUDIO_RATE as usize * CH;
        if p.buf.len() > cap {
            let extra = p.buf.len() - cap;
            p.buf.drain(..extra);
            p.base += (extra / CH) as u64;
        }
    }

    /// [from, to) の PCM を写す（無い部分は無音）
    fn copy_pcm(&self, from: u64, to: u64) -> Vec<i16> {
        let p = self.pcm.lock().unwrap();
        let mut out = vec![0i16; (to - from) as usize * CH];
        let end = p.base + (p.buf.len() / CH) as u64;
        let (a, b) = (from.max(p.base), to.min(end));
        if b > a {
            let src = (a - p.base) as usize * CH..(b - p.base) as usize * CH;
            let dst = (a - from) as usize * CH;
            for (o, s) in out[dst..].iter_mut().zip(p.buf.range(src)) {
                *o = *s;
            }
        }
        out
    }
}

fn new_seg(dev: &Dev, p: &EncParams) -> Result<Seg> {
    let path = seg_dir().join(format!("seg-{}.mp4", COUNTER.fetch_add(1, SeqCst)));
    let file = Arc::new(FileGuard(path));
    let enc = p.encoder(&file.0.to_string_lossy(), dev, false, true)?;
    Ok(Seg { enc, file, v0: None })
}

fn finalize(old: Seg, state: &AtomicU8) {
    let r = old.enc.finish();
    if let Err(e) = &r {
        log(&format!("replay finalize: {e}"));
    }
    drop(old.enc);
    state.store(if r.is_ok() { OK } else { FAILED }, SeqCst);
}

/// リプレイの保存（別スレッドで呼ぶ）
pub struct Saver {
    pub(crate) replay: Arc<Replay>,
    pub(crate) shared: Arc<Shared>,
}

/// 保存の結果（秒数）
pub struct Saved {
    pub path: PathBuf,
    pub secs: f64,
}

impl Saver {
    pub fn save(self, out: &Path) -> std::result::Result<Saved, String> {
        com();
        let r = &self.replay;
        let sh = &self.shared;
        let _g = r.save_lock.lock().unwrap();
        let fps = r.p.fps;
        let secs = r.secs() as u64;

        // 1. 今の区切りをここで切り、新しい区切りに差し替える
        let ready = r.next.lock().unwrap().take();
        let fresh = match ready {
            Some(s) => s,
            None => new_seg(&r.dev, &r.p).map_err(|e| format!("エンコーダを用意できませんでした: {e}"))?,
        };
        let (v_cut, retired, segs) = {
            let mut st = r.st.lock().unwrap();
            let v_cut = st.next_idx;
            let retired = if st.cur.as_ref().is_some_and(|c| c.v0.is_some()) {
                let old = st.cur.replace(fresh).unwrap();
                r.retire(&mut st, old, v_cut, false)
            } else {
                // 今の区切りにまだ何も書いていない（直前に保存した）ので、そのまま使う
                if st.cur.is_none() {
                    st.cur = Some(fresh);
                } else {
                    let mut n = r.next.lock().unwrap();
                    if n.is_none() {
                        *n = Some(fresh);
                    }
                }
                None
            };
            let from = v_cut.saturating_sub(secs * fps as u64);
            let segs: Vec<Done> = st.done.iter().filter(|d| d.end > from).cloned().collect();
            (v_cut, retired, segs)
        };
        if let Some((old, state)) = retired {
            finalize(old, &state);
        }

        // 2. 区切りの Finalize と、音声が終端まで届くのを待つ
        let end_t = frame_time(v_cut, fps);
        let end_a = audio_pos(end_t);
        for d in &segs {
            sh.wait(Duration::from_secs(10), || d.state.load(SeqCst) != FINALIZING);
        }
        if r.audio {
            sh.wait(Duration::from_millis(1500), || sh.apos.load(SeqCst) >= end_a);
        }
        let segs: Vec<Done> = segs.into_iter().filter(|d| d.state.load(SeqCst) == OK).collect();
        let Some(first) = segs.first() else { return Err("まだ映像が溜まっていません".into()) };

        // 3. 終端の N 秒前以前で最後のキーフレームから始める
        let target = end_t - secs as i64 * 10_000_000;
        let base0 = frame_time(first.v0, fps);
        let start_t = if base0 >= target {
            base0
        } else {
            last_key_before(&first.file.0, target - base0).map_err(|e| format!("区切りを読めませんでした: {e}"))? + base0
        };

        // 4. 無変換でつなぎ、音声を足して書き出す
        let name = out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = out.with_file_name(format!(".{name}.part"));
        let pcm = if r.audio { Some(r.copy_pcm(audio_pos(start_t), end_a)) } else { None };
        let res = remux(&segs, fps, start_t, end_t, pcm.as_deref(), &tmp);
        match res {
            Ok(dur) => {
                std::fs::rename(&tmp, out).map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    format!("保存先に書けませんでした: {e}")
                })?;
                log(&format!("replay saved: {} ({:.1}s, {} segment(s))", out.display(), dur, segs.len()));
                Ok(Saved { path: out.to_path_buf(), secs: dur })
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(format!("保存に失敗しました: {e}"))
            }
        }
    }
}

fn reader(path: &Path) -> Result<IMFSourceReader> {
    unsafe {
        let r = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
        r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        r.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)?;
        Ok(r)
    }
}

/// 次の映像サンプル（終わりなら None）
fn next_sample(r: &IMFSourceReader) -> Result<Option<IMFSample>> {
    loop {
        let mut flags = 0u32;
        let mut s = None;
        unsafe { r.ReadSample(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0, None, Some(&mut flags), None, Some(&mut s))? };
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Ok(None);
        }
        if s.is_some() {
            return Ok(s);
        }
    }
}

fn is_key(s: &IMFSample) -> bool {
    unsafe { s.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0 }
}

/// 区切りファイルの中で、時刻 t 以前の最後のキーフレームの時刻
fn last_key_before(path: &Path, t: i64) -> Result<i64> {
    let r = reader(path)?;
    let mut best = 0;
    while let Some(s) = next_sample(&r)? {
        let st = unsafe { s.GetSampleTime()? };
        if st > t {
            break;
        }
        if is_key(&s) {
            best = st;
        }
    }
    Ok(best)
}

/// 区切りファイルの映像（[start_t, end_t)）と PCM を 1 本の MP4 にする。書けた長さ（秒）を返す
fn remux(segs: &[Done], fps: u32, start_t: i64, end_t: i64, pcm: Option<&[i16]>, out: &Path) -> Result<f64> {
    unsafe {
        let first = reader(&segs[0].file.0)?;
        let vtype = first.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)?;
        drop(first);
        let w = encoder::mp4_writer(&out.to_string_lossy(), None)?;
        let vi = w.AddStream(&vtype)?;
        w.SetInputMediaType(vi, &vtype, None)?;
        let ai = match pcm {
            Some(_) => Some(encoder::add_audio(&w)?),
            None => None,
        };
        w.BeginWriting()?;

        // 映像の時刻に合わせて、音声を 0.5 秒先まで交互に書く
        let chunk = encoder::AUDIO_RATE as usize / 10;
        let mut apos = 0usize;
        let mut feed = |upto: i64| -> Result<()> {
            let (Some(ai), Some(pcm)) = (ai, pcm) else { return Ok(()) };
            let total = pcm.len() / CH;
            let limit = (audio_pos(upto) as usize).min(total);
            while apos < limit {
                let n = chunk.min(total - apos);
                encoder::write_pcm(&w, ai, &pcm[apos * CH..(apos + n) * CH], apos as u64)?;
                apos += n;
            }
            Ok(())
        };
        let mut last_end = 0i64;
        let mut started = false;
        for d in segs {
            let base = frame_time(d.v0, fps);
            if frame_time(d.end, fps) <= start_t {
                continue;
            }
            let r = reader(&d.file.0)?;
            while let Some(s) = next_sample(&r)? {
                let g = base + s.GetSampleTime()?;
                if g >= end_t {
                    break;
                }
                if g < start_t || (!started && !is_key(&s)) {
                    continue;
                }
                started = true;
                let t = g - start_t;
                feed(t + 5_000_000)?;
                s.SetSampleTime(t)?;
                let _ = s.DeleteItem(&MFSampleExtension_Discontinuity);
                let dur = s.GetSampleDuration().unwrap_or(0);
                w.WriteSample(vi, &s)?;
                last_end = t + dur;
            }
        }
        if !started {
            return Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "no video"));
        }
        feed(i64::MAX)?;
        w.Finalize()?;
        Ok(last_end.max(audio_time(apos as u64)) as f64 / 1e7)
    }
}
