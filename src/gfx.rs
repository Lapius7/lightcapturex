//! Direct2D / DirectWrite で描く小さな描画層と、色・寸法の決まった値。
//! 画面の色や角丸は必ず `tok` の値を使う（描画コードの中で色を直に書かない）。

use std::cell::{OnceCell, RefCell};
use windows::core::{Interface, Result, BOOL, HSTRING};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows_numerics::{Matrix3x2, Vector2};

#[derive(Clone, Copy, PartialEq)]
pub struct C(pub f32, pub f32, pub f32, pub f32);

const fn rgb(h: u32) -> C {
    C(((h >> 16) & 255) as f32 / 255.0, ((h >> 8) & 255) as f32 / 255.0, (h & 255) as f32 / 255.0, 1.0)
}
const fn white(a: f32) -> C {
    C(1.0, 1.0, 1.0, a)
}

/// 色・角丸・寸法（グラファイトの地に、録画の赤だけを色として使う）
pub mod tok {
    use super::{rgb, white, C};
    pub const BG: C = rgb(0x151618);
    pub const CARD: C = rgb(0x1C1D20);
    pub const CTRL: C = rgb(0x25272B);
    pub const CTRL_HOVER: C = rgb(0x2D3034);
    pub const CTRL_PRESS: C = rgb(0x33363B);
    pub const SEG_ON: C = rgb(0x3B3E44);
    pub const HAIR: C = white(0.06);
    pub const EDGE: C = white(0.09);
    pub const ICON_EDGE: C = white(0.10);
    pub const TEXT: C = rgb(0xE9EAEC);
    pub const TEXT_2: C = rgb(0x9B9EA4);
    pub const TEXT_3: C = rgb(0x676B72);
    pub const FOCUS: C = C(0.91, 0.92, 0.93, 0.55);
    pub const REC: C = rgb(0xE5484D);
    pub const REC_HOVER: C = rgb(0xEC5B60);
    pub const REC_PRESS: C = rgb(0xD23E43);
    pub const ON_REC: C = white(0.2);
    /// 赤の上の文字
    pub const ON_ACCENT: C = white(1.0);
    /// 録画中の点滅で消えている側
    pub const REC_DIM: C = C(0.898, 0.282, 0.302, 0.3);
    pub const SEG_ON_HOVER: C = rgb(0x44474D);
    pub const HALO: C = white(0.14);
    pub const SHADE: C = C(0.0, 0.0, 0.0, 0.45);

    pub const R_CARD: f32 = 10.0;
    pub const R_CTRL: f32 = 7.0;
    pub const SEG_PAD: f32 = 2.0;
    /// 入れ子の角丸は 外側 = 内側 + 余白
    pub const R_SEG: f32 = R_CTRL - SEG_PAD;
    pub const R_POP: f32 = 8.0;
}

/// Segoe Fluent Icons / Segoe MDL2 Assets の字形
pub mod glyph {
    pub const CHEVRON: u16 = 0xE70D;
    pub const CHECK: u16 = 0xE73E;
    pub const MONITOR: u16 = 0xE7F4;
    pub const WINDOW: u16 = 0xE737;
    pub const VOLUME: u16 = 0xE767;
    pub const FOLDER: u16 = 0xE838;
    pub const REFRESH: u16 = 0xE72C;
    pub const WARNING: u16 = 0xE7BA;
    pub const ERROR: u16 = 0xEA39;
    pub const PLAY: u16 = 0xE768;
    pub const DELETE: u16 = 0xE74D;
    pub const VIDEO: u16 = 0xE714;
    pub const EDIT: u16 = 0xE70F;
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct R {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl R {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        R { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    pub fn inset(&self, dx: f32, dy: f32) -> R {
        R::new(self.x + dx, self.y + dy, (self.w - dx * 2.0).max(0.0), (self.h - dy * 2.0).max(0.0))
    }
    /// 左から w を切り出す
    /// 右から w を切り出す
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.x, top: self.y, right: self.right(), bottom: self.bottom() }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Face {
    Text,
    Glyph,
}

#[derive(Clone, Copy, PartialEq)]
pub struct Font {
    pub size: f32,
    pub bold: bool,
    pub align: Align,
    pub face: Face,
}

impl Font {
    pub const fn text(size: f32) -> Self {
        Font { size, bold: false, align: Align::Left, face: Face::Text }
    }
    pub const fn strong(size: f32) -> Self {
        Font { size, bold: true, align: Align::Left, face: Face::Text }
    }
    pub const fn glyph(size: f32) -> Self {
        Font { size, bold: false, align: Align::Center, face: Face::Glyph }
    }
    pub const fn at(mut self, align: Align) -> Self {
        self.align = align;
        self
    }
    fn key(&self) -> u32 {
        (self.size * 4.0) as u32 | (self.bold as u32) << 12 | (self.align as u32) << 13 | (self.face as u32) << 15
    }
}

/// 共有するファクトリ（UI スレッドで1回だけ作る）
#[derive(Clone)]
pub struct Gfx {
    pub d2d: ID2D1Factory,
    pub dw: IDWriteFactory,
    pub wic: IWICImagingFactory,
    glyph_family: HSTRING,
}

thread_local! {
    static GFX: OnceCell<Gfx> = const { OnceCell::new() };
}

pub fn gfx() -> Gfx {
    GFX.with(|g| {
        g.get_or_init(|| unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).expect("D2D1CreateFactory");
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).expect("DWriteCreateFactory");
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).expect("WIC");
            // Windows 11 は Segoe Fluent Icons、10 は Segoe MDL2 Assets
            let mut fluent = false;
            let mut coll = None;
            if dw.GetSystemFontCollection(&mut coll, false).is_ok() {
                if let Some(c) = coll {
                    let (mut i, mut ok) = (0u32, BOOL(0));
                    fluent = c.FindFamilyName(&HSTRING::from("Segoe Fluent Icons"), &mut i, &mut ok).is_ok() && ok.as_bool();
                }
            }
            let glyph_family = HSTRING::from(if fluent { "Segoe Fluent Icons" } else { "Segoe MDL2 Assets" });
            Gfx { d2d, dw, wic, glyph_family }
        })
        .clone()
    })
}

