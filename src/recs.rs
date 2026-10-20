//! 録画一覧: 保存先のフォルダにある mp4 を集め、長さ・日時・サイズを読む。
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::Shell::{SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW};

#[derive(Clone, Default)]
pub struct RecEntry {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    /// 長さ（秒）。読めなければ None
    pub secs: Option<u64>,
    /// 更新日時（ローカル時間）: 年, 月, 日, 時, 分
    pub when: (u16, u16, u16, u16, u16),
}

/// mp4 の moov/mvhd から長さを読む
fn duration(path: &Path) -> Option<u64> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut pos = 0u64;
    let header = |f: &mut File, at: u64| -> Option<(u64, [u8; 4], u64)> {
        f.seek(SeekFrom::Start(at)).ok()?;
        let mut h = [0u8; 8];
        f.read_exact(&mut h).ok()?;
        let size = u32::from_be_bytes(h[..4].try_into().ok()?) as u64;
        let typ: [u8; 4] = h[4..].try_into().ok()?;
        match size {
            1 => {
                let mut b = [0u8; 8];
                f.read_exact(&mut b).ok()?;
                Some((u64::from_be_bytes(b), typ, 16))
            }
            0 => Some((len - at, typ, 8)),
            n => Some((n, typ, 8)),
        }
    };
    while pos + 8 <= len {
        let (size, typ, hdr) = header(&mut f, pos)?;
        if size < hdr {
            return None;
        }
        if &typ == b"moov" {
            let (mut cp, end) = (pos + hdr, pos + size);
            while cp + 8 <= end {
                let (csize, ctyp, chdr) = header(&mut f, cp)?;
                if &ctyp == b"mvhd" {
                    f.seek(SeekFrom::Start(cp + chdr)).ok()?;
                    let mut v = [0u8; 4];
                    f.read_exact(&mut v).ok()?;
                    let (scale, dur) = if v[0] == 1 {
                        let mut b = [0u8; 28];
                        f.read_exact(&mut b).ok()?;
                        (u32::from_be_bytes(b[16..20].try_into().ok()?) as u64, u64::from_be_bytes(b[20..28].try_into().ok()?))
                    } else {
                        let mut b = [0u8; 16];
                        f.read_exact(&mut b).ok()?;
                        (u32::from_be_bytes(b[8..12].try_into().ok()?) as u64, u32::from_be_bytes(b[12..16].try_into().ok()?) as u64)
                    };
                    return (scale > 0).then(|| (dur + scale / 2) / scale);
                }
                if csize < chdr {
                    return None;
                }
                cp += csize;
            }
            return None;
        }
        pos += size;
    }
    None
}

fn local_time(t: std::time::SystemTime) -> (u16, u16, u16, u16, u16) {
    let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) else { return (0, 0, 0, 0, 0) };
    let ticks = (d.as_secs() + 11_644_473_600) * 10_000_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    unsafe {
        let mut utc = Default::default();
        let mut loc = Default::default();
        if FileTimeToSystemTime(&ft, &mut utc).is_err() || SystemTimeToTzSpecificLocalTime(None, &utc, &mut loc).is_err() {
            return (0, 0, 0, 0, 0);
        }
        (loc.wYear, loc.wMonth, loc.wDay, loc.wHour, loc.wMinute)
    }
}

/// 保存先の mp4 を新しい順に集める。skip は録画中のファイル
pub fn scan(dir: &Path, skip: Option<&Path>) -> Vec<RecEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<(std::time::SystemTime, RecEntry)> = rd
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4")) || skip.is_some_and(|s| s == path) {
                return None;
            }
            let m = e.metadata().ok().filter(|m| m.is_file())?;
            let t = m.modified().ok()?;
            let name = path.file_name()?.to_string_lossy().into_owned();
            Some((t, RecEntry { secs: duration(&path), when: local_time(t), size: m.len(), name, path }))
        })
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.into_iter().take(500).map(|x| x.1).collect()
}

/// ごみ箱へ移す（あとで戻せる）
pub fn recycle(path: &Path) -> bool {
    let mut from: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().collect();
    from.extend([0, 0]);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    unsafe { SHFileOperationW(&mut op) == 0 && !op.fAnyOperationsAborted.as_bool() }
}

/// 保存先のドライブの空き容量（バイト）
pub fn free_bytes(dir: &Path) -> Option<u64> {
    let mut d = dir;
    while !d.exists() {
        d = d.parent()?;
    }
    let mut avail = 0u64;
    unsafe { GetDiskFreeSpaceExW(&HSTRING::from(d.as_os_str()), Some(&mut avail), None, None).ok()? };
    Some(avail)
}

pub fn fmt_size(b: u64) -> String {
    let mb = b as f64 / 1_048_576.0;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{:.0} MB", mb)
    }
}

pub fn fmt_dur(s: u64) -> String {
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}
