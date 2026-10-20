use windows::core::{Interface, Result};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::HMONITOR;

fn adapters() -> Vec<(String, IDXGIAdapter1)> {
    let mut out = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return out };
    let mut i = 0;
    while let Ok(a) = unsafe { factory.EnumAdapters1(i) } {
        i += 1;
        let Ok(d) = (unsafe { a.GetDesc1() }) else { continue };
        if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let n = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
        out.push((String::from_utf16_lossy(&d.Description[..n]), a));
    }
    out
}

/// GPU の名前一覧（GUI の選択肢用）
pub fn adapter_names() -> Vec<String> {
    // ハイブリッド GPU では同じ GPU が並ばずに 2 回出ることがある
    let mut v: Vec<String> = Vec::new();
    for (n, _) in adapters() {
        if !v.contains(&n) {
            v.push(n);
        }
    }
    v
}

/// そのモニターを出力している GPU。画面の合成もこの GPU で行われるので、
/// 同じ GPU でエンコードすれば GPU 間のコピーが発生しない
fn adapter_for_monitor(mon: HMONITOR) -> Option<IDXGIAdapter1> {
    adapters().into_iter().find_map(|(_, a)| {
        let mut j = 0;
        while let Ok(o) = unsafe { a.EnumOutputs(j) } {
            j += 1;
            if unsafe { o.GetDesc() }.map(|d| d.Monitor == mon).unwrap_or(false) {
                return Some(a.clone());
            }
        }
        None
    })
}

/// 候補の GPU（優先順）。名前指定 → モニター担当 → 既定 の順に試す
pub fn candidates(name: Option<&str>, mon: HMONITOR) -> Vec<Option<IDXGIAdapter1>> {
    let mut v = Vec::new();
    if let Some(n) = name {
        if let Some((_, a)) = adapters().into_iter().find(|(an, _)| an == n) {
            v.push(Some(a));
        }
    }
    if let Some(a) = adapter_for_monitor(mon) {
        v.push(Some(a));
    }
    v.push(None);
    v
}

pub fn adapter_name(device: &ID3D11Device) -> String {
    (|| -> Result<String> {
        let a = unsafe { device.cast::<IDXGIDevice>()?.GetAdapter()? };
        let d = unsafe { a.GetDesc()? };
        let n = d.Description.iter().position(|&c| c == 0).unwrap_or(0);
        Ok(String::from_utf16_lossy(&d.Description[..n]))
    })()
    .unwrap_or_default()
}

pub fn create_device(adapter: Option<&IDXGIAdapter1>) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut ctx = None;
    let adapter: Option<IDXGIAdapter> = adapter.map(|a| a.cast()).transpose()?;
    unsafe {
        D3D11CreateDevice(
            adapter.as_ref(),
            if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE },
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
    }
    let device = device.unwrap();
    // キャプチャ処理と Media Foundation のエンコーダが同じデバイスを触るため
    let mt: ID3D11Multithread = device.cast()?;
    unsafe {
        let _ = mt.SetMultithreadProtected(true);
    }
    // ゲームに GPU を取られて録画がカクつくのを軽減する（権限がなければ無視）
    if let Ok(dxgi) = device.cast::<IDXGIDevice>() {
        unsafe {
            let _ = dxgi.SetGPUThreadPriority(7);
        }
    }
    Ok((device, ctx.unwrap()))
}
