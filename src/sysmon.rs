//! 録画中の CPU / GPU 使用率。GPU は PDH の「GPU Engine」を 1 秒ごとに別スレッドで測る
//! （一度に数十 ms かかることがあるので、画面のスレッドでは測らない）。
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use windows::core::w;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Performance::*;
use windows::Win32::System::Threading::GetSystemTimes;

static RUN: AtomicBool = AtomicBool::new(false);
static CPU: AtomicU32 = AtomicU32::new(0);
static GPU: AtomicU32 = AtomicU32::new(0);

/// (CPU %, GPU %)。測れていないものは None
pub fn read() -> (Option<u32>, Option<u32>) {
    let f = |a: &AtomicU32| Some(a.load(Relaxed)).filter(|&v| v != u32::MAX);
    (f(&CPU), f(&GPU))
}

pub fn stop() {
    RUN.store(false, Relaxed);
}

pub fn start() {
    if RUN.swap(true, Relaxed) {
        return;
    }
    CPU.store(u32::MAX, Relaxed);
    GPU.store(u32::MAX, Relaxed);
    std::thread::spawn(|| {
        let mut gpu = GpuCounter::new();
        let mut prev = times();
        while RUN.load(Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            let now = times();
            let (idle, total) = (now.0.wrapping_sub(prev.0), now.1.wrapping_sub(prev.1));
            prev = now;
            if total > 0 {
                CPU.store((100 - (idle * 100 / total).min(100)) as u32, Relaxed);
            }
            if let Some(v) = gpu.as_mut().and_then(|g| g.sample()) {
                GPU.store(v, Relaxed);
            }
        }
    });
}

fn ft(f: FILETIME) -> u64 {
    (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
}

/// (アイドル時間, 全体の時間)。kernel にはアイドルも含まれる
fn times() -> (u64, u64) {
    let (mut i, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe {
        let _ = GetSystemTimes(Some(&mut i), Some(&mut k), Some(&mut u));
    }
    (ft(i), ft(k) + ft(u))
}

struct GpuCounter {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    primed: bool,
}

impl GpuCounter {
    fn new() -> Option<Self> {
        unsafe {
            let mut q = PDH_HQUERY::default();
            if PdhOpenQueryW(None, 0, &mut q) != 0 {
                return None;
            }
            let mut c = PDH_HCOUNTER::default();
            // 英語名で指定する（日本語環境でもカウンタ名が変わらない）
            if PdhAddEnglishCounterW(q, w!("\\GPU Engine(*engtype_3D)\\Utilization Percentage"), 0, &mut c) != 0 {
                let _ = PdhCloseQuery(q);
                return None;
            }
            Some(GpuCounter { query: q, counter: c, primed: false })
        }
    }

    fn sample(&mut self) -> Option<u32> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            // 1 回目は差分が取れないので捨てる
            if !self.primed {
                self.primed = true;
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            // 0x800007D2 = PDH_MORE_DATA（必要な大きさが size に入る）
            PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            if size == 0 {
                return None;
            }
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
                return None;
            }
            // インスタンス名は「pid_…_luid_…_phys_0_eng_0_engtype_3D」。同じ GPU のエンジンごとに全プロセスの値を足し、最大のものを使う
            let mut groups: Vec<(String, f64)> = Vec::new();
            for it in std::slice::from_raw_parts(items, count as usize) {
                let name = it.szName.to_string().unwrap_or_default();
                let key = name.split_once("luid_").map_or(name.clone(), |(_, r)| r.to_string());
                let v = it.FmtValue.Anonymous.doubleValue;
                match groups.iter_mut().find(|g| g.0 == key) {
                    Some(g) => g.1 += v,
                    None => groups.push((key, v)),
                }
            }
            Some(groups.iter().map(|g| g.1).fold(0.0, f64::max).round().min(100.0) as u32)
        }
    }
}

impl Drop for GpuCounter {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}
