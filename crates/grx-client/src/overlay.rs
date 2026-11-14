//! Menu en overlay de `grx-client play`, inspiré de celui de Parsec.
//!
//! * [`Overlay`] : état et logique du menu, **sans winit ni Direct2D** (testé) : entrées
//!   abstraites ([`Ev`], [`Hot`]) -> [`Action`]s à appliquer + [`Overlay::scene`] à dessiner.
//! * [`OverlayInput`] : adaptateur winit (raccourcis, souris, clavier de navigation) ; il décide
//!   quels événements sont consommés et ne doivent donc JAMAIS atteindre le host.
//!
//! Ouverture : Ctrl+Maj+M ou bouton flottant en haut au centre (apparaît quand la souris
//! s'approche du bord haut). Menu ouvert : souris et clavier vont au menu, la capture souris
//! relative est suspendue (rétablie à la fermeture).

use std::collections::HashSet;

use grx_core::displays::{DisplayInfo, DisplayList};
use grx_core::stream_settings::{HostInfo, StreamSettings};
use grx_render::winit::event::{ElementState, MouseButton as WinButton, MouseScrollDelta, WindowEvent};
use grx_render::winit::keyboard::{KeyCode, PhysicalKey};
use grx_render::{Align, Color, Font, OverlayScene, Rect};

use crate::overlay_layout::{frac_of, place, Frac, Preset, MARGIN};
use crate::overlay_prefs::OverlayPrefs;

// ---------------------------------------------------------------------------------------------
// Raccourcis

/// Raccourcis du client (Ctrl+Maj+lettre, comme Parsec). Jamais transmis au host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hot {
    ToggleMenu,
    ToggleStats,
    Disconnect,
    /// Écran distant précédent / suivant (Ctrl+Maj+← / →).
    PrevDisplay,
    NextDisplay,
}

/// `Ctrl+Maj+M` menu, `Ctrl+Maj+S` stats, `Ctrl+Maj+Q` déconnexion, `Ctrl+Maj+←/→` écran
/// distant précédent/suivant (sans Alt).
pub fn classify_hotkey(code: KeyCode, ctrl: bool, shift: bool, alt: bool) -> Option<Hot> {
    if !ctrl || !shift || alt {
        return None;
    }
    match code {
        KeyCode::KeyM => Some(Hot::ToggleMenu),
        KeyCode::KeyS => Some(Hot::ToggleStats),
        KeyCode::KeyQ => Some(Hot::Disconnect),
        KeyCode::ArrowLeft => Some(Hot::PrevDisplay),
        KeyCode::ArrowRight => Some(Hot::NextDisplay),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Modèle

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Fullscreen,
    Stats,
    Volume,
    Bandwidth,
    Fps,
    /// Écran distant (masquée s'il n'y en a qu'un).
    Display,
    Capture,
    KbdLock,
    Vsync,
    Button,
    MenuPos,
    StatsPos,
    Shortcuts,
    Disconnect,
}

const ROWS: [Row; 14] = [
    Row::Fullscreen,
    Row::Stats,
    Row::Volume,
    Row::Bandwidth,
    Row::Fps,
    Row::Display,
    Row::Capture,
    Row::KbdLock,
    Row::Vsync,
    Row::Button,
    Row::MenuPos,
    Row::StatsPos,
    Row::Shortcuts,
    Row::Disconnect,
];

/// Ce que l'intégration doit faire (fenêtre, audio, réseau, entrées).
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    SetFullscreen(bool),
    /// Volume effectif (muet appliqué), 0..1.
    SetVolume(f32),
    /// Réglages à envoyer au host (`GRX-SET`).
    SendSettings(StreamSettings),
    SetKbdLock(bool),
    SetVsync(bool),
    /// Demander au host de capturer l'écran d'index donné (`GRX-MON`).
    SelectDisplay(u8),
    Disconnect,
    /// Menu ouvert : relâcher les entrées tenues côté host, suspendre la capture souris.
    MenuOpened,
    /// Menu fermé : rétablir la capture souris si `Overlay::capture`.
    MenuClosed,
}

/// Entrée abstraite (coordonnées en DIP dans la fenêtre).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ev {
    Move(f32, f32),
    Leave,
    Down(f32, f32),
    Up,
    /// Molette (crans, positif = vers le haut).
    Wheel(f32),
    Nav(Nav),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Up,
    Down,
    Left,
    Right,
    Activate,
    Close,
}

/// Statistiques de la dernière seconde (fil vidéo).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatsSnapshot {
    pub fps: f64,
    pub video_mbps: f64,
    pub rtt_ms: f64,
    pub loss_pct: f64,
    pub rx_ms: f64,
    pub dec_ms: f64,
    pub pres_ms: f64,
    pub e2e_ms: f64,
    pub width: u32,
    pub height: u32,
    pub codec: String,
    pub frames_lost: u64,
    pub dropped: u64,
}

/// Zone sous la souris.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Hit {
    FloatButton,
    Close,
    Row(Row),
    Slider(Row),
    Mute,
    Segment(u32),
    /// Bouton d'écran distant (index dans la liste du host).
    Display(usize),
    /// Barre de titre du menu (poignée de déplacement).
    Title,
    /// Panneau des statistiques (déplaçable quand le menu est ouvert).
    Stats,
    /// Ligne « Position » : 0..5 = `Preset::ALL`, 5 = réinitialiser.
    Preset(Row, usize),
    Panel,
}

/// Élément de l'overlay déplaçable à la souris.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Menu,
    Stats,
    Button,
}

/// Glisser-déposer en cours.
#[derive(Clone, Copy, Debug, PartialEq)]
struct MoveDrag {
    target: Target,
    /// Décalage curseur -> coin haut-gauche de l'élément au moment de la saisie.
    grab: (f32, f32),
    start: (f32, f32),
    /// Seuil de déplacement franchi (sinon un clic sur le bouton flottant ouvre le menu).
    moved: bool,
}

/// En dessous de ce déplacement (DIP), un appui/relâchement sur le bouton flottant est un clic.
const DRAG_THRESHOLD: f32 = 4.0;
/// Positions par défaut (relatives, voir `overlay_layout`).
const DEFAULT_MENU_POS: Frac = Frac::new(0.5, 0.0);
const DEFAULT_STATS_POS: Frac = Frac::new(0.0, 0.0);
const DEFAULT_BUTTON_POS: Frac = Frac::new(0.5, 0.0);
const STATS_W: f32 = 290.0;
const STATS_LINE_H: f32 = 19.0;

const PANEL_W: f32 = 460.0;
const ROW_H: f32 = 34.0;
const TITLE_H: f32 = 46.0;
const PAD: f32 = 16.0;
const LABEL_W: f32 = 180.0;
const HELP_LINE_H: f32 = 18.0;
const FOOTER_H: f32 = 30.0;
/// Ligne d'état des manettes sous le pied du menu (voir `gamepads::status_line`).
const PADS_H: f32 = 18.0;
/// La souris à moins de ça du bouton flottant (ou du bord de fenêtre auquel il est collé) le fait apparaître.
const REVEAL_ZONE: f32 = 48.0;
const FLOAT_W: f32 = 132.0;
const FLOAT_H: f32 = 26.0;

const BW_STEPS_MBPS: [u32; 16] = [2, 3, 5, 8, 10, 15, 20, 25, 30, 40, 50, 60, 80, 100, 150, 200];
const FPS_CHOICES: [u32; 4] = [30, 60, 120, 144];
const VOLUME_STEP: f32 = 0.05;

// Palette (sombre, semi-transparente).
const BG: Color = Color::rgba(18, 20, 26, 0.92);
const BORDER: Color = Color::rgba(255, 255, 255, 0.10);
const TEXT: Color = Color::rgb(230, 232, 238);
const DIM: Color = Color::rgba(230, 232, 238, 0.55);
const ACCENT: Color = Color::rgb(79, 140, 255);
const TRACK: Color = Color::rgba(255, 255, 255, 0.18);
const HILITE: Color = Color::rgba(255, 255, 255, 0.07);
const DANGER: Color = Color::rgb(214, 64, 69);

pub struct Overlay {
    pub menu_open: bool,
    pub show_stats: bool,
    /// Bouton flottant autorisé (masquable depuis le menu).
    pub button_enabled: bool,
    pub fullscreen: bool,
    pub volume: f32,
    pub muted: bool,
    /// Plafond de débit choisi (Mb/s), `None` = Auto (maximum du host).
    pub bitrate_cap_mbps: Option<u32>,
    /// FPS max choisi, `None` = cadence du host.
    pub fps_cap: Option<u32>,
    /// Capture souris souhaitée (suspendue tant que le menu est ouvert).
    pub capture: bool,
    pub kbd_lock: bool,
    pub vsync: bool,
    pub show_shortcuts: bool,
    pub host: Option<HostInfo>,
    pub stats: Option<StatsSnapshot>,
    pub host_name: String,
    pub capture_key: String,
    pub input_enabled: bool,
    pub audio_enabled: bool,
    pub monitor_hz: u32,
    /// Ligne d'état des manettes (vide : non affichée), mise à jour par `play.rs`.
    pub gamepads: String,
    /// Bandeau de sécurité (écran sécurisé Windows / fenêtre administrateur côté host) ; `None` =
    /// situation normale, rien n'est affiché. Rempli par `play.rs` d'après le message `GRX-SEC`.
    pub security_banner: Option<String>,
    /// Écrans du host (`GRX-DSP`), vide avec un host ancien.
    pub displays: Vec<DisplayInfo>,
    /// Écran capturé (index dans `displays`).
    pub active_display: Option<usize>,
    /// Écran choisi par l'utilisateur ([`display_key`]), mémorisé dans `overlay.json`.
    pub preferred_display: Option<String>,
    /// Le choix mémorisé a déjà été redemandé au host pour cette connexion.
    display_restored: bool,
    view_w: f32,
    view_h: f32,
    pub scale: f32,
    cursor: Option<(f32, f32)>,
    focus: usize,
    drag: Option<Row>,
    /// Positions relatives à la fenêtre (toujours entièrement visibles, voir `overlay_layout`).
    pub menu_pos: Frac,
    pub stats_pos: Frac,
    pub button_pos: Frac,
    move_drag: Option<MoveDrag>,
}

