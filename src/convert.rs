//! キャプチャした BGRA を GPU の Video Processor で NV12 に変換し、同時に出力サイズへ縮小する。
use crate::log;
use std::mem::ManuallyDrop;
use windows::core::{Interface, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// エンコーダがまだ読んでいる途中のテクスチャに書き込まないよう、出力は輪番で使う
const SLOTS: usize = 8;

pub struct Converter {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vctx: ID3D11VideoContext,
    vdev: ID3D11VideoDevice,
    enumr: ID3D11VideoProcessorEnumerator,
    proc: ID3D11VideoProcessor,
    out_w: u32,
    out_h: u32,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    /// キャプチャのテクスチャごとの入力ビュー（アドレス, テクスチャ, ビュー）
    views: Vec<(usize, ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    direct: bool,
    copy: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView, u32, u32)>,
    rect_for: (u32, u32),
    slot: usize,
}

impl Converter {
    pub fn new(device: &ID3D11Device, ctx: &ID3D11DeviceContext, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Result<Self> {
        let vdev: ID3D11VideoDevice = device.cast()?;
        let vctx: ID3D11VideoContext = ctx.cast()?;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumr = unsafe { vdev.CreateVideoProcessorEnumerator(&desc)? };
        let proc = unsafe { vdev.CreateVideoProcessor(&enumr, 0)? };
        unsafe {
            vctx.VideoProcessorSetStreamFrameFormat(&proc, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&proc, 0, false);
            // 入力: RGB フルレンジ / 出力: BT.709 リミテッドレンジ
            let input_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let output_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
            vctx.VideoProcessorSetStreamColorSpace(&proc, 0, &input_cs);
            vctx.VideoProcessorSetOutputColorSpace(&proc, &output_cs);
            let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
            vctx.VideoProcessorSetOutputBackgroundColor(&proc, false, &black);
            let full = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
            vctx.VideoProcessorSetOutputTargetRect(&proc, true, Some(&full));
        }

        let mut outputs = Vec::with_capacity(SLOTS);
        for _ in 0..SLOTS {
            let td = D3D11_TEXTURE2D_DESC {
                Width: out_w,
                Height: out_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            let mut tex = None;
            unsafe { device.CreateTexture2D(&td, None, Some(&mut tex))? };
            let tex = tex.unwrap();
            let vd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut view = None;
            unsafe { vdev.CreateVideoProcessorOutputView(&tex, &enumr, &vd, Some(&mut view))? };
            outputs.push((tex, view.unwrap()));
        }

        Ok(Self { device: device.clone(), ctx: ctx.clone(), vctx, vdev, enumr, proc, out_w, out_h, outputs, views: Vec::new(), direct: std::env::var_os("LCX_COPY").is_none(), copy: None, rect_for: (0, 0), slot: 0 })
    }

    fn input_view(&self, tex: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let vd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut view = None;
        unsafe { self.vdev.CreateVideoProcessorInputView(tex, &self.enumr, &vd, Some(&mut view))? };
        Ok(view.unwrap())
    }

    /// キャプチャのテクスチャを直接読む入力ビュー。プールのテクスチャは数枚を使い回すのでキャッシュする
    fn direct_view(&mut self, frame: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let key = frame.as_raw() as usize;
        if let Some((_, _, v)) = self.views.iter().find(|(k, _, _)| *k == key) {
            return Ok(v.clone());
        }
        let v = self.input_view(frame)?;
        if self.views.len() >= 6 {
            self.views.remove(0);
        }
        // テクスチャも保持しておき、アドレスの再利用で別物と取り違えないようにする
        self.views.push((key, frame.clone(), v.clone()));
        Ok(v)
    }

    /// 直接読めない環境向け：自前のテクスチャへコピーしてから読む
    fn copied_view(&mut self, frame: &ID3D11Texture2D, w: u32, h: u32) -> Result<ID3D11VideoProcessorInputView> {
        if !matches!(&self.copy, Some((_, _, cw, ch)) if *cw == w && *ch == h) {
            self.copy = None;
            let td = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                ..Default::default()
            };
            let mut tex = None;
            unsafe { self.device.CreateTexture2D(&td, None, Some(&mut tex))? };
            let tex = tex.unwrap();
            let view = self.input_view(&tex)?;
            self.copy = Some((tex, view, w, h));
        }
        let (tex, view, _, _) = self.copy.as_ref().unwrap();
        unsafe { self.ctx.CopyResource(tex, frame) };
        Ok(view.clone())
    }

    /// フレームを NV12 に変換し、書き込んだテクスチャを返す（キャプチャのフレームが閉じられる前に呼ぶ）
    pub fn process(&mut self, frame: &ID3D11Texture2D, content: (u32, u32)) -> Result<ID3D11Texture2D> {
        let mut fd = D3D11_TEXTURE2D_DESC::default();
        unsafe { frame.GetDesc(&mut fd) };
        let view = if self.direct {
            match self.direct_view(frame) {
                Ok(v) => v,
                Err(e) => {
                    log(&format!("convert: direct input unavailable ({e}), using copy"));
                    self.direct = false;
                    self.views.clear();
                    self.copied_view(frame, fd.Width, fd.Height)?
                }
            }
        } else {
            self.copied_view(frame, fd.Width, fd.Height)?
        };
        let cw = content.0.min(fd.Width);
        let ch = content.1.min(fd.Height);
        if self.rect_for != (cw, ch) {
            self.rect_for = (cw, ch);
            // 有効領域はアスペクト比を保ったまま中央に収める
            let scale = (self.out_w as f64 / cw as f64).min(self.out_h as f64 / ch as f64);
            let dw = ((cw as f64 * scale).round() as i32).min(self.out_w as i32);
            let dh = ((ch as f64 * scale).round() as i32).min(self.out_h as i32);
            let dx = (self.out_w as i32 - dw) / 2;
            let dy = (self.out_h as i32 - dh) / 2;
            unsafe {
                let src = RECT { left: 0, top: 0, right: cw as i32, bottom: ch as i32 };
                let dst = RECT { left: dx, top: dy, right: dx + dw, bottom: dy + dh };
                self.vctx.VideoProcessorSetStreamSourceRect(&self.proc, 0, true, Some(&src));
                self.vctx.VideoProcessorSetStreamDestRect(&self.proc, 0, true, Some(&dst));
            }
        }
        self.slot = (self.slot + 1) % SLOTS;
        let (tex, out_view) = &self.outputs[self.slot];
        let streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(view)),
            ..Default::default()
        }];
        let r = unsafe { self.vctx.VideoProcessorBlt(&self.proc, out_view, 0, &streams) };
        let [mut st] = streams;
        unsafe { ManuallyDrop::drop(&mut st.pInputSurface) };
        r?;
        Ok(tex.clone())
    }
}
