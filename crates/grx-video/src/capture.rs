//! Capture d'écran via DXGI Desktop Duplication, sortie en texture D3D11 (GPU).

use crate::cursor::{convert_pointer_shape, CursorTracker};
use crate::error::{Result, VideoError, WinCtx};
use crate::gpu;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::{E_ACCESSDENIED, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::*;

const RING_SIZE: usize = 3;

/// Description d'une sortie (moniteur) disponible.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub adapter_index: u32,
    pub output_index: u32,
    pub adapter_name: String,
    pub vendor_id: u32,
    pub width: u32,
    pub height: u32,
    /// Coin supérieur gauche dans le bureau virtuel (pixels physiques, peut être négatif).
    pub left: i32,
    pub top: i32,
    pub attached_to_desktop: bool,
    /// Moniteur dont le coin supérieur gauche est en (0,0).
    pub primary: bool,
    /// Nom GDI de la sortie (`\\.\DISPLAY1`), identifiant stable tant que la configuration ne change pas.
    pub device_name: String,
    /// Nom convivial du moniteur (DisplayConfig), vide si inconnu.
    pub friendly_name: String,
    /// Sortie indirecte (écran virtuel IddCx : Parsec VDD, écrans « fantômes »…).
    pub is_virtual: bool,
}

impl OutputInfo {
    /// Nom à afficher : nom du moniteur, sinon `DISPLAYn`.
    pub fn display_name(&self) -> String {
        if !self.friendly_name.is_empty() {
            return self.friendly_name.clone();
        }
        self.device_name.trim_start_matches(r"\\.\").to_string()
    }
}

/// Écrans capturables, dans l'ordre présenté à l'utilisateur : sorties rattachées au bureau
/// (les sorties inactives/déconnectées sont exclues), de surface non nulle, une seule fois par
/// nom GDI, triées de gauche à droite puis de haut en bas.
pub fn capturable(outs: &[OutputInfo]) -> Vec<OutputInfo> {
    let mut v: Vec<OutputInfo> = Vec::new();
    for o in outs.iter().filter(|o| o.attached_to_desktop && o.width > 0 && o.height > 0) {
        if !o.device_name.is_empty() && v.iter().any(|x| x.device_name == o.device_name) {
            continue;
        }
        v.push(o.clone());
    }
    v.sort_by_key(|o| (o.left, o.top, o.adapter_index, o.output_index));
    v
}

/// Nom de moniteur typique d'un pilote d'écran virtuel (IddCx : « VDD by MTT », Parsec VDA,
/// IddSampleDriver…), dont la technologie de sortie n'est pas toujours déclarée « indirecte ».
pub fn looks_virtual(friendly: &str) -> bool {
    let f = friendly.to_ascii_lowercase();
    ["vdd", "virtual", "parsec", "idd", "dummy", "spacedesk"].iter().any(|k| f.contains(k))
}

/// [`list_outputs`] filtré par [`capturable`].
pub fn list_displays() -> Result<Vec<OutputInfo>> {
    Ok(capturable(&list_outputs()?))
}

/// Une image capturée. La texture appartient à un ring interne de 3 textures :
/// elle reste valide jusqu'à 2 captures ultérieures ; au-delà elle est réécrite.
#[derive(Clone)]
pub struct CapturedFrame {
    /// Texture BGRA (ou le format natif du bureau) sur le device du capturer.
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    /// Instant où `AcquireNextFrame` a rendu la main (début de la mesure de latence).
    pub captured_at: Instant,
    /// Compteur monotone d'images capturées.
    pub seq: u64,
    /// Nombre d'updates du bureau accumulées depuis la dernière capture (DXGI `AccumulatedFrames`).
    pub accumulated_frames: u32,
}

// SAFETY : device D3D11 en mode multithread-protected, COM en MTA ; usage exclusif via &mut self.
unsafe impl Send for ScreenCapturer {}
unsafe impl Send for CapturedFrame {}

pub struct ScreenCapturer {
    adapter_index: u32,
    output_index: u32,
    adapter: IDXGIAdapter1,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dupl: Option<IDXGIOutputDuplication>,
    width: u32,
    height: u32,
    /// Position de la sortie dans le bureau virtuel (lue à chaque ouverture de la duplication).
    origin: (i32, i32),
    format: DXGI_FORMAT,
    ring: Vec<ID3D11Texture2D>,
    ring_pos: usize,
    have_image: bool,
    seq: u64,
    last_idx: Option<usize>,
    /// Nombre de fois où la duplication a dû être recréée (perte d'accès, changement de mode).
    pub reopen_count: u32,
    /// Destination des mises à jour du curseur (position/visibilité/forme), si demandée.
    cursor: Option<Arc<CursorTracker>>,
}

/// Liste les sorties de tous les adaptateurs matériels.
pub fn list_outputs() -> Result<Vec<OutputInfo>> {
    let f = gpu::factory()?;
    let names = crate::displayconfig::monitor_names();
    let mut v = Vec::new();
    let mut ai = 0;
    while let Ok(a) = unsafe { f.EnumAdapters1(ai) } {
        let info = gpu::adapter_desc(&a)?;
        let mut oi = 0;
        while let Ok(o) = unsafe { a.EnumOutputs(oi) } {
            let d = unsafe { o.GetDesc() }.ctx("IDXGIOutput::GetDesc")?;
            let r: RECT = d.DesktopCoordinates;
            let len = d.DeviceName.iter().position(|&c| c == 0).unwrap_or(d.DeviceName.len());
            let device_name = String::from_utf16_lossy(&d.DeviceName[..len]);
            let mon = names.get(&device_name).cloned().unwrap_or_default();
            v.push(OutputInfo {
                adapter_index: ai,
                output_index: oi,
                adapter_name: info.name.clone(),
                vendor_id: info.vendor_id,
                width: (r.right - r.left).max(0) as u32,
                height: (r.bottom - r.top).max(0) as u32,
                left: r.left,
                top: r.top,
                attached_to_desktop: d.AttachedToDesktop.as_bool(),
                primary: r.left == 0 && r.top == 0 && d.AttachedToDesktop.as_bool(),
                device_name,
                is_virtual: mon.is_virtual || looks_virtual(&mon.friendly),
                friendly_name: mon.friendly,
            });
            oi += 1;
        }
        ai += 1;
    }
    Ok(v)
}

fn is_transient(e: &windows::core::Error) -> bool {
    let c = e.code();
    c == DXGI_ERROR_ACCESS_LOST
        || c == E_ACCESSDENIED // desktop sécurisé (UAC, Ctrl+Alt+Suppr)
        || c == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE
        || c == DXGI_ERROR_SESSION_DISCONNECTED
        || c == DXGI_ERROR_DEVICE_REMOVED
}

impl ScreenCapturer {
    /// Capture le moniteur principal sur l'adaptateur NVIDIA (sinon le premier adaptateur matériel).
    pub fn new() -> Result<Self> {
        let outs = list_outputs()?;
        let best = outs
            .iter()
            .filter(|o| o.attached_to_desktop)
            .max_by_key(|o| (o.vendor_id == gpu::VENDOR_NVIDIA, o.primary))
            .ok_or(VideoError::NoOutput)?;
        Self::with_output(best.adapter_index, best.output_index)
    }