impl Overlay {
    pub fn new(host_name: impl Into<String>, capture_key: impl Into<String>) -> Self {
        Overlay {
            menu_open: false,
            show_stats: false,
            button_enabled: true,
            fullscreen: false,
            volume: 0.8,
            muted: false,
            bitrate_cap_mbps: None,
            fps_cap: None,
            capture: false,
            kbd_lock: false,
            vsync: false,
            show_shortcuts: false,
            host: None,
            stats: None,
            host_name: host_name.into(),
            capture_key: capture_key.into(),
            input_enabled: true,
            audio_enabled: true,
            monitor_hz: 60,
            gamepads: String::new(),
            security_banner: None,
            displays: Vec::new(),
            active_display: None,
            preferred_display: None,
            display_restored: false,
            view_w: 1280.0,
            view_h: 720.0,
            scale: 1.0,
            cursor: None,
            focus: 0,
            drag: None,
            menu_pos: DEFAULT_MENU_POS,
            stats_pos: DEFAULT_STATS_POS,
            button_pos: DEFAULT_BUTTON_POS,
            move_drag: None,
        }
    }

    /// Applique les préférences mémorisées (déjà bornées par `OverlayPrefs::sanitized`).
    pub fn apply_prefs(&mut self, p: &OverlayPrefs) {
        self.menu_pos = OverlayPrefs::pos(p.menu_pos).map_or(DEFAULT_MENU_POS, Frac::sanitized);
        self.stats_pos = OverlayPrefs::pos(p.stats_pos).map_or(DEFAULT_STATS_POS, Frac::sanitized);
        self.button_pos = OverlayPrefs::pos(p.button_pos).map_or(DEFAULT_BUTTON_POS, Frac::sanitized);
        self.show_stats = p.show_stats;
        self.button_enabled = p.button_enabled;
        self.volume = p.volume.clamp(0.0, 1.0);
        self.muted = p.muted;
        self.vsync = p.vsync;
        self.capture = p.capture;
        self.bitrate_cap_mbps = p.bitrate_cap_mbps;
        self.fps_cap = p.fps_cap;
        self.preferred_display = p.display.clone();
    }

    /// Préférences à mémoriser (positions + réglages du menu, aucun secret).
    pub fn prefs(&self) -> OverlayPrefs {
        OverlayPrefs {
            menu_pos: Some(OverlayPrefs::to_arr(self.menu_pos)),
            stats_pos: Some(OverlayPrefs::to_arr(self.stats_pos)),
            button_pos: Some(OverlayPrefs::to_arr(self.button_pos)),
            show_stats: self.show_stats,
            button_enabled: self.button_enabled,
            volume: self.volume,
            muted: self.muted,
            vsync: self.vsync,
            capture: self.capture,
            bitrate_cap_mbps: self.bitrate_cap_mbps,
            fps_cap: self.fps_cap,
            display: self.preferred_display.clone(),
            ..OverlayPrefs::default()
        }
    }

    /// Un glisser (élément ou curseur de réglage) est en cours : ne pas écrire les préférences à chaque pas.
    pub fn is_dragging(&self) -> bool {
        self.move_drag.is_some() || self.drag.is_some()
    }

    /// Curseur « déplacer » : survol d'une poignée ou déplacement en cours.
    pub fn wants_move_cursor(&self) -> bool {
        match self.move_drag {
            Some(d) => d.moved || d.target != Target::Button,
            None => self.menu_open && matches!(self.hit(), Some(Hit::Title | Hit::Stats)),
        }
    }

    /// Taille de la fenêtre en pixels physiques + facteur DPI.
    pub fn set_view(&mut self, phys_w: u32, phys_h: u32, scale: f64) {
        self.scale = if scale.is_finite() && scale > 0.1 { scale as f32 } else { 1.0 };
        self.view_w = phys_w as f32 / self.scale;
        self.view_h = phys_h as f32 / self.scale;
    }

    /// Quelque chose est-il à l'écran ? (sinon aucun coût de rendu)
    pub fn is_visible(&self) -> bool {
        self.menu_open || self.show_stats || self.float_visible() || self.security_banner.is_some()
    }

    fn float_visible(&self) -> bool {
        if self.move_drag.is_some_and(|d| d.target == Target::Button) {
            return true;
        }
        self.button_enabled && !self.menu_open && !self.capture && self.cursor.is_some_and(|(x, y)| self.in_reveal_zone(x, y))
    }

    /// Près du bouton, ou dans la bande du bord de fenêtre auquel il est collé (haut par défaut).
    fn in_reveal_zone(&self, x: f32, y: f32) -> bool {
        if x < 0.0 || y < 0.0 || x >= self.view_w || y >= self.view_h {
            return false;
        }
        let r = self.float_rect();
        if r.inset(-REVEAL_ZONE, -REVEAL_ZONE).contains(x, y) {
            return true;
        }
        let near = 2.0 * MARGIN;
        (r.y <= near && y < REVEAL_ZONE)
            || (r.y + r.h >= self.view_h - near && y >= self.view_h - REVEAL_ZONE)
            || (r.x <= near && x < REVEAL_ZONE)
            || (r.x + r.w >= self.view_w - near && x >= self.view_w - REVEAL_ZONE)
    }

    pub fn settings(&self) -> StreamSettings {
        StreamSettings { max_bitrate_kbps: self.bitrate_cap_mbps.map(|m| m * 1000), max_fps: self.fps_cap }
    }

