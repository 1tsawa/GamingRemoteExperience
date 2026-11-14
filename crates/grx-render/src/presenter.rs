//! Fenêtre winit + swapchain DXGI flip-model (waitable object, tearing) alimentée par des
//! [`DecodedFrame`] NV12. Conversion couleur et mise à l'échelle par ID3D11VideoProcessor.
//!
//! Modèle de threads : [`Presenter::run`] occupe le thread appelant (boucle de messages Windows ;
//! en pratique le thread principal) et dessine. Un [`PresenterHandle`] (`Clone + Send + Sync`)
//! permet aux autres threads (réseau/décodage) de pousser des images et d'envoyer des commandes.
//! Les images arrivent dans une boîte aux lettres à une place : si le présentateur n'a pas fini
//! la précédente, la plus ancienne est jetée (« latest frame wins ») pour ne jamais accumuler de latence.

use crate::blit::{fit_rect, BlitColor, Blitter};
use crate::decoder::DecodedFrame;
use crate::error::{RenderError, Result, WinCtx};
use crate::gpu;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

pub use winit;

/// Paramètres de la fenêtre de rendu.
#[derive(Clone)]
pub struct PresenterConfig {
    pub title: String,
    /// Taille intérieure initiale (pixels physiques).
    pub width: u32,
    pub height: u32,
    /// Démarre en plein écran borderless.
    pub fullscreen: bool,
    /// `false` (défaut) : `Present(0, ALLOW_TEARING)` = latence minimale, sans VSync.
    pub vsync: bool,
    /// Bascule plein écran/fenêtré avec F11 (traité après le hook d'événements).
    pub f11_fullscreen: bool,
    /// Device D3D11 à utiliser (celui du décodeur). Par défaut : [`crate::create_device`].
    pub device: Option<ID3D11Device>,
}

impl Default for PresenterConfig {
    fn default() -> Self {
        PresenterConfig {
            title: "GamingRemoteExperience".into(),
            width: 1280,
            height: 720,
            fullscreen: false,
            vsync: false,
            f11_fullscreen: true,
            device: None,
        }
    }
}

