//! WASAPI で PC の音（全体、または選んだアプリだけ）とマイクを取り込み、1 本のステレオトラックにミックスする。
use crate::encoder::{Encoder, AUDIO_CH, AUDIO_RATE};
use crate::log;
use crate::recorder::{qpc, qpf, Sleeper};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::{implement, Interface, Ref, Result, HRESULT, HSTRING};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

const CH: usize = AUDIO_CH as usize;
/// 遅れて届くパケットを待つため、ミックスは実時間より少し後ろを書く
const LATENCY: i64 = AUDIO_RATE as i64 / 10;

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// 使えるマイクの一覧（デバイス ID, 表示名）
pub fn list_mics() -> Vec<(String, String)> {
    let f = || -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        unsafe {
            let col = enumerator()?.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
            for i in 0..col.GetCount()? {
                let dev = col.Item(i)?;
                let id = dev.GetId()?;
                let ids = id.to_string().unwrap_or_default();
                CoTaskMemFree(Some(id.0 as _));
                let name = dev
                    .OpenPropertyStore(STGM_READ)
                    .and_then(|p| p.GetValue(&PKEY_Device_FriendlyName))
                    .map(|v| v.to_string())
                    .unwrap_or_else(|_| ids.clone());
                out.push((ids, name));
            }
        }
        Ok(out)
    };
    f().unwrap_or_default()
}

/// 実行中のプロセス（PID, 親 PID, exe 名）
fn processes() -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..n])));
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// いま音声セッションを持っている（音を出せる状態の）アプリの exe 名
pub fn audio_apps() -> Vec<String> {
    let pids = || -> Result<Vec<u32>> {
        let mut out = Vec::new();
        unsafe {
            let dev = enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
            let list = mgr.GetSessionEnumerator()?;
            for i in 0..list.GetCount()? {
                if let Ok(c) = list.GetSession(i).and_then(|s| s.cast::<IAudioSessionControl2>()) {
                    out.push(c.GetProcessId().unwrap_or(0));
                }
            }
        }
        Ok(out)
    };
    let pids = pids().unwrap_or_default();
    let mut names: Vec<String> = processes()
        .into_iter()
        .filter(|(pid, _, _)| *pid != 0 && *pid != std::process::id() && pids.contains(pid))
        .map(|(_, _, n)| n)
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    names
}

/// 名前が一致するプロセスのうち、親が同じアプリでないもの（プロセスツリーの根）
fn app_roots(names: &[String]) -> Vec<(u32, String)> {
    let ps = processes();
    let is_target = |n: &str| names.iter().any(|x| x.eq_ignore_ascii_case(n));
    ps.iter()
        .filter(|(_, parent, n)| is_target(n) && !ps.iter().any(|(p, _, pn)| p == parent && is_target(pn)))
        .map(|(pid, _, n)| (*pid, n.clone()))
        .collect()
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(HANDLE);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _: Ref<IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        unsafe { SetEvent(self.0) }
    }
}

/// 指定したプロセスとその子プロセスの音だけを取り込むクライアント（Windows 10 2004 以降）
fn process_client(pid: u32) -> Result<IAudioClient> {
    unsafe {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        // blob はスタック上の params を指すので、PropVariantClear が走らないよう drop させない
        let mut pv = std::mem::ManuallyDrop::new(PROPVARIANT::default());
        let v = &mut *pv.Anonymous.Anonymous;
        v.vt = VT_BLOB;
        v.Anonymous.blob.cbSize = size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
        v.Anonymous.blob.pBlobData = &params as *const _ as *mut u8;
        let ev = CreateEventW(None, false, false, None)?;
        let handler: IActivateAudioInterfaceCompletionHandler = Activated(ev).into();
        let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler)?;
        // 完了しないまま閉じると、遅れて呼ばれたハンドラが閉じたハンドルを触るので、成功時だけ閉じる
        if WaitForSingleObject(ev, 3000) != WAIT_OBJECT_0 {
            return Err(E_FAIL.into());
        }
        let _ = CloseHandle(ev);
        let (mut hr, mut iface) = (HRESULT(0), None);
        op.GetActivateResult(&mut hr, &mut iface)?;
        hr.ok()?;
        iface.ok_or_else(|| windows::core::Error::from(E_FAIL))?.cast()
    }
}