    pub fn effective_volume(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.volume.clamp(0.0, 1.0)
        }
    }

    fn host_max_mbps(&self) -> u32 {
        self.host.map(|h| h.host_max_bitrate_kbps / 1000).filter(|&m| m > 0).unwrap_or(50)
    }

    /// Positions du curseur de débit : paliers <= maximum du host, puis Auto (`None`).
    pub fn bw_steps(&self) -> Vec<Option<u32>> {
        let max = self.host_max_mbps();
        let mut v: Vec<Option<u32>> = BW_STEPS_MBPS.iter().copied().filter(|&m| m < max).map(Some).collect();
        v.push(None);
        v
    }

    fn bw_index(&self) -> usize {
        let steps = self.bw_steps();
        match self.bitrate_cap_mbps {
            None => steps.len() - 1,
            Some(c) => steps.iter().position(|s| s.is_some_and(|m| m >= c)).unwrap_or(steps.len() - 1),
        }
    }

    /// Choix de FPS proposés : 30/60/120/144 jusqu'à la fréquence de l'écran (30 et 60 toujours).
    pub fn fps_options(&self) -> Vec<u32> {
        FPS_CHOICES.iter().copied().filter(|&f| f <= 60 || f <= self.monitor_hz + 1).collect()
    }

    fn fps_selected(&self) -> u32 {
        self.fps_cap.unwrap_or_else(|| self.host.map_or(60, |h| h.host_fps))
    }

    // ── écrans distants ──

    /// Lignes affichées : « Écran » seulement si le host en propose plusieurs.
    pub fn rows(&self) -> Vec<Row> {
        ROWS.iter().copied().filter(|&r| r != Row::Display || self.displays.len() > 1).collect()
    }

    /// Position d'une ligne dans [`Self::rows`] (0 si elle est masquée).
    pub fn row_index(&self, r: Row) -> usize {
        self.rows().iter().position(|&x| x == r).unwrap_or(0)
    }

    fn focused_row(&self) -> Row {
        let rows = self.rows();
        rows[self.focus.min(rows.len() - 1)]
    }

    /// Nouvelle annonce du host. À la première annonce de la connexion, redemande l'écran
    /// mémorisé s'il existe encore (et n'est pas déjà capturé).
    pub fn set_displays(&mut self, l: DisplayList) -> Vec<Action> {
        let focused = self.focused_row();
        self.displays = l.displays;
        self.active_display = l.active.map(usize::from).filter(|&i| i < self.displays.len());
        self.focus = self.row_index(focused);
        if self.display_restored {
            return vec![];
        }
        self.display_restored = true;
        let Some(want) = self.preferred_display.as_deref() else { return vec![] };
        match self.displays.iter().position(|d| display_key(d) == want) {
            Some(i) if Some(i) != self.active_display => {
                self.active_display = Some(i);
                vec![Action::SelectDisplay(i as u8)]
            }
            _ => vec![],
        }
    }

    /// Choix de l'écran `i` (clic, clavier, raccourci).
    pub fn select_display(&mut self, i: usize) -> Vec<Action> {
        let Some(d) = self.displays.get(i) else { return vec![] };
        self.preferred_display = Some(display_key(d));
        if self.active_display == Some(i) {
            return vec![];
        }
        // Affiché tout de suite ; l'annonce suivante du host confirme (ou corrige).
        self.active_display = Some(i);
        vec![Action::SelectDisplay(i as u8)]
    }

    /// Écran précédent (`dir` < 0) / suivant, en boucle. Rien avec un seul écran.
    pub fn cycle_display(&mut self, dir: i32) -> Vec<Action> {
        let n = self.displays.len() as i32;
        if n < 2 {
            return vec![];
        }
        let cur = self.active_display.map_or(0, |i| i as i32);
        self.select_display((cur + dir.signum()).rem_euclid(n) as usize)
    }

    // ── ouverture / fermeture ──

    pub fn open_menu(&mut self) -> Vec<Action> {
        if self.menu_open {
            return vec![];
        }
        self.menu_open = true;
        self.drag = None;
        self.move_drag = None;
        self.focus = 0;
        vec![Action::MenuOpened]
    }

    pub fn close_menu(&mut self) -> Vec<Action> {
        if !self.menu_open {
            return vec![];
        }
        self.menu_open = false;
        self.drag = None;
        self.move_drag = None;
        vec![Action::MenuClosed]
    }

    pub fn on_hotkey(&mut self, h: Hot) -> Vec<Action> {
        match h {
            Hot::ToggleMenu => {
                if self.menu_open {
                    self.close_menu()
                } else {
                    self.open_menu()
                }
            }
            Hot::ToggleStats => {
                self.show_stats = !self.show_stats;
                vec![]
            }
            Hot::Disconnect => vec![Action::Disconnect],
            Hot::PrevDisplay => self.cycle_display(-1),
            Hot::NextDisplay => self.cycle_display(1),
        }
    }

    // ── entrées ──

    /// Retourne `(consommé, actions)`. Un événement consommé ne doit pas partir vers le host.
    pub fn on_ev(&mut self, e: Ev) -> (bool, Vec<Action>) {
        match e {
            Ev::Move(x, y) => {
                self.cursor = Some((x, y));
                if self.move_drag.is_some() {
                    self.drag_to(x, y);
                    return (true, vec![]);
                }
                if let Some(row) = self.drag {
                    return (true, self.slide(row, x));
                }
                (self.menu_open, vec![])
            }
            Ev::Leave => {
                self.cursor = None;
                (self.menu_open, vec![])
            }
            Ev::Up => {
                if let Some(d) = self.move_drag.take() {
                    // Bouton flottant appuyé puis relâché sans bouger : c'est un clic.
                    let a = if d.target == Target::Button && !d.moved { self.open_menu() } else { vec![] };
                    return (true, a);
                }
                let was = self.drag.take().is_some();
                (self.menu_open || was, vec![])
            }
            Ev::Down(x, y) => {
                self.cursor = Some((x, y));
                if !self.menu_open {
                    if self.float_visible() && self.float_rect().contains(x, y) {
                        self.start_move(Target::Button, x, y);
                        return (true, vec![]);
                    }
                    return (false, vec![]);
                }
                (true, self.click(x, y))
            }
            Ev::Wheel(d) => {
                if !self.menu_open {
                    return (false, vec![]);
                }
                let row = match self.hit() {
                    Some(Hit::Row(r) | Hit::Slider(r) | Hit::Preset(r, _)) => Some(r),
                    Some(Hit::Mute) => Some(Row::Volume),
                    Some(Hit::Segment(_)) => Some(Row::Fps),
                    Some(Hit::Display(_)) => Some(Row::Display),
                    _ => None,
                };
                let a = match row {
                    Some(r) if d != 0.0 => self.adjust(r, if d > 0.0 { 1 } else { -1 }),
                    _ => vec![],
                };
                (true, a)
            }
            Ev::Nav(n) => {
                if !self.menu_open {
                    return (false, vec![]);
                }
                let n_rows = self.rows().len();
                let a = match n {
                    Nav::Up => {
                        self.focus = (self.focus.min(n_rows - 1) + n_rows - 1) % n_rows;
                        vec![]
                    }
                    Nav::Down => {
                        self.focus = (self.focus + 1) % n_rows;
                        vec![]
                    }
                    Nav::Left => self.adjust(self.focused_row(), -1),
                    Nav::Right => self.adjust(self.focused_row(), 1),
                    Nav::Activate => self.activate(self.focused_row()),
                    Nav::Close => self.close_menu(),
                };
                (true, a)
            }
        }
    }

    fn click(&mut self, x: f32, y: f32) -> Vec<Action> {
        match self.hit_at(x, y) {
            None => self.close_menu(),
            Some(Hit::Close) => self.close_menu(),
            Some(Hit::Panel) | Some(Hit::FloatButton) => vec![],
            Some(Hit::Title) => {
                self.start_move(Target::Menu, x, y);
                vec![]
            }
            Some(Hit::Stats) => {
                self.start_move(Target::Stats, x, y);
                vec![]
            }
            Some(Hit::Preset(r, i)) => {
                self.focus = self.row_index(r);
                self.apply_preset(r, i);
                vec![]
            }
            Some(Hit::Mute) => {
                self.focus = self.row_index(Row::Volume);
                self.muted = !self.muted;
                vec![Action::SetVolume(self.effective_volume())]
            }
            Some(Hit::Segment(f)) => {
                self.focus = self.row_index(Row::Fps);
                self.set_fps(f)
            }
            Some(Hit::Display(i)) => {
                self.focus = self.row_index(Row::Display);
                self.select_display(i)
            }
            Some(Hit::Slider(r)) => {
                self.focus = self.row_index(r);
                self.drag = Some(r);
                self.slide(r, x)
            }
            Some(Hit::Row(r)) => {
                self.focus = self.row_index(r);
                self.activate(r)
            }
        }
    }

    // ── déplacement ──

    fn target_rect(&self, t: Target) -> Rect {
        match t {
            Target::Menu => self.panel_rect(),
            Target::Stats => self.stats_rect(),
            Target::Button => self.float_rect(),
        }
    }

    fn start_move(&mut self, target: Target, x: f32, y: f32) {
        let r = self.target_rect(target);
        self.move_drag = Some(MoveDrag { target, grab: (x - r.x, y - r.y), start: (x, y), moved: false });
    }

    /// Le curseur est en (x, y) pendant un déplacement : nouvelle position relative, bornée.
    fn drag_to(&mut self, x: f32, y: f32) {
        let Some(mut d) = self.move_drag else { return };
        if !d.moved && (x - d.start.0).abs() < DRAG_THRESHOLD && (y - d.start.1).abs() < DRAG_THRESHOLD {
            return;
        }
        d.moved = true;
        self.move_drag = Some(d);
        let r = self.target_rect(d.target);
        let f = frac_of((x - d.grab.0, y - d.grab.1), (r.w, r.h), (self.view_w, self.view_h), MARGIN);
        match d.target {
            Target::Menu => self.menu_pos = f,
            Target::Stats => self.stats_pos = f,
            Target::Button => self.button_pos = f,
        }
    }

    fn pos_of_row(&mut self, r: Row) -> Option<&mut Frac> {
        match r {
            Row::MenuPos => Some(&mut self.menu_pos),
            Row::StatsPos => Some(&mut self.stats_pos),
            _ => None,
        }
    }

    /// Segment `i` de la ligne « Position » : préréglage, ou (dernier) réinitialiser. La
    /// réinitialisation de la position du menu remet aussi le bouton flottant à sa place.
    fn apply_preset(&mut self, r: Row, i: usize) {
        if let Some(p) = Preset::ALL.get(i) {
            if let Some(pos) = self.pos_of_row(r) {
                *pos = p.frac();
            }
            return;
        }
        match r {
            Row::MenuPos => {
                self.menu_pos = DEFAULT_MENU_POS;
                self.button_pos = DEFAULT_BUTTON_POS;
            }
            Row::StatsPos => self.stats_pos = DEFAULT_STATS_POS,
            _ => {}
        }
    }

    /// Préréglage suivant/précédent (clavier, molette).
    fn cycle_preset(&mut self, r: Row, dir: i32) {
        let Some(&mut cur) = self.pos_of_row(r) else { return };
        let n = Preset::ALL.len() as i32;
        let i = match Preset::matching(cur).and_then(|p| Preset::ALL.iter().position(|&q| q == p)) {
            Some(i) => (i as i32 + dir).rem_euclid(n),
            None if dir > 0 => 0,
            None => n - 1,
        };
        self.apply_preset(r, i as usize);
    }

    fn set_fps(&mut self, f: u32) -> Vec<Action> {
        if self.fps_cap == Some(f) {
            return vec![];
        }
        self.fps_cap = Some(f);
        vec![Action::SendSettings(self.settings())]
    }

    fn set_bw_index(&mut self, i: usize) -> Vec<Action> {
        let steps = self.bw_steps();
        let v = steps[i.min(steps.len() - 1)];
        if v == self.bitrate_cap_mbps {
            return vec![];
        }
        self.bitrate_cap_mbps = v;
        vec![Action::SendSettings(self.settings())]
    }

    fn set_volume(&mut self, v: f32) -> Vec<Action> {
        let v = (v.clamp(0.0, 1.0) * 100.0).round() / 100.0;
        if v == self.volume && !self.muted {
            return vec![];
        }
        self.volume = v;
        self.muted = false;
        vec![Action::SetVolume(self.effective_volume())]
    }

    /// Curseur glissé à l'abscisse `x`.
    fn slide(&mut self, row: Row, x: f32) -> Vec<Action> {
        let Some(track) = self.slider_rect(row) else { return vec![] };
        let t = ((x - track.x) / track.w.max(1.0)).clamp(0.0, 1.0);
        match row {
            Row::Volume => self.set_volume(t),
            Row::Bandwidth => {
                let n = self.bw_steps().len();
                self.set_bw_index((t * (n - 1) as f32).round() as usize)
            }
            _ => vec![],
        }
    }

    /// Gauche/droite (ou molette) sur une ligne.
    fn adjust(&mut self, row: Row, dir: i32) -> Vec<Action> {
        match row {
            Row::Volume => self.set_volume(self.volume + dir as f32 * VOLUME_STEP),
            Row::Bandwidth => {
                let i = self.bw_index() as i32 + dir;
                if i < 0 {
                    return vec![];
                }
                self.set_bw_index(i as usize)
            }
            Row::Fps => {
                let opts = self.fps_options();
                let cur = self.fps_selected();
                let i = opts.iter().position(|&f| f >= cur).unwrap_or(opts.len() - 1) as i32 + dir;
                match opts.get(i.clamp(0, opts.len() as i32 - 1) as usize) {
                    Some(&f) => self.set_fps(f),
                    None => vec![],
                }
            }
            Row::MenuPos | Row::StatsPos => {
                self.cycle_preset(row, dir);
                vec![]
            }
            Row::Display => self.cycle_display(dir),
            // Les interrupteurs : droite = activer, gauche = désactiver.
            r => {
                let on = dir > 0;
                if self.toggle_state(r) != Some(on) {
                    self.activate(r)
                } else {
                    vec![]
                }
            }
        }
    }

    fn toggle_state(&self, r: Row) -> Option<bool> {
        Some(match r {
            Row::Fullscreen => self.fullscreen,
            Row::Stats => self.show_stats,
            Row::Capture => self.capture,
            Row::KbdLock => self.kbd_lock,
            Row::Vsync => self.vsync,
            Row::Button => self.button_enabled,
            Row::Shortcuts => self.show_shortcuts,
            Row::Volume => !self.muted,
            _ => return None,
        })
    }

    fn row_enabled(&self, r: Row) -> bool {
        match r {
            Row::Capture | Row::KbdLock => self.input_enabled,
            Row::Volume => self.audio_enabled,
            _ => true,
        }
    }

    /// Entrée / clic sur une ligne.
    fn activate(&mut self, r: Row) -> Vec<Action> {
        if !self.row_enabled(r) {
            return vec![];
        }
        match r {
            Row::Fullscreen => {
                self.fullscreen = !self.fullscreen;
                vec![Action::SetFullscreen(self.fullscreen)]
            }
            Row::Stats => {
                self.show_stats = !self.show_stats;
                vec![]
            }
            Row::Volume => {
                self.muted = !self.muted;
                vec![Action::SetVolume(self.effective_volume())]
            }
            Row::Bandwidth | Row::Fps => vec![],
            Row::Display => self.cycle_display(1),
            Row::MenuPos | Row::StatsPos => {
                self.apply_preset(r, Preset::ALL.len());
                vec![]
            }
            Row::Capture => {
                // Appliquée à la fermeture du menu (impossible de capturer la souris menu ouvert).
                self.capture = !self.capture;
                vec![]
            }
            Row::KbdLock => {
                self.kbd_lock = !self.kbd_lock;
                vec![Action::SetKbdLock(self.kbd_lock)]
            }
            Row::Vsync => {
                self.vsync = !self.vsync;
                vec![Action::SetVsync(self.vsync)]
            }
            Row::Button => {
                self.button_enabled = !self.button_enabled;
                vec![]
            }
            Row::Shortcuts => {
                self.show_shortcuts = !self.show_shortcuts;
                vec![]
            }
            Row::Disconnect => vec![Action::Disconnect],
        }
    }

    // ── géométrie ──

    fn panel_rect(&self) -> Rect {
        let help = if self.show_shortcuts { HELP_LINES.len() as f32 * HELP_LINE_H + 10.0 } else { 0.0 };
        let pads = if self.gamepads.is_empty() { 0.0 } else { PADS_H };
        let h = TITLE_H + self.rows().len() as f32 * ROW_H + help + FOOTER_H + pads + 8.0;
        let w = PANEL_W.min(self.view_w - 16.0).max(200.0);
        let (x, y) = place(self.menu_pos, (w, h), (self.view_w, self.view_h), MARGIN);
        Rect::new(x, y, w, h)
    }

    fn stats_rect(&self) -> Rect {
        let n = if self.stats.is_some() { STATS_LINES } else { 1 };
        let (w, h) = (STATS_W, 14.0 + n as f32 * STATS_LINE_H);
        let (x, y) = place(self.stats_pos, (w, h), (self.view_w, self.view_h), MARGIN);
        Rect::new(x, y, w, h)
    }

    /// Segments de la ligne « Position » : 5 préréglages + réinitialiser.
    fn preset_rects(&self, row: Row) -> Vec<Rect> {
        let w = self.widget_rect(self.row_index(row));
        let n = Preset::ALL.len() + 1;
        let gap = 4.0;
        let sw = (w.w - gap * (n as f32 - 1.0)) / n as f32;
        (0..n).map(|i| Rect::new(w.x + i as f32 * (sw + gap), w.y + 6.0, sw, w.h - 12.0)).collect()
    }

    fn row_rect(&self, i: usize) -> Rect {
        let p = self.panel_rect();
        Rect::new(p.x + 6.0, p.y + TITLE_H + i as f32 * ROW_H, p.w - 12.0, ROW_H)
    }

    /// Zone des widgets (à droite du libellé) d'une ligne.
    fn widget_rect(&self, i: usize) -> Rect {
        let r = self.row_rect(i);
        let x = r.x + LABEL_W;
        Rect::new(x, r.y, (r.x + r.w - PAD + 6.0 - x).max(40.0), r.h)
    }

    fn slider_rect(&self, row: Row) -> Option<Rect> {
        let w = self.widget_rect(self.row_index(row));
        match row {
            Row::Volume => Some(Rect::new(w.x + 8.0, w.y, (w.w - 120.0).max(30.0), w.h)),
            Row::Bandwidth => Some(Rect::new(w.x + 8.0, w.y, (w.w - 96.0).max(30.0), w.h)),
            _ => None,
        }
    }

    fn mute_rect(&self) -> Rect {
        let w = self.widget_rect(self.row_index(Row::Volume));
        Rect::new(w.x + w.w - 58.0, w.y + 7.0, 58.0, w.h - 14.0)
    }

    fn segment_rects(&self) -> Vec<(u32, Rect)> {
        let w = self.widget_rect(self.row_index(Row::Fps));
        let opts = self.fps_options();
        let gap = 6.0;
        let sw = (w.w - gap * (opts.len() as f32 - 1.0)) / opts.len() as f32;
        opts.iter().enumerate().map(|(i, &f)| (f, Rect::new(w.x + i as f32 * (sw + gap), w.y + 7.0, sw, w.h - 14.0))).collect()
    }

    /// Boutons « 1 », « 2 »… de la ligne « Écran » (largeur bornée, alignés à gauche).
    fn display_rects(&self) -> Vec<(usize, Rect)> {
        let w = self.widget_rect(self.row_index(Row::Display));
        let n = self.displays.len().max(1);
        let gap = 6.0;
        let sw = ((w.w - gap * (n as f32 - 1.0)) / n as f32).min(48.0);
        (0..self.displays.len()).map(|i| (i, Rect::new(w.x + i as f32 * (sw + gap), w.y + 7.0, sw, w.h - 14.0))).collect()
    }

    fn close_rect(&self) -> Rect {
        let p = self.panel_rect();
        Rect::new(p.x + p.w - 40.0, p.y + 8.0, 30.0, 30.0)
    }

    fn float_rect(&self) -> Rect {
        let (x, y) = place(self.button_pos, (FLOAT_W, FLOAT_H), (self.view_w, self.view_h), MARGIN);
        Rect::new(x, y, FLOAT_W, FLOAT_H)
    }

    fn hit(&self) -> Option<Hit> {
        self.cursor.and_then(|(x, y)| self.hit_at(x, y))
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        if !self.menu_open {
            return (self.float_visible() && self.float_rect().contains(x, y)).then_some(Hit::FloatButton);
        }
        let p = self.panel_rect();
        if !p.contains(x, y) {
            return (self.show_stats && self.stats_rect().contains(x, y)).then_some(Hit::Stats);
        }
        if self.close_rect().contains(x, y) {
            return Some(Hit::Close);
        }
        if y < p.y + TITLE_H {
            return Some(Hit::Title);
        }
        for (i, &r) in self.rows().iter().enumerate() {
            if !self.row_rect(i).contains(x, y) {
                continue;
            }
            if r == Row::Volume && self.mute_rect().contains(x, y) {
                return Some(Hit::Mute);
            }
            if r == Row::Fps {
                if let Some((f, _)) = self.segment_rects().into_iter().find(|(_, s)| s.contains(x, y)) {
                    return Some(Hit::Segment(f));
                }
            }
            if r == Row::Display {
                if let Some((d, _)) = self.display_rects().into_iter().find(|(_, s)| s.contains(x, y)) {
                    return Some(Hit::Display(d));
                }
            }
            if matches!(r, Row::MenuPos | Row::StatsPos) {
                if let Some(i) = self.preset_rects(r).iter().position(|s| s.contains(x, y)) {
                    return Some(Hit::Preset(r, i));
                }
            }
            if let Some(s) = self.slider_rect(r) {
                // Zone de saisie un peu plus large que la piste.
                if s.inset(-8.0, 0.0).contains(x, y) {
                    return Some(Hit::Slider(r));
                }
            }
            return Some(Hit::Row(r));
        }
        Some(Hit::Panel)
    }

    // ── rendu ──

    /// Scène à dessiner, `None` si rien n'est visible (aucun coût côté rendu).
    pub fn scene(&self) -> Option<OverlayScene> {
        if !self.is_visible() {
            return None;
        }
        let mut s = OverlayScene::new(self.scale);
        if let Some(text) = &self.security_banner {
            self.draw_security_banner(&mut s, text);
        }
        if self.show_stats {
            self.draw_stats(&mut s);
        }
        if self.menu_open {
            self.draw_menu(&mut s);
        } else if self.float_visible() {
            let r = self.float_rect();
            let hover = self.hit() == Some(Hit::FloatButton);
            s.fill(r, FLOAT_H / 2.0, if hover { Color::rgba(40, 46, 60, 0.95) } else { Color::rgba(18, 20, 26, 0.80) });
            s.stroke(r, FLOAT_H / 2.0, BORDER, 1.0);
            s.text(r, "\u{2630}  Menu GRX", 13.0, TEXT, Align::Center, Font::UiBold);
        }
        Some(s)
    }

    fn draw_stats(&self, s: &mut OverlayScene) {
        let lines: Vec<(String, String)> = match &self.stats {
            None => vec![("Statistiques".into(), "en attente...".into())],
            Some(st) => {
                let cap = self
                    .host
                    .and_then(|h| h.bitrate_cap_kbps)
                    .map_or(String::new(), |k| format!("  (max {:.0})", k as f64 / 1000.0));
                vec![
                    ("Flux".into(), format!("{}x{} {}", st.width, st.height, st.codec)),
                    ("FPS".into(), format!("{:.0}", st.fps)),
                    ("Débit".into(), format!("{:.1} Mb/s{cap}", st.video_mbps)),
                    ("RTT".into(), format!("{:.1} ms", st.rtt_ms)),
                    ("Perte".into(), format!("{:.1} %", st.loss_pct)),
                    ("Capture→récep.".into(), format!("{:.1} ms", st.rx_ms)),
                    ("Décodage".into(), format!("{:.2} ms", st.dec_ms)),
                    ("Affichage".into(), format!("{:.2} ms", st.pres_ms)),
                    ("Capture→écran".into(), format!("~{:.1} ms", st.e2e_ms)),
                    ("Perdues/écartées".into(), format!("{} / {}", st.frames_lost, st.dropped)),
                    ("Écran".into(), self.display_summary()),
                ]
            }
        };
        let lh = STATS_LINE_H;
        let r = self.stats_rect();
        s.fill(r, 8.0, Color::rgba(10, 12, 16, 0.78));
        if self.menu_open && (self.hit() == Some(Hit::Stats) || self.move_drag.is_some_and(|d| d.target == Target::Stats)) {
            s.stroke(r, 8.0, ACCENT.with_alpha(0.7), 1.0);
        }
        for (i, (k, v)) in lines.iter().enumerate() {
            let y = r.y + 7.0 + i as f32 * lh;
            s.text(Rect::new(r.x + 12.0, y, 130.0, lh), k.as_str(), 12.5, DIM, Align::Left, Font::Mono);
            s.text(Rect::new(r.x + 140.0, y, r.w - 152.0, lh), v.as_str(), 12.5, TEXT, Align::Right, Font::Mono);
        }
    }

    /// « 2/3 VG27A » (écran capturé / nombre d'écrans), « - » si le host ne l'annonce pas.
    pub fn display_summary(&self) -> String {
        match self.active_display.and_then(|i| self.displays.get(i).map(|d| (i, d))) {
            Some((i, d)) => format!("{}/{} {}", i + 1, self.displays.len(), if d.name.is_empty() { "Écran" } else { &d.name }),
            None => "-".into(),
        }
    }

    /// Bandeau discret en haut au centre : état de sécurité du host (écran sécurisé / fenêtre
    /// administrateur). Toujours visible tant que la situation n'est pas revenue à la normale.
    fn draw_security_banner(&self, s: &mut OverlayScene, text: &str) {
        // Largeur proportionnelle au texte (≈ 7 px/caractère à 13 px), bornée à la fenêtre.
        let w = ((text.chars().count() as f32 * 7.4) + 44.0).min(self.view_w - 2.0 * MARGIN).max(220.0);
        let h = 30.0;
        let x = ((self.view_w - w) / 2.0).max(MARGIN);
        let y = MARGIN;
        let r = Rect::new(x, y, w, h);
        // Fond ambre sombre, liseré ambre : visible sans masquer le jeu.
        s.fill(r, 7.0, Color::rgba(60, 42, 10, 0.92));
        s.stroke(r, 7.0, Color::rgba(240, 180, 60, 0.85), 1.0);
        s.fill(Rect::new(r.x, r.y, 4.0, r.h), 2.0, Color::rgba(240, 180, 60, 0.95));
        s.text(Rect::new(r.x + 14.0, r.y, r.w - 22.0, r.h), text, 13.0, Color::rgb(248, 226, 170), Align::Left, Font::UiBold);
    }

    fn draw_toggle(s: &mut OverlayScene, w: Rect, on: bool, enabled: bool) {
        let r = Rect::new(w.x + w.w - 44.0, w.y + (w.h - 22.0) / 2.0, 44.0, 22.0);
        let a = if enabled { 1.0 } else { 0.35 };
        s.fill(r, 11.0, if on { ACCENT.with_alpha(a) } else { TRACK.with_alpha(0.18 * a) });
        let kx = if on { r.x + r.w - 19.0 } else { r.x + 3.0 };
        s.fill(Rect::new(kx, r.y + 3.0, 16.0, 16.0), 8.0, Color::rgb(255, 255, 255).with_alpha(a));
    }

    fn draw_slider(s: &mut OverlayScene, track: Rect, t: f32, enabled: bool) {
        let a = if enabled { 1.0 } else { 0.35 };
        let y = track.y + track.h / 2.0;
        s.fill(Rect::new(track.x, y - 2.0, track.w, 4.0), 2.0, TRACK);
        let fx = track.w * t.clamp(0.0, 1.0);
        s.fill(Rect::new(track.x, y - 2.0, fx, 4.0), 2.0, ACCENT.with_alpha(a));
        s.fill(Rect::new(track.x + fx - 7.0, y - 7.0, 14.0, 14.0), 7.0, Color::rgb(255, 255, 255).with_alpha(a));
    }

    fn draw_menu(&self, s: &mut OverlayScene) {
        let p = self.panel_rect();
        s.fill(p, 12.0, BG);
        s.stroke(p, 12.0, BORDER, 1.0);
        if self.hit() == Some(Hit::Title) || self.move_drag.is_some_and(|d| d.target == Target::Menu) {
            s.fill(Rect::new(p.x + 1.0, p.y + 1.0, p.w - 2.0, TITLE_H - 3.0), 11.0, HILITE);
        }
        s.text(
            Rect::new(p.x + PAD, p.y + 4.0, p.w - 60.0, TITLE_H - 8.0),
            format!("GRX  ·  {}", self.host_name),
            15.0,
            TEXT,
            Align::Left,
            Font::UiBold,
        );
        let c = self.close_rect();
        if self.hit() == Some(Hit::Close) {
            s.fill(c, 6.0, HILITE);
        }
        s.text(c, "\u{2715}", 14.0, DIM, Align::Center, Font::Ui);
        s.fill(Rect::new(p.x + PAD, p.y + TITLE_H - 2.0, p.w - 2.0 * PAD, 1.0), 0.0, BORDER);

        let hover_row = match self.hit() {
            Some(Hit::Row(r) | Hit::Slider(r)) => Some(r),
            Some(Hit::Mute) => Some(Row::Volume),
            Some(Hit::Segment(_)) => Some(Row::Fps),
            Some(Hit::Display(_)) => Some(Row::Display),
            Some(Hit::Preset(r, _)) => Some(r),
            _ => None,
        };
        let rows = self.rows();
        for (i, &row) in rows.iter().enumerate() {
            let r = self.row_rect(i);
            let enabled = self.row_enabled(row);
            if row == Row::Disconnect {
                let b = r.inset(4.0, 4.0);
                let hl = hover_row == Some(row) || self.focus == i;
                s.fill(b, 8.0, if hl { DANGER } else { DANGER.with_alpha(0.75) });
                s.text(b.inset(12.0, 0.0), "Déconnexion", 14.0, Color::rgb(255, 255, 255), Align::Left, Font::UiBold);
                s.text(b.inset(12.0, 0.0), "Ctrl+Maj+Q", 12.0, Color::rgba(255, 255, 255, 0.8), Align::Right, Font::Mono);
                continue;
            }
            if self.focus == i || hover_row == Some(row) {
                s.fill(r, 7.0, HILITE);
            }
            if self.focus == i {
                s.fill(Rect::new(r.x, r.y + 9.0, 3.0, r.h - 18.0), 1.5, ACCENT);
            }
            let (label, mut hint) = row_label(row, &self.capture_key);
            if let Some(Hit::Preset(r, j)) = self.hit() {
                if r == row {
                    hint = Preset::ALL.get(j).map_or("Réinitialiser", |p| p.label()).to_string();
                }
            }
            if row == Row::Display {
                // Survol d'un bouton : nom et résolution de cet écran ; sinon l'écran capturé.
                let shown = match self.hit() {
                    Some(Hit::Display(j)) => Some(j),
                    _ => self.active_display,
                };
                if let Some((j, d)) = shown.and_then(|j| self.displays.get(j).map(|d| (j, d))) {
                    hint = display_hint(j, d);
                }
            }
            let lc = if enabled { TEXT } else { DIM };
            let lh = if hint.is_empty() { r.h } else { r.h * 0.62 };
            s.text(Rect::new(r.x + PAD - 6.0, r.y, LABEL_W - PAD, lh), label, 14.0, lc, Align::Left, Font::Ui);
            if !hint.is_empty() {
                s.text(Rect::new(r.x + PAD - 6.0, r.y + r.h * 0.52, LABEL_W - PAD, r.h * 0.46), hint, 10.5, DIM, Align::Left, Font::Mono);
            }
            let w = self.widget_rect(i);
            match row {
                Row::Volume => {
                    let track = self.slider_rect(row).unwrap();
                    Self::draw_slider(s, track, if self.muted { 0.0 } else { self.volume }, enabled && !self.muted);
                    let vt = Rect::new(track.x + track.w + 8.0, w.y, 46.0, w.h);
                    let txt = if !enabled { "--".to_string() } else { format!("{:.0} %", self.volume * 100.0) };
                    s.text(vt, txt, 13.0, if self.muted { DIM } else { TEXT }, Align::Right, Font::Ui);
                    let m = self.mute_rect();
                    s.fill(m, 6.0, if self.muted { DANGER.with_alpha(0.85) } else { TRACK });
                    s.text(m, if self.muted { "Muet" } else { "Son" }, 12.5, TEXT, Align::Center, Font::UiBold);
                }
                Row::Bandwidth => {
                    let track = self.slider_rect(row).unwrap();
                    let n = self.bw_steps().len();
                    Self::draw_slider(s, track, self.bw_index() as f32 / (n - 1).max(1) as f32, true);
                    let txt = match self.bitrate_cap_mbps {
                        Some(m) => format!("{m} Mb/s"),
                        None => format!("Auto ({})", self.host_max_mbps()),
                    };
                    s.text(Rect::new(track.x + track.w + 6.0, w.y, w.x + w.w - track.x - track.w - 6.0, w.h), txt, 13.0, TEXT, Align::Right, Font::Ui);
                }
                Row::Fps => {
                    let sel = self.fps_selected();
                    let host_fps = self.host.map_or(u32::MAX, |h| h.host_fps.max(1));
                    for (f, sr) in self.segment_rects() {
                        let on = f == sel;
                        s.fill(sr, 6.0, if on { ACCENT } else { TRACK.with_alpha(0.12) });
                        let col = if on { Color::rgb(255, 255, 255) } else if f > host_fps { DIM } else { TEXT };
                        s.text(sr, f.to_string(), 13.0, col, Align::Center, Font::UiBold);
                    }
                }
                Row::Display => {
                    let hov = match self.hit() {
                        Some(Hit::Display(j)) => Some(j),
                        _ => None,
                    };
                    for (j, sr) in self.display_rects() {
                        let on = self.active_display == Some(j);
                        let bg = if on {
                            ACCENT
                        } else if hov == Some(j) {
                            TRACK.with_alpha(0.30)
                        } else {
                            TRACK.with_alpha(0.12)
                        };
                        s.fill(sr, 6.0, bg);
                        let col = if on { Color::rgb(255, 255, 255) } else { TEXT };
                        s.text(sr, (j + 1).to_string(), 13.0, col, Align::Center, Font::UiBold);
                    }
                }
                Row::MenuPos | Row::StatsPos => {
                    let cur = if row == Row::MenuPos { self.menu_pos } else { self.stats_pos };
                    let sel = Preset::matching(cur);
                    let hov = match self.hit() {
                        Some(Hit::Preset(r, j)) if r == row => Some(j),
                        _ => None,
                    };
                    for (j, sr) in self.preset_rects(row).into_iter().enumerate() {
                        let p = Preset::ALL.get(j).copied();
                        let on = p.is_some() && p == sel;
                        let bg = if on {
                            ACCENT
                        } else if hov == Some(j) {
                            TRACK.with_alpha(0.30)
                        } else {
                            TRACK.with_alpha(0.12)
                        };
                        s.fill(sr, 6.0, bg);
                        let t = p.map_or("\u{21BA}", |p| p.glyph());
                        s.text(sr, t, 14.0, if on { Color::rgb(255, 255, 255) } else { TEXT }, Align::Center, Font::UiBold);
                    }
                }
                Row::Shortcuts => {
                    let t = if self.show_shortcuts { "\u{25B4}" } else { "\u{25BE}" };
                    s.text(Rect::new(w.x, w.y, w.w - 12.0, w.h), t, 14.0, DIM, Align::Right, Font::Ui);
                }
                r => {
                    let on = self.toggle_state(r).unwrap_or(false);
                    Self::draw_toggle(s, w, on, enabled);
                }
            }
        }

        let mut y = self.row_rect(rows.len() - 1).y + ROW_H + 4.0;
        if self.show_shortcuts {
            for (k, d) in HELP_LINES {
                let k = if k.is_empty() { self.capture_key.as_str() } else { k };
                s.text(Rect::new(p.x + PAD + 4.0, y, 150.0, HELP_LINE_H), k, 12.0, ACCENT, Align::Left, Font::Mono);
                s.text(Rect::new(p.x + PAD + 160.0, y, p.w - 2.0 * PAD - 160.0, HELP_LINE_H), d, 12.5, TEXT, Align::Left, Font::Ui);
                y += HELP_LINE_H;
            }
            y += 10.0;
        }
        let footer = match self.host {
            Some(h) => format!(
                "Host : {} img/s max · {:.0} Mb/s max{}{}",
                h.host_fps,
                h.host_max_bitrate_kbps as f64 / 1000.0,
                h.bitrate_cap_kbps.map_or(String::new(), |k| format!(" · plafond {:.0} Mb/s", k as f64 / 1000.0)),
                h.fps_cap.map_or(String::new(), |f| format!(" · {f} img/s")),
            ),
            None => "Host : limites inconnues (host ancien ?)".into(),
        };
        s.text(Rect::new(p.x + PAD, y, p.w - 2.0 * PAD, FOOTER_H - 6.0), footer, 11.5, DIM, Align::Left, Font::Ui);
        if !self.gamepads.is_empty() {
            let r = Rect::new(p.x + PAD, y + FOOTER_H - 8.0, p.w - 2.0 * PAD, PADS_H);
            s.text(r, &self.gamepads, 11.5, DIM, Align::Left, Font::Ui);
        }
    }
}