    /// Capture une sortie précise (voir [`list_outputs`]).
    pub fn with_output(adapter_index: u32, output_index: u32) -> Result<Self> {
        gpu::init_com();
        let f = gpu::factory()?;
        let adapter = unsafe { f.EnumAdapters1(adapter_index) }.ctx("EnumAdapters1")?;
        let (device, context) = gpu::create_device_for_adapter(&adapter)?;
        let mut c = ScreenCapturer {
            adapter_index,
            output_index,
            adapter,
            device,
            context,
            dupl: None,
            width: 0,
            height: 0,
            origin: (0, 0),
            format: DXGI_FORMAT(0),
            ring: Vec::new(),
            ring_pos: 0,
            have_image: false,
            seq: 0,
            last_idx: None,
            reopen_count: 0,
            cursor: None,
        };
        c.reopen()?;
        Ok(c)
    }

    /// Device D3D11 sur lequel vivent les textures capturées ; à passer à [`crate::Encoder::with_device`].
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Publie le curseur (position, visibilité, forme) dans `t` à chaque appel de [`Self::capture`],
    /// y compris pour les mises à jour « curseur seul » sans nouvelle image du bureau.
    pub fn set_cursor_tracker(&mut self, t: Arc<CursorTracker>) {
        self.cursor = Some(t);
    }

