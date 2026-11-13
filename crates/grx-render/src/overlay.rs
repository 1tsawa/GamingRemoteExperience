//! Calque d'interface (overlay) dessiné par Direct2D/DirectWrite directement sur le back buffer
//! DXGI, par-dessus la vidéo, juste avant `Present`.
//!
//! * L'intégration décrit le calque par une [`OverlayScene`] (liste de rectangles et de textes en
//!   pixels indépendants de la résolution, `scale` = facteur DPI) et la pousse avec
//!   [`crate::PresenterHandle::set_overlay`] depuis n'importe quel thread. Aucun état d'interface ici.
//! * Coût nul quand il n'y a pas de scène : aucun objet Direct2D n'est même créé avant la première.
//! * Direct2D partage le device D3D11 du présentateur (créé avec `BGRA_SUPPORT`, protégé
//!   multithread) ; fabrique `MULTI_THREADED` car le décodeur utilise le même device sur un autre thread.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::error::{Result, WinCtx};
use windows::core::{Interface, HSTRING};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface, IDXGISwapChain1};

/// Couleur RGBA linéaire 0..1 (alpha non prémultiplié).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::rgba(r, g, b, 1.0)
    }
    pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Self {
        Color { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a }
    }
    pub const fn with_alpha(self, a: f32) -> Self {
        Color { a, ..self }
    }
}

/// Rectangle en pixels indépendants (DIP) : multiplié par [`OverlayScene::scale`] au rendu.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { x, y, w, h }
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
    /// Rectangle réduit de `d` de chaque côté.
    pub fn inset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, (self.w - 2.0 * dx).max(0.0), (self.h - 2.0 * dy).max(0.0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Font {
    /// Segoe UI.
    Ui,
    /// Segoe UI semi-gras.
    UiBold,
    /// Consolas (statistiques, colonnes alignées).
    Mono,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OverlayItem {
    Fill { rect: Rect, radius: f32, color: Color },
    Stroke { rect: Rect, radius: f32, color: Color, width: f32 },
    /// Texte sur une ligne, centré verticalement dans `rect`, rogné à `rect`.
    Text { rect: Rect, text: String, size: f32, color: Color, align: Align, font: Font },
}

/// Ce qu'il faut dessiner par-dessus la vidéo.
#[derive(Clone, Debug, PartialEq)]
pub struct OverlayScene {
    /// Facteur DIP -> pixels physiques (DPI de la fenêtre).
    pub scale: f32,
    pub items: Vec<OverlayItem>,
}

impl OverlayScene {
    pub fn new(scale: f32) -> Self {
        OverlayScene { scale, items: Vec::new() }
    }
    pub fn fill(&mut self, rect: Rect, radius: f32, color: Color) {
        self.items.push(OverlayItem::Fill { rect, radius, color });
    }
    pub fn stroke(&mut self, rect: Rect, radius: f32, color: Color, width: f32) {
        self.items.push(OverlayItem::Stroke { rect, radius, color, width });
    }
    pub fn text(&mut self, rect: Rect, text: impl Into<String>, size: f32, color: Color, align: Align, font: Font) {
        self.items.push(OverlayItem::Text { rect, text: text.into(), size, color, align, font });
    }
}

/// Emplacement partagé entre la poignée (n'importe quel thread) et le rendu.
pub(crate) type OverlaySlot = Arc<Mutex<Option<Arc<OverlayScene>>>>;

// ---------------------------------------------------------------------------------------------
// Rendu Direct2D

struct D2d {
    ctx: ID2D1DeviceContext,
    dwrite: IDWriteFactory,
    brush: ID2D1SolidColorBrush,
    formats: HashMap<(Font, u32, Align), IDWriteTextFormat>,
    target: Option<ID2D1Bitmap1>,
}

impl D2d {
    fn new(device: &ID3D11Device) -> Result<Self> {
        let factory: ID2D1Factory1 =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, None) }.ctx("D2D1CreateFactory")?;
        let dxgi: IDXGIDevice = device.cast().ctx("IDXGIDevice")?;
        let d2d_dev = unsafe { factory.CreateDevice(&dxgi) }.ctx("ID2D1Factory1::CreateDevice")?;
        let ctx = unsafe { d2d_dev.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) }.ctx("CreateDeviceContext")?;
        unsafe { ctx.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE) };
        let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.ctx("DWriteCreateFactory")?;
        let white = D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        let brush = unsafe { ctx.CreateSolidColorBrush(&white, None) }.ctx("CreateSolidColorBrush")?;
        Ok(D2d { ctx, dwrite, brush, formats: HashMap::new(), target: None })
    }

    fn format(&mut self, font: Font, size: f32, align: Align) -> Result<IDWriteTextFormat> {
        let key = (font, (size * 4.0).round().clamp(4.0, 800.0) as u32, align);
        if let Some(f) = self.formats.get(&key) {
            return Ok(f.clone());
        }
        let (family, weight) = match font {
            Font::Ui => ("Segoe UI", DWRITE_FONT_WEIGHT_NORMAL),
            Font::UiBold => ("Segoe UI", DWRITE_FONT_WEIGHT_SEMI_BOLD),
            Font::Mono => ("Consolas", DWRITE_FONT_WEIGHT_NORMAL),
        };
        let f = unsafe {
            self.dwrite.CreateTextFormat(
                &HSTRING::from(family),
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                key.1 as f32 / 4.0,
                &HSTRING::from("fr-fr"),
            )
        }
        .ctx("CreateTextFormat")?;
        unsafe {
            let _ = f.SetTextAlignment(match align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
            let _ = f.SetTrimming(&trim, None);
        }
        if self.formats.len() > 64 {
            self.formats.clear();
        }
        self.formats.insert(key, f.clone());
        Ok(f)
    }

    fn ensure_target(&mut self, swapchain: &IDXGISwapChain1) -> Result<()> {
        if self.target.is_some() {
            return Ok(());
        }
        let surface: IDXGISurface = unsafe { swapchain.GetBuffer(0) }.ctx("GetBuffer(IDXGISurface)")?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let bmp = unsafe { self.ctx.CreateBitmapFromDxgiSurface(&surface, Some(&props)) }
            .ctx("CreateBitmapFromDxgiSurface")?;
        unsafe { self.ctx.SetTarget(&bmp) };
        self.target = Some(bmp);
        Ok(())
    }

    fn draw(&mut self, swapchain: &IDXGISwapChain1, scene: &OverlayScene) -> Result<()> {
        self.ensure_target(swapchain)?;
        let s = if scene.scale.is_finite() && scene.scale > 0.1 { scene.scale } else { 1.0 };
        let ctx = self.ctx.clone();
        let brush = self.brush.clone();
        unsafe {
            ctx.BeginDraw();
            ctx.SetTransform(&Matrix3x2 { M11: s, M12: 0.0, M21: 0.0, M22: s, M31: 0.0, M32: 0.0 });
        }
        let rr = |r: &Rect, radius: f32| D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F { left: r.x, top: r.y, right: r.x + r.w, bottom: r.y + r.h },
            radiusX: radius,
            radiusY: radius,
        };
        let set = |c: &Color| unsafe {
            let a = c.a.clamp(0.0, 1.0);
            brush.SetColor(&D2D1_COLOR_F { r: c.r, g: c.g, b: c.b, a });
        };
        for item in &scene.items {
            match item {
                OverlayItem::Fill { rect, radius, color } => {
                    set(color);
                    unsafe { ctx.FillRoundedRectangle(&rr(rect, *radius), &brush) };
                }
                OverlayItem::Stroke { rect, radius, color, width } => {
                    set(color);
                    unsafe { ctx.DrawRoundedRectangle(&rr(rect, *radius), &brush, *width, None) };
                }
                OverlayItem::Text { rect, text, size, color, align, font } => {
                    let fmt = self.format(*font, *size, *align)?;
                    set(color);
                    let utf16: Vec<u16> = text.encode_utf16().collect();
                    let r = D2D_RECT_F { left: rect.x, top: rect.y, right: rect.x + rect.w, bottom: rect.y + rect.h };
                    unsafe {
                        ctx.DrawText(&utf16, &fmt, &r, &brush, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL)
                    };
                }
            }
        }
        let r = unsafe { ctx.EndDraw(None, None) };
        if r.is_err() {
            // Cible perdue (redimensionnement, device) : recréée au prochain tour.
            self.release_target();
        }
        r.ctx("ID2D1DeviceContext::EndDraw")
    }

    fn release_target(&mut self) {
        if self.target.take().is_some() {
            unsafe { self.ctx.SetTarget(None) };
        }
    }
}