/// Lignes du panneau de statistiques quand elles sont disponibles (voir `draw_stats`).
const STATS_LINES: usize = 11;

const HELP_LINES: [(&str, &str); 9] = [
    ("Ctrl+Maj+M", "Ouvrir / fermer ce menu"),
    ("Ctrl+Maj+S", "Statistiques à l'écran"),
    ("Ctrl+Maj+Q", "Déconnexion"),
    ("Ctrl+Maj+\u{2190}/\u{2192}", "Écran distant précédent / suivant"),
    ("F11", "Plein écran / fenêtré"),
    ("F10", "Statistiques dans le titre"),
    ("", "Capture souris (mode relatif)"),
    ("Échap", "Fermer le menu"),
    ("Alt+F4", "Quitter"),
];

/// Info-bulle d'un bouton d'écran (sous le libellé, ~26 caractères) : « 2: VG27A 2560x1440 » ;
/// un nom trop long est abrégé pour garder la résolution visible, `*` = écran principal.
pub fn display_hint(i: usize, d: &DisplayInfo) -> String {
    let dims = format!("{}x{}{}", d.width, d.height, if d.primary { "*" } else { "" });
    let name = if d.name.is_empty() { "Écran" } else { d.name.as_str() };
    let room = 26usize.saturating_sub(dims.len() + 4);
    let name: String = if name.chars().count() > room {
        name.chars().take(room.saturating_sub(1)).collect::<String>() + "\u{2026}"
    } else {
        name.to_string()
    };
    format!("{}: {name} {dims}", i + 1)
}