    /// Lit position et forme du curseur dans l'image DXGI acquise (avant `ReleaseFrame`).
    fn read_pointer(&self, dupl: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) {
        let Some(t) = &self.cursor else { return };
        let pos = (info.LastMouseUpdateTime != 0).then(|| {
            let p = info.PointerPosition;
            (p.Visible.as_bool(), p.Position.x, p.Position.y)
        });
        let mut shape = None;
        let size = info.PointerShapeBufferSize;
        if size > 0 && size <= 16 << 20 {
            let mut buf = vec![0u8; size as usize];
            let mut needed = 0u32;
            let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            let r = unsafe { dupl.GetFramePointerShape(size, buf.as_mut_ptr() as *mut _, &mut needed, &mut si) };
            if r.is_ok() {
                buf.truncate(needed.min(size) as usize);
                shape = convert_pointer_shape(si.Type, si.Width, si.Height, si.Pitch, si.HotSpot.x.max(0) as u32, si.HotSpot.y.max(0) as u32, &buf);
            }
        }
        if pos.is_some() || shape.is_some() || self.width != 0 {
            t.update(pos, shape, self.width, self.height);
        }
    }

    /// Résolution courante du bureau capturé.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn adapter_output(&self) -> (u32, u32) {
        (self.adapter_index, self.output_index)
    }

    /// Coin supérieur gauche de la sortie capturée dans le bureau virtuel.
    pub fn origin(&self) -> (i32, i32) {
        self.origin
    }

    /// Bascule à chaud vers une autre sortie du MÊME adaptateur : le device D3D11 (et donc
    /// l'encodeur qui l'utilise) est conservé. En cas d'échec, la sortie précédente est rouverte.
    /// La prochaine image peut avoir une autre taille (l'encodeur la met à l'échelle).
    pub fn switch_output(&mut self, output_index: u32) -> Result<()> {
        let prev = self.output_index;
        self.output_index = output_index;
        self.ring.clear();
        self.last_idx = None;
        match self.reopen() {
            Ok(()) => Ok(()),
            Err(e) => {
                self.output_index = prev;
                let _ = self.reopen();
                Err(e)
            }
        }
    }

    fn reopen(&mut self) -> Result<()> {
        self.dupl = None;
        self.have_image = false;
        let output = unsafe { self.adapter.EnumOutputs(self.output_index) }.ctx("EnumOutputs")?;
        let od = unsafe { output.GetDesc() }.ctx("IDXGIOutput::GetDesc")?;
        let output1: IDXGIOutput1 = output.cast().ctx("IDXGIOutput1")?;
        let dupl = unsafe { output1.DuplicateOutput(&self.device) }.ctx("DuplicateOutput")?;
        let d = unsafe { dupl.GetDesc() };
        self.width = d.ModeDesc.Width;
        self.height = d.ModeDesc.Height;
        self.origin = (od.DesktopCoordinates.left, od.DesktopCoordinates.top);
        self.dupl = Some(dupl);
        self.reopen_count += 1;
        Ok(())
    }