struct Input {
    name: String,
    /// アプリの音なら対象の PID（終了したら一覧から外して、再起動を拾えるようにする）
    pid: u32,
    event: HANDLE,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// 出力位置 pos 以降のサンプル（インターリーブ）
    ring: VecDeque<f32>,
}

impl Input {
    /// mic_id が None ならスピーカーのループバック（PC の音すべて）
    fn device(mic_id: Option<&str>) -> Result<Self> {
        unsafe {
            let en = enumerator()?;
            let dev = match mic_id {
                Some(id) => en.GetDevice(&HSTRING::from(id))?,
                None => en.GetDefaultAudioEndpoint(eRender, eConsole)?,
            };
            let name = if mic_id.is_some() { "mic" } else { "system" };
            Self::start(dev.Activate(CLSCTX_ALL, None)?, mic_id.is_none(), name.into(), 0)
        }
    }

    fn app(pid: u32, exe: &str) -> Result<Self> {
        Self::start(process_client(pid)?, true, format!("{exe}({pid})"), pid)
    }

    fn start(client: IAudioClient, loopback: bool, name: String, pid: u32) -> Result<Self> {
        unsafe {
            // 48kHz float ステレオへの変換は WASAPI に任せる
            let wf = WAVEFORMATEX {
                wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
                nChannels: CH as u16,
                nSamplesPerSec: AUDIO_RATE,
                nAvgBytesPerSec: AUDIO_RATE * CH as u32 * 4,
                nBlockAlign: CH as u16 * 4,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            if loopback {
                flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
            }
            // アプリ単位のループバックはイベント方式が必須。取り出しはどれもポーリングで行う
            let event = if pid != 0 { CreateEventW(None, false, false, None)? } else { HANDLE::default() };
            if pid != 0 {
                flags |= AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
            }
            let r = (|| -> Result<IAudioCaptureClient> {
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &wf, None)?;
                if pid != 0 {
                    client.SetEventHandle(event)?;
                }
                let capture: IAudioCaptureClient = client.GetService()?;
                client.Start()?;
                Ok(capture)
            })();
            match r {
                Ok(capture) => Ok(Self { name, pid, event, client, capture, ring: VecDeque::new() }),
                Err(e) => {
                    if !event.is_invalid() {
                        let _ = CloseHandle(event);
                    }
                    Err(e)
                }
            }
        }
    }

    /// 届いているパケットを、QPC タイムスタンプに合わせた位置へ並べる
    fn drain(&mut self, pos: i64, t0_hns: i64) -> Result<()> {
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let mut data = std::ptr::null_mut();
                let (mut n, mut flags, mut qpc_hns) = (0u32, 0u32, 0u64);
                self.capture.GetBuffer(&mut data, &mut n, &mut flags, None, Some(&mut qpc_hns))?;
                let samples: &[f32] = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    &[]
                } else {
                    std::slice::from_raw_parts(data as *const f32, n as usize * CH)
                };
                let end = pos + (self.ring.len() / CH) as i64;
                let at = if qpc_hns > 0 {
                    (qpc_hns as i64 - t0_hns) * AUDIO_RATE as i64 / 10_000_000
                } else {
                    end
                };
                let mut skip = 0i64;
                if at > end + 480 {
                    // 無音区間（ループバックは何も鳴っていないとパケットが来ない）を埋める
                    self.ring.extend(std::iter::repeat_n(0.0, (at - end) as usize * CH));
                } else if at < end - 960 {
                    // 古すぎる・重なっている分は捨てる
                    skip = (end - at).min(n as i64);
                }
                for i in skip as usize..n as usize {
                    for c in 0..CH {
                        self.ring.push_back(samples.get(i * CH + c).copied().unwrap_or(0.0));
                    }
                }
                self.capture.ReleaseBuffer(n)?;
            }
        }
        Ok(())
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            if !self.event.is_invalid() {
                let _ = CloseHandle(self.event);
            }
        }
    }
}