/// Identifiant mémorisé d'un écran du host : nom + taille + position (deux moniteurs du même
/// modèle ont le même nom).
pub fn display_key(d: &DisplayInfo) -> String {
    format!("{} {}x{} @{},{}", d.name, d.width, d.height, d.left, d.top)
}

fn row_label(r: Row, capture_key: &str) -> (&'static str, String) {
    let (l, h) = match r {
        Row::Fullscreen => ("Plein écran", "F11"),
        Row::Stats => ("Statistiques", "Ctrl+Maj+S"),
        Row::Volume => ("Volume", ""),
        Row::Bandwidth => ("Bande passante max", ""),
        Row::Fps => ("FPS max", ""),
        Row::Display => ("Écran", "Ctrl+Maj+\u{2190}/\u{2192}"),
        Row::Capture => ("Capture souris", ""),
        Row::KbdLock => ("Verrouiller le clavier", "Win, Alt+Tab -> host"),
        Row::Vsync => ("VSync", ""),
        Row::Button => ("Bouton flottant", "glisser pour déplacer"),
        Row::MenuPos => ("Position du menu", "ou glisser le titre"),
        Row::StatsPos => ("Position des stats", "ou glisser le panneau"),
        Row::Shortcuts => ("Raccourcis", ""),
        Row::Disconnect => ("Déconnexion", "Ctrl+Maj+Q"),
    };
    let h = if r == Row::Capture { capture_key.to_string() } else { h.to_string() };
    (l, h)
}