    fn ensure_ring(&mut self, src: &ID3D11Texture2D) -> Result<()> {
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut sd) };
        if !self.ring.is_empty() && sd.Width == self.width && sd.Height == self.height && sd.Format == self.format {
            return Ok(());
        }
        self.width = sd.Width;
        self.height = sd.Height;
        self.format = sd.Format;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: sd.Width,
            Height: sd.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: sd.Format,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        self.ring.clear();
        self.last_idx = None;
        for _ in 0..RING_SIZE {
            let mut t = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut t)) }.ctx("CreateTexture2D(ring)")?;
            self.ring.push(t.unwrap());
        }
        Ok(())
    }

    /// Attend la prochaine image du bureau (max `timeout_ms`). `Ok(None)` si rien n'a changé
    /// (écran statique) ou si le bureau est temporairement inaccessible (UAC, verrouillage).
    ///
    /// Gère automatiquement `DXGI_ERROR_ACCESS_LOST` (changement de résolution, bascule de
    /// desktop sécurisé) en recréant la duplication ; la taille de la frame suivante reflète
    /// la nouvelle résolution.
    pub fn capture(&mut self, timeout_ms: u32) -> Result<Option<CapturedFrame>> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if self.dupl.is_none() {
                match self.reopen() {
                    Ok(()) => {}
                    Err(VideoError::Win { source, .. }) if is_transient(&source) => {
                        if remaining.is_zero() {
                            return Ok(None);
                        }
                        std::thread::sleep(remaining.min(Duration::from_millis(50)));
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            let dupl = self.dupl.as_ref().unwrap().clone();
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match unsafe { dupl.AcquireNextFrame(remaining.as_millis() as u32, &mut info, &mut res) } {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) if is_transient(&e) => {
                    self.dupl = None;
                    continue;
                }
                Err(e) => return Err(VideoError::Win { context: "AcquireNextFrame", source: e }),
            }
            let captured_at = Instant::now();
            self.read_pointer(&dupl, &info);
            // Mise à jour du curseur seul : pas de nouvelle image de bureau.
            if info.LastPresentTime == 0 && self.have_image {
                unsafe { dupl.ReleaseFrame() }.ok();
                continue;
            }
            let copy = (|| -> Result<usize> {
                let res = res.as_ref().ok_or_else(|| VideoError::Other("AcquireNextFrame: no resource".into()))?;
                let src: ID3D11Texture2D = res.cast().ctx("IDXGIResource->ID3D11Texture2D")?;
                self.ensure_ring(&src)?;
                let idx = self.ring_pos;
                self.ring_pos = (self.ring_pos + 1) % RING_SIZE;
                unsafe { self.context.CopyResource(&self.ring[idx], &src) };
                Ok(idx)
            })();
            unsafe { dupl.ReleaseFrame() }.ok();
            let idx = copy?;
            self.have_image = true;
            self.last_idx = Some(idx);
            self.seq += 1;
            return Ok(Some(CapturedFrame {
                texture: self.ring[idx].clone(),
                width: self.width,
                height: self.height,
                captured_at,
                seq: self.seq,
                accumulated_frames: info.AccumulatedFrames,
            }));
        }
    }

    /// Force la recréation de la duplication au prochain `capture` (ex. après un changement
    /// de configuration d'affichage détecté par l'appelant).
    pub fn reset(&mut self) {
        self.dupl = None;
    }

    /// Redonne la dernière image capturée (nouvel horodatage). Utile pour répondre à une demande
    /// d'IDR ou maintenir un FPS constant quand l'écran est statique.
    pub fn last_frame(&mut self) -> Option<CapturedFrame> {
        let idx = self.last_idx?;
        self.seq += 1;
        Some(CapturedFrame {
            texture: self.ring[idx].clone(),
            width: self.width,
            height: self.height,
            captured_at: Instant::now(),
            seq: self.seq,
            accumulated_frames: 0,
        })
    }
}