/// Calque attaché au `Gfx` du présentateur.
#[derive(Default)]
pub(crate) struct OverlayLayer {
    slot: Option<OverlaySlot>,
    d2d: Option<D2d>,
    failed: bool,
}

impl OverlayLayer {
    pub(crate) fn attach(&mut self, slot: OverlaySlot) {
        self.slot = Some(slot);
    }

    /// Dessine la scène courante sur le back buffer (rien s'il n'y en a pas).
    pub(crate) fn draw(&mut self, device: &ID3D11Device, swapchain: &IDXGISwapChain1) {
        if self.failed {
            return;
        }
        let Some(scene) = self.slot.as_ref().and_then(|s| s.lock().unwrap().clone()) else { return };
        if self.d2d.is_none() {
            match D2d::new(device) {
                Ok(d) => self.d2d = Some(d),
                Err(e) => {
                    eprintln!("grx-render: overlay indisponible : {e}");
                    self.failed = true;
                    return;
                }
            }
        }
        if let Some(d) = self.d2d.as_mut() {
            if let Err(e) = d.draw(swapchain, &scene) {
                eprintln!("grx-render: overlay : {e}");
            }
        }
    }

    /// À appeler avant `ResizeBuffers` (la cible Direct2D référence le back buffer).
    pub(crate) fn release_target(&mut self) {
        if let Some(d) = self.d2d.as_mut() {
            d.release_target();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_helpers() {
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert!(r.contains(10.0, 20.0) && r.contains(109.9, 69.9));
        assert!(!r.contains(110.0, 30.0) && !r.contains(9.9, 30.0));
        assert_eq!(r.inset(5.0, 10.0), Rect::new(15.0, 30.0, 90.0, 30.0));
        assert_eq!(r.inset(80.0, 80.0).w, 0.0);
        assert_eq!(Color::rgb(255, 0, 0).with_alpha(0.5), Color { r: 1.0, g: 0.0, b: 0.0, a: 0.5 });
    }
}