pub struct Painter {
    pub rt: ID2D1RenderTarget,
    hwnd_rt: Option<ID2D1HwndRenderTarget>,
    brush: ID2D1SolidColorBrush,
    g: Gfx,
    formats: RefCell<Vec<(u32, IDWriteTextFormat)>>,
    bitmaps: RefCell<Vec<(String, ID2D1Bitmap)>>,
}

fn props(dpi: f32) -> D2D1_RENDER_TARGET_PROPERTIES {
    D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: dpi,
        dpiY: dpi,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    }
}

impl Painter {
    fn from_rt(rt: ID2D1RenderTarget, hwnd_rt: Option<ID2D1HwndRenderTarget>, g: Gfx) -> Result<Self> {
        unsafe {
            // 薄い文字がにじまないよう、グレースケールのアンチエイリアスにそろえる
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let brush = rt.CreateSolidColorBrush(&D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, None)?;
            Ok(Painter { rt, hwnd_rt, brush, g, formats: RefCell::new(Vec::new()), bitmaps: RefCell::new(Vec::new()) })
        }
    }

    pub fn for_hwnd(hwnd: HWND, w: u32, h: u32, dpi: f32) -> Result<Self> {
        let g = gfx();
        let hp = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U { width: w.max(1), height: h.max(1) },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        let hrt = unsafe { g.d2d.CreateHwndRenderTarget(&props(dpi), &hp)? };
        Self::from_rt(hrt.cast()?, Some(hrt), g)
    }

    pub fn for_wic(bmp: &IWICBitmap, dpi: f32) -> Result<Self> {
        let g = gfx();
        let rt = unsafe { g.d2d.CreateWicBitmapRenderTarget(bmp, &props(dpi))? };
        Self::from_rt(rt, None, g)
    }

    pub fn resize(&self, w: u32, h: u32) {
        if let Some(h_rt) = &self.hwnd_rt {
            unsafe { let _ = h_rt.Resize(&D2D_SIZE_U { width: w.max(1), height: h.max(1) }); }
        }
    }

    pub fn set_dpi(&self, dpi: f32) {
        unsafe { self.rt.SetDpi(dpi, dpi) };
    }

    pub fn begin(&self, bg: C) {
        unsafe {
            self.rt.BeginDraw();
            self.rt.SetTransform(&Matrix3x2::identity());
            self.rt.Clear(Some(&col(bg)));
        }
    }

    /// 描画を終える。D2DERR_RECREATE_TARGET などのときは Err を返すので、作り直す
    pub fn end(&self) -> Result<()> {
        unsafe { self.rt.EndDraw(None, None) }
    }

    fn b(&self, c: C) -> &ID2D1SolidColorBrush {
        unsafe { self.brush.SetColor(&col(c)) };
        &self.brush
    }

    pub fn fill(&self, r: R, radius: f32, c: C) {
        unsafe {
            if radius <= 0.0 {
                self.rt.FillRectangle(&r.d2d(), self.b(c));
            } else {
                self.rt.FillRoundedRectangle(&rr(r, radius), self.b(c));
            }
        }
    }

    /// 内側に収まる線（幅 w）で枠を描く
    pub fn stroke(&self, r: R, radius: f32, c: C, w: f32) {
        let h = w / 2.0;
        unsafe { self.rt.DrawRoundedRectangle(&rr(r.inset(h, h), (radius - h).max(0.0)), self.b(c), w, None) };
    }

    pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, c: C, w: f32) {
        unsafe { self.rt.DrawLine(Vector2 { X: x0, Y: y0 }, Vector2 { X: x1, Y: y1 }, self.b(c), w, None) };
    }

    pub fn dot(&self, cx: f32, cy: f32, rad: f32, c: C) {
        let e = D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: rad, radiusY: rad };
        unsafe { self.rt.FillEllipse(&e, self.b(c)) };
    }

    fn format(&self, f: Font) -> Option<IDWriteTextFormat> {
        let key = f.key();
        if let Some((_, tf)) = self.formats.borrow().iter().find(|(k, _)| *k == key) {
            return Some(tf.clone());
        }
        unsafe {
            let family = match f.face {
                Face::Text => HSTRING::from("Segoe UI"),
                Face::Glyph => self.g.glyph_family.clone(),
            };
            let weight = if f.bold { DWRITE_FONT_WEIGHT_SEMI_BOLD } else { DWRITE_FONT_WEIGHT_NORMAL };
            let tf = self
                .g
                .dw
                .CreateTextFormat(&family, None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, f.size, &HSTRING::from("ja-jp"))
                .ok()?;
            let _ = tf.SetTextAlignment(match f.align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
            let _ = tf.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = tf.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            if let Ok(sign) = self.g.dw.CreateEllipsisTrimmingSign(&tf) {
                let t = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
                let _ = tf.SetTrimming(&t, &sign);
            }
            self.formats.borrow_mut().push((key, tf.clone()));
            Some(tf)
        }
    }

    /// 1行の文字列を r の中に描く（縦は中央、はみ出す分は … で省く）
    pub fn text(&self, s: &str, r: R, f: Font, c: C) {
        let Some(tf) = self.format(f) else { return };
        let w: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            self.rt.DrawText(&w, &tf, &r.d2d(), self.b(c), D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
        }
    }

    pub fn glyph(&self, code: u16, r: R, size: f32, c: C) {
        let Some(tf) = self.format(Font::glyph(size)) else { return };
        unsafe { self.rt.DrawText(&[code], &tf, &r.d2d(), self.b(c), D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL) };
    }

    /// 文字列の幅（DIP）
    pub fn measure(&self, s: &str, f: Font) -> f32 {
        let Some(tf) = self.format(f.at(Align::Left)) else { return 0.0 };
        let w: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            let Ok(layout) = self.g.dw.CreateTextLayout(&w, &tf, 4096.0, 100.0) else { return 0.0 };
            let mut m = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut m).is_ok() {
                m.widthIncludingTrailingWhitespace
            } else {
                0.0
            }
        }
    }

    /// アプリのアイコンなど。key ごとに D2D のビットマップを作って使い回す
    pub fn image(&self, key: &str, src: &IWICBitmap, r: R) {
        let cached = self.bitmaps.borrow().iter().find(|(k, _)| k == key).map(|(_, b)| b.clone());
        let bmp = match cached {
            Some(b) => b,
            None => {
                let made = unsafe {
                    self.g.wic.CreateFormatConverter().and_then(|conv| {
                        conv.Initialize(src, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeMedianCut)?;
                        self.rt.CreateBitmapFromWicBitmap(&conv, None)
                    })
                };
                let Ok(b) = made else { return };
                self.bitmaps.borrow_mut().push((key.to_string(), b.clone()));
                b
            }
        };
        unsafe { self.rt.DrawBitmap(&bmp, Some(&r.d2d()), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None) };
    }

    pub fn clip(&self, r: R) {
        unsafe { self.rt.PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_ALIASED) };
    }

    pub fn unclip(&self) {
        unsafe { self.rt.PopAxisAlignedClip() };
    }

    /// 押している間だけ、r の中心を基準に縮める
    pub fn scale(&self, r: R, s: f32) {
        let (cx, cy) = r.center();
        unsafe { self.rt.SetTransform(&Matrix3x2::scale_around(s, s, Vector2 { X: cx, Y: cy })) };
    }

    pub fn unscale(&self) {
        unsafe { self.rt.SetTransform(&Matrix3x2::identity()) };
    }
}

fn col(c: C) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: c.0, g: c.1, b: c.2, a: c.3 }
}

fn rr(r: R, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius }
}

/// 動作確認用: w×h ピクセルの WIC ビットマップに描いて PNG に書き出す
pub fn render_png(path: &str, w: u32, h: u32, dpi: f32, draw: impl FnOnce(&Painter)) -> Result<()> {
    let g = gfx();
    unsafe {
        let bmp = g.wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
        {
            let p = Painter::for_wic(&bmp, dpi)?;
            draw(&p);
        }
        let stream = g.wic.CreateStream()?;
        stream.InitializeFromFilename(&HSTRING::from(path), 0x40000000 /* GENERIC_WRITE */)?;
        let enc = g.wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        let mut bag = None;
        enc.CreateNewFrame(&mut frame, &mut bag)?;
        let frame = frame.ok_or(windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        frame.Initialize(bag.as_ref())?;
        frame.SetSize(w, h)?;
        let mut fmt = GUID_WICPixelFormat32bppPBGRA;
        frame.SetPixelFormat(&mut fmt)?;
        frame.WriteSource(&bmp, std::ptr::null())?;
        frame.Commit()?;
        enc.Commit()?;
    }
    Ok(())
}