/// Événement brut transmis au hook de l'intégration (clavier/souris/…, sans interprétation).
pub enum PresenterEvent<'a> {
    /// La fenêtre et la swapchain sont prêtes.
    Ready,
    /// Événement winit de la fenêtre (clavier, souris, focus, DPI, …).
    Window(&'a WindowEvent),
    /// Événement brut du périphérique (ex. `DeviceEvent::MouseMotion` pour la souris relative).
    Device(&'a DeviceEvent),
}

/// Statistiques de latence « image soumise -> Present » (en µs) depuis le dernier appel.
#[derive(Debug, Clone, Copy, Default)]
pub struct LatencyStats {
    pub count: usize,
    pub avg_us: u32,
    pub p99_us: u32,
    pub max_us: u32,
}

/// Compteurs du présentateur.
#[derive(Debug, Clone, Copy, Default)]
pub struct PresenterStats {
    /// Images effectivement présentées.
    pub presented: u64,
    /// Images remplacées dans la boîte aux lettres avant d'avoir été dessinées.
    pub dropped: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct Layout {
    win_w: u32,
    win_h: u32,
    vid_w: u32,
    vid_h: u32,
}

enum Command {
    SetFullscreen(bool),
    ToggleFullscreen,
    SetTitle(String),
    SetCursorGrab(bool),
    SetCursorVisible(bool),
    Close,
    /// Overlay : la scène a changé (redessiner si aucune image n'est en attente).
    OverlayChanged,
    SetVsync(bool),
    /// Curseur distant déplacé : re-présenter la dernière image sans attendre la vidéo.
    CursorChanged,
}

enum UserEvent {
    Wake,
    Cmd(Command),
}

struct Shared {
    device: ID3D11Device,
    mailbox: Mutex<Option<(DecodedFrame, Instant)>>,
    wake_pending: AtomicBool,
    proxy: EventLoopProxy<UserEvent>,
    presented: AtomicU64,
    dropped: AtomicU64,
    latencies: Mutex<Vec<u32>>,
    layout: Mutex<Layout>,
    closed: AtomicBool,
    overlay: crate::overlay::OverlaySlot,
    cursor: Arc<crate::cursor::CursorShared>,
}

// SAFETY : le device est protégé multithread (create_device) ; le reste de Shared est Sync.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// Poignée thread-safe vers le présentateur.
#[derive(Clone)]
pub struct PresenterHandle {
    shared: Arc<Shared>,
}

impl PresenterHandle {
    /// Device D3D11 du présentateur : y créer le décodeur ([`crate::VideoDecoder::with_device`]).
    pub fn device(&self) -> &ID3D11Device {
        &self.shared.device
    }

    /// Crée un décodeur sur le device du présentateur, depuis n'importe quel thread.
    pub fn create_decoder(&self, codec: grx_video::Codec) -> Result<crate::VideoDecoder> {
        crate::VideoDecoder::with_device(&self.shared.device, codec)
    }

    /// Soumet une image à afficher (remplace celle en attente). Renvoie `false` si la fenêtre est fermée.
    pub fn submit(&self, frame: DecodedFrame) -> bool {
        if self.shared.closed.load(Ordering::Acquire) {
            return false;
        }
        let old = self.shared.mailbox.lock().unwrap().replace((frame, Instant::now()));
        if old.is_some() {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
        drop(old);
        if !self.shared.wake_pending.swap(true, Ordering::AcqRel) {
            return self.shared.proxy.send_event(UserEvent::Wake).is_ok();
        }
        true
    }

    fn cmd(&self, c: Command) {
        let _ = self.shared.proxy.send_event(UserEvent::Cmd(c));
    }

    pub fn set_fullscreen(&self, on: bool) {
        self.cmd(Command::SetFullscreen(on));
    }
    pub fn toggle_fullscreen(&self) {
        self.cmd(Command::ToggleFullscreen);
    }
    pub fn set_title(&self, title: impl Into<String>) {
        self.cmd(Command::SetTitle(title.into()));
    }
    /// Confine le curseur à la fenêtre (utile pour la capture de souris).
    pub fn set_cursor_grab(&self, on: bool) {
        self.cmd(Command::SetCursorGrab(on));
    }
    pub fn set_cursor_visible(&self, on: bool) {
        self.cmd(Command::SetCursorVisible(on));
    }
    /// Ferme la fenêtre et termine [`Presenter::run`].
    pub fn close(&self) {
        self.cmd(Command::Close);
    }
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Calque d'interface dessiné par-dessus la vidéo (voir [`crate::OverlayScene`]) ; `None`
    /// le retire (aucun coût de rendu). Sans effet si la scène est identique à la courante.
    pub fn set_overlay(&self, scene: Option<crate::OverlayScene>) {
        let mut cur = self.shared.overlay.lock().unwrap();
        if cur.as_deref() == scene.as_ref() {
            return;
        }
        *cur = scene.map(Arc::new);
        drop(cur);
        self.cmd(Command::OverlayChanged);
    }

    /// Curseur distant dessiné par-dessus la vidéo (`None` : aucun). Le présentateur re-présente
    /// aussitôt la dernière image avec le curseur à jour (mises à jour coalescées).
    pub fn set_remote_cursor(&self, c: Option<crate::RemoteCursor>) {
        if self.shared.cursor.set(c) {
            self.cmd(Command::CursorChanged);
        }
    }

    /// Latences « mise à jour du curseur sur le host -> dessin » depuis le dernier appel
    /// (horloges host/client supposées synchrones).
    pub fn take_cursor_latency_stats(&self) -> LatencyStats {
        let mut v = std::mem::take(&mut *self.shared.cursor.latencies.lock().unwrap());
        if v.is_empty() {
            return LatencyStats::default();
        }
        v.sort_unstable();
        let n = v.len();
        LatencyStats {
            count: n,
            avg_us: (v.iter().map(|&x| x as u64).sum::<u64>() / n as u64) as u32,
            p99_us: v[(n * 99 / 100).min(n - 1)],
            max_us: v[n - 1],
        }
    }

    /// Active/désactive la synchronisation verticale à chaud.
    pub fn set_vsync(&self, on: bool) {
        self.cmd(Command::SetVsync(on));
    }

    pub fn stats(&self) -> PresenterStats {
        PresenterStats {
            presented: self.shared.presented.load(Ordering::Relaxed),
            dropped: self.shared.dropped.load(Ordering::Relaxed),
        }
    }

    /// Statistiques de latence soumission -> Present depuis le dernier appel (puis remise à zéro).
    pub fn take_latency_stats(&self) -> LatencyStats {
        let mut v = std::mem::take(&mut *self.shared.latencies.lock().unwrap());
        if v.is_empty() {
            return LatencyStats::default();
        }
        v.sort_unstable();
        let n = v.len();
        LatencyStats {
            count: n,
            avg_us: (v.iter().map(|&x| x as u64).sum::<u64>() / n as u64) as u32,
            p99_us: v[(n * 99 / 100).min(n - 1)],
            max_us: v[n - 1],
        }
    }

    /// Convertit une position en pixels dans la fenêtre (`PhysicalPosition`) en coordonnées
    /// normalisées `[0,1]` dans l'image vidéo affichée (bandes noires exclues). `None` si le
    /// point est hors de l'image ou si aucune image n'a encore été affichée.
    pub fn window_to_video(&self, x: f64, y: f64) -> Option<(f32, f32)> {
        let l = *self.shared.layout.lock().unwrap();
        map_window_to_video(l, x, y)
    }

    /// Rectangle `(x, y, w, h)` de l'image dans la fenêtre (pixels), bandes noires exclues.
    pub fn video_rect(&self) -> Option<(u32, u32, u32, u32)> {
        let l = *self.shared.layout.lock().unwrap();
        if l.vid_w == 0 || l.win_w == 0 {
            return None;
        }
        Some(fit_rect(l.vid_w, l.vid_h, l.win_w, l.win_h))
    }
}

fn map_window_to_video(l: Layout, x: f64, y: f64) -> Option<(f32, f32)> {
    if l.vid_w == 0 || l.win_w == 0 {
        return None;
    }
    let (rx, ry, rw, rh) = fit_rect(l.vid_w, l.vid_h, l.win_w, l.win_h);
    let (u, v) = ((x - rx as f64) / rw as f64, (y - ry as f64) / rh as f64);
    if (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v) {
        Some((u as f32, v as f32))
    } else {
        None
    }
}

/// Fenêtre + boucle d'événements. Créer sur le thread qui fera tourner [`Presenter::run`].
pub struct Presenter {
    event_loop: EventLoop<UserEvent>,
    shared: Arc<Shared>,
    cfg: PresenterConfig,
}

impl Presenter {
    /// Prépare la boucle d'événements et le device GPU ; la fenêtre apparaît dans [`Presenter::run`].
    pub fn new(mut cfg: PresenterConfig) -> Result<Self> {
        // Ce thread est le thread de fenêtre : winit y initialise OLE (STA), donc pas de MTA ici.
        gpu::mark_ui_thread();
        let device = match cfg.device.take() {
            Some(d) => d,
            None => gpu::create_device()?,
        };
        let event_loop = EventLoop::<UserEvent>::with_user_event()
            .build()
            .map_err(|e| RenderError::Window(e.to_string()))?;
        let shared = Arc::new(Shared {
            device,
            mailbox: Mutex::new(None),
            wake_pending: AtomicBool::new(false),
            proxy: event_loop.create_proxy(),
            presented: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            latencies: Mutex::new(Vec::new()),
            layout: Mutex::new(Layout::default()),
            closed: AtomicBool::new(false),
            overlay: Default::default(),
            cursor: Default::default(),
        });
        Ok(Presenter { event_loop, shared, cfg })
    }

    /// Poignée utilisable depuis n'importe quel thread, dès maintenant (les images soumises avant
    /// l'apparition de la fenêtre attendent dans la boîte aux lettres).
    pub fn handle(&self) -> PresenterHandle {
        PresenterHandle { shared: self.shared.clone() }
    }

    /// Device D3D11 partagé (à donner à [`crate::VideoDecoder::with_device`]).
    pub fn device(&self) -> &ID3D11Device {
        &self.shared.device
    }

    /// Boucle d'événements (bloquante) : crée la fenêtre, dessine, retourne à la fermeture.
    ///
    /// `hook` reçoit tous les événements bruts winit **sur ce thread** (copiez ce dont vous avez
    /// besoin dans un canal `mpsc` pour le traiter ailleurs) ainsi que la `Window`. Retourner `true`
    /// signifie « consommé » : le traitement interne de la touche F11 est alors sauté.
    pub fn run<F>(self, hook: F) -> Result<()>
    where
        F: FnMut(&PresenterEvent, &Window) -> bool + 'static,
    {
        unsafe { timeBeginPeriod(1) };
        let mut app = App {
            shared: self.shared.clone(),
            cfg: self.cfg,
            hook,
            window: None,
            gfx: None,
            last: None,
            error: None,
            retry: false,
            cursor_retry: false,
        };
        let r = self.event_loop.run_app(&mut app);
        self.shared.closed.store(true, Ordering::Release);
        self.shared.mailbox.lock().unwrap().take();
        unsafe { timeEndPeriod(1) };
        r.map_err(|e| RenderError::Window(e.to_string()))?;
        match app.error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// [`Presenter::run`] sans hook.
    pub fn run_simple(self) -> Result<()> {
        self.run(|_, _| false)
    }
}

// ---------------------------------------------------------------------------------------------

struct Gfx {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain1,
    waitable: HANDLE,
    backbuffer: Option<ID3D11Texture2D>,
    w: u32,
    h: u32,
    flags: u32,
    tearing: bool,
    vsync: bool,
    blitter: Blitter,
    overlay: crate::overlay::OverlayLayer,
    cursor: crate::cursor::CursorLayer,
    /// Flux HDR10 : conversion PQ -> back buffer (tone-mapping SDR ou scRGB).
    hdr: Option<crate::hdr_present::HdrPresent>,
    /// Back buffer FP16 scRGB (écran HDR) au lieu de BGRA 8 bits.
    hdr_out: bool,
    /// Dernière vérification « l'écran de la fenêtre est-il en HDR ? ».
    hdr_check: Option<(Instant, bool)>,
}

impl Gfx {
    fn new(device: &ID3D11Device, hwnd: HWND, w: u32, h: u32, vsync: bool) -> Result<Self> {
        let factory = gpu::factory()?;
        let mut allow: BOOL = BOOL(0);
        if let Ok(f5) = factory.cast::<IDXGIFactory5>() {
            let ok = unsafe {
                f5.CheckFeatureSupport(
                    DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                    &mut allow as *mut BOOL as *mut c_void,
                    std::mem::size_of::<BOOL>() as u32,
                )
            };
            if ok.is_err() {
                allow = BOOL(0);
            }
        }
        let tearing = allow.as_bool();
        let flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32
            | if tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 } else { 0 };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: w.max(1),
            Height: h.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: BOOL(0),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: flags,
        };
        let swapchain = unsafe { factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None) }
            .ctx("CreateSwapChainForHwnd")?;
        unsafe { factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) }.ctx("MakeWindowAssociation")?;
        let sc2: IDXGISwapChain2 = swapchain.cast().ctx("IDXGISwapChain2")?;
        unsafe { sc2.SetMaximumFrameLatency(1) }.ctx("SetMaximumFrameLatency")?;
        let waitable = unsafe { sc2.GetFrameLatencyWaitableObject() };
        let context = unsafe { device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        let mut g = Gfx {
            device: device.clone(),
            context,
            swapchain,
            waitable,
            backbuffer: None,
            w: w.max(1),
            h: h.max(1),
            flags,
            tearing,
            vsync,
            blitter: Blitter::new(device)?,
            overlay: Default::default(),
            cursor: Default::default(),
            hdr: None,
            hdr_out: false,
            hdr_check: None,
        };
        g.fetch_backbuffer()?;
        if std::env::var("GRX_RENDER_DEBUG").is_ok() {
            eprintln!("grx-render: swapchain {}x{} tearing={} vsync={}", g.w, g.h, g.tearing, g.vsync);
        }
        Ok(g)
    }

    fn fetch_backbuffer(&mut self) -> Result<()> {
        self.backbuffer = Some(unsafe { self.swapchain.GetBuffer::<ID3D11Texture2D>(0) }.ctx("GetBuffer(0)")?);
        Ok(())
    }

    fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        if w == 0 || h == 0 || (w == self.w && h == self.h) {
            return Ok(());
        }
        self.rebuild(w, h, DXGI_FORMAT_UNKNOWN)
    }

    /// Recrée les tampons de la swapchain (taille et/ou format) après avoir relâché les vues.
    fn rebuild(&mut self, w: u32, h: u32, format: DXGI_FORMAT) -> Result<()> {
        self.blitter.release_output();
        self.overlay.release_target();
        self.cursor.release_target();
        if let Some(h) = &mut self.hdr {
            h.release_target();
        }
        self.backbuffer = None;
        unsafe { self.context.ClearState() };
        unsafe { self.context.Flush() };
        unsafe { self.swapchain.ResizeBuffers(0, w, h, format, DXGI_SWAP_CHAIN_FLAG(self.flags as i32)) }
            .ctx("ResizeBuffers")?;
        self.w = w;
        self.h = h;
        self.fetch_backbuffer()
    }

    /// L'écran qui affiche la fenêtre est-il en mode HDR (PQ BT.2020) ? Vérifié toutes les 2 s.
    fn display_is_hdr(&mut self) -> bool {
        if let Some((t, v)) = self.hdr_check {
            if t.elapsed() < Duration::from_secs(2) {
                return v;
            }
        }
        let v = (|| -> windows::core::Result<bool> {
            let o = unsafe { self.swapchain.GetContainingOutput() }?;
            let o6: IDXGIOutput6 = o.cast()?;
            Ok(unsafe { o6.GetDesc1() }?.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
        })()
        .unwrap_or(false)
            && std::env::var_os("GRX_FORCE_SDR_PRESENT").is_none();
        self.hdr_check = Some((Instant::now(), v));
        v
    }

    /// Bascule le back buffer FP16 scRGB (écran HDR) <-> BGRA 8 bits.
    fn set_hdr_out(&mut self, on: bool) -> Result<()> {
        if on == self.hdr_out {
            return Ok(());
        }
        let fmt = if on { DXGI_FORMAT_R16G16B16A16_FLOAT } else { DXGI_FORMAT_B8G8R8A8_UNORM };
        self.rebuild(self.w, self.h, fmt)?;
        let cs = if on { DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709 } else { DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 };
        if let Ok(sc3) = self.swapchain.cast::<IDXGISwapChain3>() {
            unsafe { sc3.SetColorSpace1(cs) }.ctx("SetColorSpace1")?;
        }
        self.hdr_out = on;
        if std::env::var("GRX_RENDER_DEBUG").is_ok() {
            eprintln!("grx-render: back buffer {}", if on { "FP16 scRGB (HDR)" } else { "BGRA 8 bits (SDR)" });
        }
        Ok(())
    }

    /// Attend que la swapchain accepte une nouvelle image (frame latency waitable).
    fn wait_ready(&self, timeout_ms: u32) -> bool {
        unsafe { WaitForSingleObjectEx(self.waitable, timeout_ms, true) == WAIT_OBJECT_0 }
    }

    fn render(&mut self, f: &DecodedFrame) -> Result<()> {
        let dev = unsafe { f.texture.GetDevice() }.ctx("ID3D11Texture2D::GetDevice")?;
        if dev.as_raw() != self.device.as_raw() {
            return Err(RenderError::Other(
                "frame belongs to another D3D11 device: create the decoder with VideoDecoder::with_device(presenter.device(), ..)"
                    .into(),
            ));
        }
        if f.hdr {
            // HDR10 : YCbCr PQ -> RGB PQ (VideoProcessor) -> tone-mapping SDR ou scRGB (écran HDR).
            let want = self.display_is_hdr();
            self.set_hdr_out(want)?;
            if self.hdr.is_none() {
                self.hdr = Some(crate::hdr_present::HdrPresent::new(&self.device)?);
            }
            let (w, h) = (self.w, self.h);
            let inter = self.hdr.as_mut().unwrap().intermediate(w, h)?;
            self.blitter.blit(&f.texture, f.subresource, f.width, f.height, &inter, w, h, BlitColor::Pq)?;
            let bb = self.backbuffer.clone().ok_or_else(|| RenderError::Other("no back buffer".into()))?;
            self.hdr.as_mut().unwrap().draw(&bb, w, h, self.hdr_out)?;
        } else {
            self.set_hdr_out(false)?;
            let bb = self.backbuffer.clone().ok_or_else(|| RenderError::Other("no back buffer".into()))?;
            self.blitter.blit(&f.texture, f.subresource, f.width, f.height, &bb, self.w, self.h, BlitColor::Sdr)?;
        }
        let bb = self.backbuffer.clone().ok_or_else(|| RenderError::Other("no back buffer".into()))?;
        self.cursor.draw(&self.device, &self.context, &bb, self.w, self.h, f.width, f.height);
        self.overlay.draw(&self.device, &self.swapchain);
        let (interval, flags) = if self.vsync {
            (1, DXGI_PRESENT(0))
        } else if self.tearing {
            (0, DXGI_PRESENT_ALLOW_TEARING)
        } else {
            (0, DXGI_PRESENT(0))
        };
        let hr = unsafe { self.swapchain.Present(interval, flags) };
        // DXGI_STATUS_OCCLUDED (succès) quand la fenêtre est masquée : rien à faire.
        hr.ok().ctx("Present")
    }
}

struct App<F> {
    shared: Arc<Shared>,
    cfg: PresenterConfig,
    hook: F,
    window: Option<Window>,
    gfx: Option<Gfx>,
    /// Dernière image dessinée, conservée pour redessiner lors d'un redimensionnement.
    last: Option<DecodedFrame>,
    error: Option<RenderError>,
    retry: bool,
    /// Redessin du curseur reporté (swapchain pas prête).
    cursor_retry: bool,
}

impl<F: FnMut(&PresenterEvent, &Window) -> bool + 'static> App<F> {
    fn fail(&mut self, el: &ActiveEventLoop, e: RenderError) {
        self.error = Some(e);
        el.exit();
    }

    fn update_layout(&self) {
        if let Some(g) = &self.gfx {
            let mut l = self.shared.layout.lock().unwrap();
            l.win_w = g.w;
            l.win_h = g.h;
            if let Some(f) = &self.last {
                l.vid_w = f.width;
                l.vid_h = f.height;
            }
        }
    }

    /// Dessine la dernière image en attente dans la boîte aux lettres.
    fn pump(&mut self) {
        self.shared.wake_pending.store(false, Ordering::Release);
        self.retry = false;
        if self.shared.mailbox.lock().unwrap().is_none() {
            return;
        }
        let Some(gfx) = self.gfx.as_mut() else { return };
        // On attend la disponibilité de la swapchain AVANT de prendre l'image, pour afficher la plus récente.
        if !gfx.wait_ready(8) {
            self.retry = true;
            return;
        }
        let Some((frame, t0)) = self.shared.mailbox.lock().unwrap().take() else { return };
        match gfx.render(&frame) {
            Ok(()) => {
                self.shared.presented.fetch_add(1, Ordering::Relaxed);
                let us = t0.elapsed().as_micros().min(u32::MAX as u128) as u32;
                let mut l = self.shared.latencies.lock().unwrap();
                if l.len() < 65_536 {
                    l.push(us);
                }
            }
            Err(e) => eprintln!("grx-render: render error: {e}"),
        }
        self.last = Some(frame);
        self.update_layout();
    }

    /// Re-présente la dernière image pour le curseur, sans bloquer la boucle de messages.
    fn cursor_redraw(&mut self) {
        self.cursor_retry = false;
        if self.shared.mailbox.lock().unwrap().is_some() {
            return; // `pump` dessinera l'image en attente, curseur compris
        }
        match self.gfx.as_ref() {
            Some(g) if self.last.is_some() => {
                if !g.wait_ready(0) {
                    self.cursor_retry = true;
                    return;
                }
            }
            _ => return,
        }
        self.redraw_last();
    }

    fn redraw_last(&mut self) {
        if let (Some(g), Some(f)) = (self.gfx.as_mut(), self.last.as_ref()) {
            if let Err(e) = g.render(f) {
                eprintln!("grx-render: redraw error: {e}");
            }
        }
    }

    fn toggle_fullscreen(&self) {
        if let Some(w) = &self.window {
            let on = w.fullscreen().is_none();
            self.set_fullscreen(on);
        }
    }

    fn set_fullscreen(&self, on: bool) {
        if let Some(w) = &self.window {
            w.set_fullscreen(if on { Some(Fullscreen::Borderless(None)) } else { None });
        }
    }
}

