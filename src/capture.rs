use windows::core::{factory, Interface, Result, BOOL};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Foundation::TimeSpan;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::*;

/// スレッド間で受け渡せるようにハンドルは整数で持つ
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Source {
    Monitor(isize),
    Window(isize),
}

impl Source {
    /// 録画対象が映っているモニター
    pub fn monitor(&self) -> HMONITOR {
        match *self {
            Source::Monitor(h) => HMONITOR(h as *mut _),
            Source::Window(h) => unsafe { MonitorFromWindow(HWND(h as *mut _), MONITOR_DEFAULTTONEAREST) },
        }
    }
}

pub struct SourceInfo {
    pub name: String,
    pub source: Source,
}

pub fn list_sources(exclude: HWND) -> Vec<SourceInfo> {
    let mut out = Vec::new();
    let mut monitors: Vec<(isize, RECT)> = Vec::new();
    unsafe extern "system" fn mon_cb(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let v = &mut *(lp.0 as *mut Vec<(isize, RECT)>);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(h, &mut mi);
        v.push((h.0 as isize, mi.rcMonitor));
        true.into()
    }
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(mon_cb), LPARAM(&mut monitors as *mut _ as isize));
    }
    for (i, (h, r)) in monitors.iter().enumerate() {
        out.push(SourceInfo {
            name: format!("モニター {} ({}x{})", i + 1, r.right - r.left, r.bottom - r.top),
            source: Source::Monitor(*h),
        });
    }

    let mut windows: Vec<isize> = Vec::new();
    unsafe extern "system" fn win_cb(h: HWND, lp: LPARAM) -> BOOL {
        let v = &mut *(lp.0 as *mut Vec<isize>);
        v.push(h.0 as isize);
        true.into()
    }
    unsafe {
        let _ = EnumWindows(Some(win_cb), LPARAM(&mut windows as *mut _ as isize));
    }
    for h in windows {
        let hwnd = HWND(h as *mut _);
        if hwnd == exclude || !is_capturable_window(hwnd) {
            continue;
        }
        let title = window_title(hwnd);
        out.push(SourceInfo { name: format!("ウィンドウ: {title}"), source: Source::Window(h) });
    }
    out
}

fn is_capturable_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) == 0 {
            return false;
        }
        if GetAncestor(hwnd, GA_ROOT) != hwnd {
            return false;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
        cloaked == 0
    }
}

pub fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

pub struct Capture {
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    d3d: IDirect3DDevice,
    pool_size: SizeInt32,
}

impl Capture {
    pub fn new(device: &ID3D11Device, source: Source, cursor: bool, fps: u32) -> Result<Self> {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe {
            match source {
                Source::Monitor(h) => interop.CreateForMonitor(HMONITOR(h as *mut _))?,
                Source::Window(h) => interop.CreateForWindow(HWND(h as *mut _))?,
            }
        };
        let dxgi: IDXGIDevice = device.cast()?;
        let d3d: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let size = item.Size()?;
        // 専用スレッドから TryGetNextFrame で取り出すので FreeThreaded にする
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            4,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(cursor);
        // 高 fps のゲームでも、録画の fps を超える頻度では画面を取り込ませない（Windows 11 24H2 以降）
        let _ = session.SetMinUpdateInterval(TimeSpan { Duration: 10_000_000 / fps.max(1) as i64 });
        session.StartCapture()?;
        Ok(Self { _item: item, pool, session, d3d, pool_size: size })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.pool_size.Width.max(1) as u32, self.pool_size.Height.max(1) as u32)
    }

    /// 溜まっているフレームを捨て、最新の 1 枚を f に渡す（テクスチャと有効領域のサイズ）。
    /// テクスチャはフレームを閉じるとプールに戻るので、f の中でコピーし終えること。
    pub fn latest(&mut self, f: impl FnOnce(&ID3D11Texture2D, (u32, u32))) -> bool {
        let mut last = None;
        while let Ok(fr) = self.pool.TryGetNextFrame() {
            last = Some(fr);
        }
        let Some(frame) = last else { return false };
        let Ok(content) = frame.ContentSize() else { return false };
        let tex = (|| -> Result<ID3D11Texture2D> {
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            unsafe { access.GetInterface() }
        })();
        let ok = tex.is_ok();
        if let Ok(tex) = tex {
            f(&tex, (content.Width.max(1) as u32, content.Height.max(1) as u32));
        }
        // ウィンドウのサイズが変わったらプールを作り直す
        if content.Width > 0
            && content.Height > 0
            && (content.Width != self.pool_size.Width || content.Height != self.pool_size.Height)
        {
            self.pool_size = content;
            let _ = self.pool.Recreate(&self.d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 4, content);
        }
        let _ = frame.Close();
        ok
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