/// 選んだアプリのうち、まだ取り込んでいないものを開く（録画中に起動したアプリも拾う）
fn open_apps(apps: &[String], inputs: &mut Vec<Input>, opened: &mut HashSet<u32>) {
    for (pid, exe) in app_roots(apps) {
        if opened.insert(pid) {
            match Input::app(pid, &exe) {
                Ok(i) => {
                    log(&format!("audio: + {}", i.name));
                    inputs.push(i);
                }
                Err(e) => log(&format!("audio: {exe}({pid}): {e}")),
            }
        }
    }
}

/// system: PC の音を録る / apps: 空でなければ、そのアプリ（exe 名）の音だけを録る
pub fn run(enc: Arc<Mutex<Encoder>>, t0: i64, system: bool, apps: Vec<String>, mic: Option<String>, stop: Arc<AtomicBool>) {
    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
    let freq = qpf();
    let t0_hns = (t0 as i128 * 10_000_000 / freq as i128) as i64;
    let mut inputs = Vec::new();
    let mut opened = HashSet::new();
    let by_app = system && !apps.is_empty();
    if system && !by_app {
        match Input::device(None) {
            Ok(i) => inputs.push(i),
            Err(e) => log(&format!("system audio: {e}")),
        }
    }
    if by_app {
        open_apps(&apps, &mut inputs, &mut opened);
    }
    if let Some(id) = &mic {
        match Input::device(Some(id)) {
            Ok(i) => inputs.push(i),
            Err(e) => log(&format!("mic: {e}")),
        }
    }
    log(&format!("audio: {} input(s)", inputs.len()));
    let mut next_scan = qpc() + freq * 2;

    let sleeper = Sleeper::new();
    let mut pos: i64 = 0;
    let mut mix: Vec<f32> = Vec::new();
    let mut pcm: Vec<i16> = Vec::new();
    loop {
        let stopping = stop.load(Ordering::SeqCst);
        let now = ((qpc() - t0) as i128 * AUDIO_RATE as i128 / freq as i128) as i64;
        // 読めなくなった入力（終了したアプリなど）は外す
        inputs.retain_mut(|i| match i.drain(pos, t0_hns) {
            Ok(()) => true,
            Err(e) => {
                log(&format!("audio: - {}: {e}", i.name));
                opened.remove(&i.pid);
                false
            }
        });
        if by_app && !stopping && qpc() >= next_scan {
            next_scan = qpc() + freq * 2;
            open_apps(&apps, &mut inputs, &mut opened);
        }
        // 停止時は遅延分も含めて最後まで書き切る
        let target = if stopping { now } else { now - LATENCY };
        if target > pos {
            let n = (target - pos) as usize;
            mix.clear();
            mix.resize(n * CH, 0.0);
            for i in inputs.iter_mut() {
                let take = (n * CH).min(i.ring.len());
                for (m, s) in mix.iter_mut().zip(i.ring.drain(..take)) {
                    *m += s;
                }
                // 届くのが遅れ続けて溜まりすぎた分は捨てる
                let max = (LATENCY as usize * 3) * CH;
                if i.ring.len() > max {
                    let extra = i.ring.len() - LATENCY as usize * CH;
                    i.ring.drain(..extra - extra % CH);
                }
            }
            pcm.clear();
            pcm.extend(mix.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
            if let Err(e) = enc.lock().unwrap().write_audio(&pcm, pos as u64) {
                log(&format!("audio write: {e}"));
            }
            pos = target;
        }
        if stopping {
            break;
        }
        sleeper.until(qpc() + freq / 100, freq);
    }
    drop(inputs);
    unsafe { CoUninitialize() };
}