impl<F: FnMut(&PresenterEvent, &Window) -> bool + 'static> ApplicationHandler<UserEvent> for App<F> {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(self.cfg.title.clone())
            .with_inner_size(PhysicalSize::new(self.cfg.width, self.cfg.height))
            .with_fullscreen(if self.cfg.fullscreen { Some(Fullscreen::Borderless(None)) } else { None });
        let window = match el.create_window(attrs) {
            Ok(w) => w,
            Err(e) => return self.fail(el, RenderError::Window(e.to_string())),
        };
        let hwnd = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => HWND(h.hwnd.get() as *mut c_void),
            _ => return self.fail(el, RenderError::Window("no Win32 window handle".into())),
        };
        let size = window.inner_size();
        let mut gfx = match Gfx::new(&self.shared.device, hwnd, size.width, size.height, self.cfg.vsync) {
            Ok(g) => g,
            Err(e) => return self.fail(el, e),
        };
        gfx.overlay.attach(self.shared.overlay.clone());
        gfx.cursor.attach(self.shared.cursor.clone());
        self.gfx = Some(gfx);
        self.update_layout();
        (self.hook)(&PresenterEvent::Ready, &window);
        self.window = Some(window);
        // Des images ont pu arriver avant la fenêtre.
        self.pump();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let consumed = match &self.window {
            Some(w) => (self.hook)(&PresenterEvent::Window(&event), w),
            None => false,
        };
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(sz) => {
                if let Some(g) = self.gfx.as_mut() {
                    if let Err(e) = g.resize(sz.width, sz.height) {
                        return self.fail(el, e);
                    }
                    self.update_layout();
                    self.redraw_last();
                }
            }
            WindowEvent::RedrawRequested => self.redraw_last(),
            WindowEvent::KeyboardInput { event: ke, .. } => {
                if !consumed
                    && self.cfg.f11_fullscreen
                    && ke.state == ElementState::Pressed
                    && !ke.repeat
                    && ke.physical_key == PhysicalKey::Code(KeyCode::F11)
                {
                    self.toggle_fullscreen();
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let Some(w) = &self.window {
            (self.hook)(&PresenterEvent::Device(&event), w);
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Wake => self.pump(),
            UserEvent::Cmd(c) => match c {
                Command::SetFullscreen(on) => self.set_fullscreen(on),
                Command::ToggleFullscreen => self.toggle_fullscreen(),
                Command::SetTitle(t) => {
                    if let Some(w) = &self.window {
                        w.set_title(&t);
                    }
                }
                Command::SetCursorGrab(on) => {
                    if let Some(w) = &self.window {
                        let r = if on {
                            w.set_cursor_grab(CursorGrabMode::Confined).or_else(|_| w.set_cursor_grab(CursorGrabMode::Locked))
                        } else {
                            w.set_cursor_grab(CursorGrabMode::None)
                        };
                        if let Err(e) = r {
                            eprintln!("grx-render: set_cursor_grab: {e}");
                        }
                    }
                }
                Command::SetCursorVisible(on) => {
                    if let Some(w) = &self.window {
                        w.set_cursor_visible(on);
                    }
                }
                Command::Close => el.exit(),
                Command::OverlayChanged => {
                    // Une image en attente sera dessinée (avec le calque) par `pump` ; sinon on
                    // redessine la dernière pour que le calque apparaisse même sur un écran statique.
                    if self.shared.mailbox.lock().unwrap().is_none() {
                        self.redraw_last();
                    }
                }
                Command::CursorChanged => {
                    self.shared.cursor.clear_pending();
                    self.cursor_redraw();
                }
                Command::SetVsync(on) => {
                    if let Some(g) = self.gfx.as_mut() {
                        g.vsync = on;
                    }
                }
            },
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.retry {
            // La swapchain n'était pas prête : on réessaie sans bloquer la boucle de messages.
            self.pump();
        }
        if self.cursor_retry {
            self.cursor_redraw();
        }
        el.set_control_flow(if self.retry || self.cursor_retry {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(1))
        } else {
            ControlFlow::Wait
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_window_points_to_video() {
        let l = Layout { win_w: 1000, win_h: 1000, vid_w: 1920, vid_h: 1080 };
        // Image 1000x563 centrée verticalement (y de 218 à 781).
        let (u, v) = map_window_to_video(l, 500.0, 500.0).unwrap();
        assert!((u - 0.5).abs() < 0.01 && (v - 0.5).abs() < 0.01);
        assert_eq!(map_window_to_video(l, 500.0, 100.0), None);
        assert_eq!(map_window_to_video(l, 0.0, 218.0), Some((0.0, 0.0)));
        assert_eq!(map_window_to_video(Layout::default(), 1.0, 1.0), None);
    }
}