// ---------------------------------------------------------------------------------------------
// Adaptateur winit

/// Conversion des événements de la fenêtre pour le menu ; suit lui-même Ctrl/Maj/Alt (en plus de
/// `ModifiersChanged`) et avale le relâchement des touches de raccourci.
#[derive(Default)]
pub struct OverlayInput {
    ctrl: bool,
    shift: bool,
    alt: bool,
    winit_mods: (bool, bool, bool),
    swallowed: HashSet<KeyCode>,
}

/// Résultat d'un événement de fenêtre.
pub struct Outcome {
    /// Ne pas transmettre à l'`InputHandler` (donc jamais au host).
    pub consumed: bool,
    pub actions: Vec<Action>,
}

impl OverlayInput {
    pub fn window_event(&mut self, ov: &mut Overlay, we: &WindowEvent) -> Outcome {
        let s = ov.scale as f64;
        let mut out = Outcome { consumed: false, actions: vec![] };
        let ev = |ov: &mut Overlay, e: Ev, out: &mut Outcome| {
            let (c, a) = ov.on_ev(e);
            out.consumed |= c;
            out.actions.extend(a);
        };
        match we {
            WindowEvent::ModifiersChanged(m) => {
                let st = m.state();
                self.winit_mods = (st.control_key(), st.shift_key(), st.alt_key());
            }
            WindowEvent::Focused(false) => {
                self.ctrl = false;
                self.shift = false;
                self.alt = false;
                // Un relâchement perdu ne doit pas laisser un glisser en cours.
                ov.drag = None;
                ov.move_drag = None;
            }
            WindowEvent::Resized(sz) => ov.set_view(sz.width, sz.height, s),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let (w, h) = (ov.view_w * ov.scale, ov.view_h * ov.scale);
                ov.set_view(w as u32, h as u32, *scale_factor);
            }
            WindowEvent::CursorMoved { position, .. } => {
                ev(ov, Ev::Move((position.x / s) as f32, (position.y / s) as f32), &mut out)
            }
            WindowEvent::CursorLeft { .. } => ev(ov, Ev::Leave, &mut out),
            WindowEvent::MouseInput { state, button: WinButton::Left, .. } => match ov.cursor {
                Some((x, y)) if *state == ElementState::Pressed => ev(ov, Ev::Down(x, y), &mut out),
                _ if *state == ElementState::Released => ev(ov, Ev::Up, &mut out),
                _ => {}
            },
            WindowEvent::MouseInput { .. } => out.consumed = ov.menu_open,
            WindowEvent::MouseWheel { delta, .. } => {
                let d = match delta {
                    MouseScrollDelta::LineDelta(_, y) => *y,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 40.0) as f32,
                };
                ev(ov, Ev::Wheel(d), &mut out)
            }
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    out.consumed = ov.menu_open;
                    return out;
                };
                let pressed = event.state == ElementState::Pressed;
                match code {
                    KeyCode::ControlLeft | KeyCode::ControlRight => self.ctrl = pressed,
                    KeyCode::ShiftLeft | KeyCode::ShiftRight => self.shift = pressed,
                    KeyCode::AltLeft | KeyCode::AltRight => self.alt = pressed,
                    _ => {}
                }
                if self.swallowed.contains(&code) {
                    if !pressed {
                        self.swallowed.remove(&code);
                    }
                    out.consumed = true;
                    return out;
                }
                let (wc, ws, wa) = self.winit_mods;
                let hot = classify_hotkey(code, self.ctrl || wc, self.shift || ws, self.alt || wa);
                if pressed && !is_synthetic {
                    if let Some(h) = hot {
                        self.swallowed.insert(code);
                        out.consumed = true;
                        if !event.repeat {
                            out.actions.extend(ov.on_hotkey(h));
                        }
                        return out;
                    }
                }
                if ov.menu_open {
                    // Menu ouvert : tout le clavier va au menu (F11 reste au présentateur).
                    out.consumed = code != KeyCode::F11;
                    if pressed {
                        let nav = match code {
                            KeyCode::ArrowUp => Some(Nav::Up),
                            KeyCode::ArrowDown | KeyCode::Tab => Some(Nav::Down),
                            KeyCode::ArrowLeft => Some(Nav::Left),
                            KeyCode::ArrowRight => Some(Nav::Right),
                            KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space => Some(Nav::Activate),
                            KeyCode::Escape => Some(Nav::Close),
                            _ => None,
                        };
                        if let Some(n) = nav {
                            if !(event.repeat && matches!(n, Nav::Activate | Nav::Close)) {
                                ev(ov, Ev::Nav(n), &mut out);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        out
    }
}

#[cfg(test)]
#[path = "overlay_drag_tests.rs"]
mod drag_tests;

#[cfg(test)]
#[path = "overlay_display_tests.rs"]
mod display_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn ov() -> Overlay {
        let mut o = Overlay::new("pc", "Ctrl+Alt+Maj+G");
        o.set_view(1280, 720, 1.0);
        o.host = Some(HostInfo { bitrate_cap_kbps: None, fps_cap: None, host_max_bitrate_kbps: 50_000, host_fps: 60 });
        o.monitor_hz = 144;
        o
    }

    fn center(r: Rect) -> (f32, f32) {
        (r.x + r.w / 2.0, r.y + r.h / 2.0)
    }

    #[test]
    fn hotkeys_require_ctrl_shift_without_alt() {
        assert_eq!(classify_hotkey(KeyCode::KeyM, true, true, false), Some(Hot::ToggleMenu));
        assert_eq!(classify_hotkey(KeyCode::KeyS, true, true, false), Some(Hot::ToggleStats));
        assert_eq!(classify_hotkey(KeyCode::KeyQ, true, true, false), Some(Hot::Disconnect));
        assert_eq!(classify_hotkey(KeyCode::KeyM, true, false, false), None);
        assert_eq!(classify_hotkey(KeyCode::KeyM, false, true, false), None);
        assert_eq!(classify_hotkey(KeyCode::KeyM, true, true, true), None);
        assert_eq!(classify_hotkey(KeyCode::KeyG, true, true, false), None);
    }

    #[test]
    fn security_banner_shows_even_when_menu_closed_and_hides_when_cleared() {
        let mut o = ov();
        // Rien de visible au départ (menu fermé, souris loin).
        assert!(o.scene().is_none());
        // Bandeau de sécurité présent : la scène le dessine même menu fermé.
        o.security_banner = Some("Fenêtre administrateur au premier plan sur le host".into());
        assert!(o.is_visible());
        let s = o.scene().expect("scène visible avec le bandeau");
        let has_banner_text = s.items.iter().any(|it| matches!(
            it,
            grx_render::OverlayItem::Text { text, .. } if text.contains("Fenêtre administrateur")
        ));
        assert!(has_banner_text, "le bandeau doit contenir le texte de sécurité");
        // Situation revenue à la normale : le bandeau disparaît.
        o.security_banner = None;
        assert!(o.scene().is_none());
    }

    #[test]
    fn nothing_drawn_when_closed_and_cursor_away() {
        let mut o = ov();
        assert!(o.scene().is_none());
        o.on_ev(Ev::Move(640.0, 400.0));
        assert!(o.scene().is_none(), "aucun coût quand le menu est fermé");
        o.on_ev(Ev::Move(640.0, 10.0));
        assert!(o.scene().is_some(), "bouton flottant près du bord haut");
        o.button_enabled = false;
        assert!(o.scene().is_none(), "bouton masquable");
        o.button_enabled = true;
        o.capture = true;
        assert!(o.scene().is_none(), "pas de bouton en capture souris");
    }

    #[test]
    fn float_button_opens_menu_and_click_is_consumed() {
        let mut o = ov();
        o.on_ev(Ev::Move(640.0, 10.0));
        let (x, y) = center(o.float_rect());
        let (c, a) = o.on_ev(Ev::Down(x, y));
        assert!(c);
        assert_eq!(a, vec![], "ouverture au relâchement (l'appui peut commencer un déplacement)");
        assert_eq!(o.on_ev(Ev::Up), (true, vec![Action::MenuOpened]));
        assert!(o.menu_open);
        // Clic ailleurs quand le menu est fermé : pas consommé (va au host).
        let mut o = ov();
        assert_eq!(o.on_ev(Ev::Down(640.0, 500.0)), (false, vec![]));
    }

    #[test]
    fn menu_consumes_everything_and_closes_on_outside_click_or_escape() {
        let mut o = ov();
        assert_eq!(o.on_hotkey(Hot::ToggleMenu), vec![Action::MenuOpened]);
        assert!(o.on_ev(Ev::Move(5.0, 700.0)).0);
        assert!(o.on_ev(Ev::Wheel(1.0)).0);
        assert_eq!(o.on_ev(Ev::Down(5.0, 700.0)), (true, vec![Action::MenuClosed]));
        o.on_hotkey(Hot::ToggleMenu);
        assert_eq!(o.on_ev(Ev::Nav(Nav::Close)), (true, vec![Action::MenuClosed]));
        assert_eq!(o.on_hotkey(Hot::ToggleMenu), vec![Action::MenuOpened]);
        assert_eq!(o.on_hotkey(Hot::ToggleMenu), vec![Action::MenuClosed]);
        assert_eq!(o.on_hotkey(Hot::Disconnect), vec![Action::Disconnect]);
    }

    #[test]
    fn toggles_by_click_and_keyboard() {
        let mut o = ov();
        o.open_menu();
        let (x, y) = center(o.row_rect(o.row_index(Row::Fullscreen)));
        assert_eq!(o.on_ev(Ev::Down(x, y)).1, vec![Action::SetFullscreen(true)]);
        assert_eq!(o.on_ev(Ev::Nav(Nav::Activate)).1, vec![Action::SetFullscreen(false)]);
        // Navigation jusqu'à VSync puis droite = activer, droite encore = rien.
        while o.rows()[o.focus] != Row::Vsync {
            o.on_ev(Ev::Nav(Nav::Down));
        }
        assert_eq!(o.on_ev(Ev::Nav(Nav::Right)).1, vec![Action::SetVsync(true)]);
        assert_eq!(o.on_ev(Ev::Nav(Nav::Right)).1, vec![]);
        assert_eq!(o.on_ev(Ev::Nav(Nav::Left)).1, vec![Action::SetVsync(false)]);
        // Stats : sans action externe, juste l'état.
        let (x, y) = center(o.row_rect(o.row_index(Row::Stats)));
        o.on_ev(Ev::Down(x, y));
        assert!(o.show_stats);
        // Capture : différée à la fermeture.
        let (x, y) = center(o.row_rect(o.row_index(Row::Capture)));
        assert_eq!(o.on_ev(Ev::Down(x, y)).1, vec![]);
        assert!(o.capture);
        let (x, y) = center(o.row_rect(o.row_index(Row::KbdLock)));
        assert_eq!(o.on_ev(Ev::Down(x, y)).1, vec![Action::SetKbdLock(true)]);
        let (x, y) = center(o.row_rect(o.row_index(Row::Disconnect)));
        assert_eq!(o.on_ev(Ev::Down(x, y)).1, vec![Action::Disconnect]);
        // Wrap de la navigation.
        o.focus = 0;
        o.on_ev(Ev::Nav(Nav::Up));
        assert_eq!(o.rows()[o.focus], Row::Disconnect);
    }

    #[test]
    fn input_rows_disabled_without_input() {
        let mut o = ov();
        o.input_enabled = false;
        o.open_menu();
        let (x, y) = center(o.row_rect(o.row_index(Row::KbdLock)));
        assert_eq!(o.on_ev(Ev::Down(x, y)).1, vec![]);
        assert!(!o.kbd_lock);
    }

    #[test]
    fn bandwidth_slider_steps_and_sends_settings() {
        let mut o = ov();
        o.open_menu();
        let steps = o.bw_steps();
        assert_eq!(steps.last(), Some(&None));
        assert!(steps.iter().flatten().all(|&m| m < 50));
        // Clic au début de la piste = plus petit palier.
        let t = o.slider_rect(Row::Bandwidth).unwrap();
        let (c, a) = o.on_ev(Ev::Down(t.x, t.y + t.h / 2.0));
        assert!(c);
        assert_eq!(a, vec![Action::SendSettings(StreamSettings { max_bitrate_kbps: Some(2_000), max_fps: None })]);
        // Glisser jusqu'au bout = Auto.
        let (_, a) = o.on_ev(Ev::Move(t.x + t.w + 50.0, t.y));
        assert_eq!(a, vec![Action::SendSettings(StreamSettings::default())]);
        o.on_ev(Ev::Up);
        assert_eq!(o.on_ev(Ev::Move(t.x, t.y)).1, vec![], "plus de glissement après relâchement");
        // Clavier : gauche depuis Auto = dernier palier < max.
        o.focus = o.row_index(Row::Bandwidth);
        let (_, a) = o.on_ev(Ev::Nav(Nav::Left));
        assert_eq!(a, vec![Action::SendSettings(StreamSettings { max_bitrate_kbps: Some(40_000), max_fps: None })]);
        assert_eq!(o.bitrate_cap_mbps, Some(40));
        o.bitrate_cap_mbps = Some(2);
        assert_eq!(o.on_ev(Ev::Nav(Nav::Left)).1, vec![], "borné en bas");
    }

    #[test]
    fn fps_segments_follow_monitor_and_send_settings() {
        let mut o = ov();
        o.monitor_hz = 60;
        assert_eq!(o.fps_options(), vec![30, 60]);
        o.monitor_hz = 120;
        assert_eq!(o.fps_options(), vec![30, 60, 120]);
        o.monitor_hz = 144;
        assert_eq!(o.fps_options(), vec![30, 60, 120, 144]);
        o.open_menu();
        let (f, r) = o.segment_rects()[0];
        assert_eq!(f, 30);
        let (x, y) = center(r);
        assert_eq!(o.on_ev(Ev::Down(x, y)).1, vec![Action::SendSettings(StreamSettings { max_bitrate_kbps: None, max_fps: Some(30) })]);
        assert_eq!(o.on_ev(Ev::Down(x, y)).1, vec![], "même choix : rien à envoyer");
        o.focus = o.row_index(Row::Fps);
        assert_eq!(o.on_ev(Ev::Nav(Nav::Right)).1, vec![Action::SendSettings(StreamSettings { max_bitrate_kbps: None, max_fps: Some(60) })]);
    }

    #[test]
    fn volume_slider_mute_and_wheel() {
        let mut o = ov();
        o.open_menu();
        let m = o.mute_rect();
        let (x, y) = center(m);
        assert_eq!(o.on_ev(Ev::Down(x, y)).1, vec![Action::SetVolume(0.0)]);
        assert!(o.muted);
        let t = o.slider_rect(Row::Volume).unwrap();
        let (_, a) = o.on_ev(Ev::Down(t.x + t.w / 2.0, t.y + 5.0));
        assert_eq!(a, vec![Action::SetVolume(0.5)], "toucher le curseur rétablit le son");
        o.on_ev(Ev::Up);
        o.on_ev(Ev::Move(t.x + 2.0, t.y + 5.0));
        let (_, a) = o.on_ev(Ev::Wheel(1.0));
        assert_eq!(a, vec![Action::SetVolume(0.55)]);
        o.audio_enabled = false;
        o.focus = o.row_index(Row::Volume);
        assert_eq!(o.on_ev(Ev::Nav(Nav::Activate)).1, vec![]);
    }

    #[test]
    fn scene_contains_menu_and_stats() {
        let mut o = ov();
        o.show_stats = true;
        o.stats = Some(StatsSnapshot { fps: 59.6, width: 2560, height: 1440, codec: "HEVC".into(), ..Default::default() });
        let s = o.scene().unwrap();
        let texts: Vec<String> = s
            .items
            .iter()
            .filter_map(|i| if let grx_render::OverlayItem::Text { text, .. } = i { Some(text.clone()) } else { None })
            .collect();
        assert!(texts.iter().any(|t| t == "2560x1440 HEVC"), "{texts:?}");
        o.open_menu();
        o.show_shortcuts = true;
        let s = o.scene().unwrap();
        let has = |w: &str| s.items.iter().any(|i| matches!(i, grx_render::OverlayItem::Text { text, .. } if text.contains(w)));
        assert!(has("Bande passante max") && has("Déconnexion") && has("Ouvrir / fermer ce menu") && has("Ctrl+Alt+Maj+G"));
        // Tout le menu tient dans une fenêtre 720p.
        assert!(o.panel_rect().y + o.panel_rect().h <= 720.0);
    }

    #[test]
    fn layout_survives_tiny_and_hidpi_windows() {
        let mut o = ov();
        o.set_view(100, 50, 2.0);
        o.open_menu();
        let _ = o.scene();
        for x in (0..200).step_by(7) {
            for y in (0..800).step_by(11) {
                let _ = o.on_ev(Ev::Move(x as f32, y as f32));
                let _ = o.on_ev(Ev::Wheel(-1.0));
            }
        }
        o.set_view(0, 0, f64::NAN);
        let _ = o.scene();
    }
}
