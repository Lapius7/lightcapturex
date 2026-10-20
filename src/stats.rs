//! 録画中の数値（フレーム数・音量）。録画スレッドが書き、画面が読む。
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

/// 画面から新しく取り込めたフレーム数（ゲームの実 fps の目安）
pub static FRESH: AtomicU64 = AtomicU64::new(0);
/// 処理が間に合わず飛ばしたフレーム数
pub static DROPPED: AtomicU64 = AtomicU64::new(0);
// 直近の最大振幅（0..1 を f32 のビット列で持つ。非負なので fetch_max で大小が保てる）
static SYS_PEAK: AtomicU32 = AtomicU32::new(0);
static MIC_PEAK: AtomicU32 = AtomicU32::new(0);

pub fn reset() {
    for a in [&FRESH, &DROPPED] {
        a.store(0, Relaxed);
    }
    SYS_PEAK.store(0, Relaxed);
    MIC_PEAK.store(0, Relaxed);
}

pub fn push_peak(mic: bool, v: f32) {
    let a = if mic { &MIC_PEAK } else { &SYS_PEAK };
    a.fetch_max(v.clamp(0.0, 1.0).to_bits(), Relaxed);
}

/// 前回の取得からの最大音量 (PC の音, マイク)
pub fn take_peaks() -> (f32, f32) {
    (f32::from_bits(SYS_PEAK.swap(0, Relaxed)), f32::from_bits(MIC_PEAK.swap(0, Relaxed)))
}
