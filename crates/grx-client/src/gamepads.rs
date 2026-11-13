//! Manettes physiques du client : lecture, normalisation et envoi au host.
//!
//! Trois sources, combinées par un fil unique `grx-client-pad` (≈ 250 Hz) :
//! * **XInput** (Xbox 360 / One / Series et compatibles XInput) : `xinput1_4.dll` chargée
//!   dynamiquement, `XInputGetStateEx` (bouton Guide) si disponible, vibrations par `XInputSetState`.
//! * **HID Sony** (DualShock 4, DualSense, USB et Bluetooth) : crate `hidapi` en implémentation
//!   Windows native (pur Rust, aucune bibliothèque C), décodeur des rapports dans
//!   [`grx_input::sony`], vibrations par rapports de sortie.
//! * **gilrs** (Windows.Gaming.Input) pour le reste (Switch Pro, manettes génériques ayant une
//!   correspondance SDL connue) ; les manettes déjà vues par XInput (`is_ff_supported`) ou Sony
//!   sont ignorées ici pour éviter les doublons. Pas de vibration sur cette voie.
//!
//! Chaque manette reçoit à sa connexion le plus petit index libre 0..=3 (stable tant qu'elle est
//! branchée, libéré au débranchement : [`grx_input::padlogic::SlotAllocator`]). Le host reçoit
//! `GamepadConnect` (type + manette virtuelle souhaitée), puis les états (zones mortes appliquées,
//! envoi sur changement à 250 Hz max + rafraîchissement toutes les 100 ms :
//! [`grx_input::padlogic::SendScheduler`]), enfin `GamepadDisconnect`. Quand la fenêtre perd le
//! focus ou que le menu de l'overlay est ouvert ([`PadShared::set_active`]), l'état neutre est envoyé.
//!
//! Boucle locale (host sur la même machine) : les manettes virtuelles créées par le host seraient
//! relues par le client ; seules les manettes présentes dans la première seconde sont alors prises.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use grx_input::padlogic::{normalize, SendScheduler, SlotAllocator};
use grx_input::{xbutton, GamepadKind, GamepadState, InputEvent, Rumble, VirtualPad, MAX_GAMEPADS};

use crate::input::Msg;

/// Manette virtuelle demandée au host (`play --gamepad-as`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VirtualPref {
    /// Xbox 360 pour toutes (compatibilité maximale).
    #[default]
    Xbox,
    /// DualShock 4 pour les manettes Sony, Xbox 360 pour les autres.
    Auto,
    /// DualShock 4 pour toutes.
    Ds4,
}

impl VirtualPref {
    pub fn for_kind(self, kind: GamepadKind) -> VirtualPad {
        match self {
            VirtualPref::Xbox => VirtualPad::Xbox360,
            VirtualPref::Ds4 => VirtualPad::DualShock4,
            VirtualPref::Auto if kind.is_sony() => VirtualPad::DualShock4,
            VirtualPref::Auto => VirtualPad::Xbox360,
        }
    }
}

impl std::str::FromStr for VirtualPref {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "xbox" => Ok(VirtualPref::Xbox),
            "auto" => Ok(VirtualPref::Auto),
            "ds4" => Ok(VirtualPref::Ds4),
            _ => Err(format!("« {s} » : attendu xbox, auto ou ds4")),
        }
    }
}

/// Manette simulée (option cachée `play --sim-gamepad`, tests de bout en bout sans matériel).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimPad {
    Xbox,
    Ds4,
    DualSense,
}

impl std::str::FromStr for SimPad {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "xbox" => Ok(SimPad::Xbox),
            "ds4" => Ok(SimPad::Ds4),
            "dualsense" | "ds5" => Ok(SimPad::DualSense),
            _ => Err(format!("« {s} » : attendu xbox, ds4 ou dualsense")),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PadOpts {
    pub pref: VirtualPref,
    pub sim: Option<SimPad>,
    /// Lit aussi les manettes physiques quand `sim` est donné.
    pub sim_with_physical: bool,
    /// Le host est sur cette machine (voir l'en-tête du module).
    pub loopback: bool,
    /// Journalise les états envoyés (diagnostic).
    pub verbose: bool,
}

/// Manette affichée par l'overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PadInfo {
    pub index: u8,
    pub kind: GamepadKind,
    pub name: String,
    pub source: &'static str,
}

/// État partagé avec la fenêtre (focus, menu) et l'overlay.
#[derive(Default)]
pub struct PadShared {
    inactive: AtomicBool,
    list: Mutex<Vec<PadInfo>>,
}

impl PadShared {
    /// Faux : les manettes envoient l'état neutre (fenêtre sans focus, menu ouvert).
    pub fn set_active(&self, on: bool) {
        self.inactive.store(!on, Ordering::Relaxed);
    }

    pub fn is_active(&self) -> bool {
        !self.inactive.load(Ordering::Relaxed)
    }

    pub fn list(&self) -> Vec<PadInfo> {
        self.list.lock().unwrap().clone()
    }

    fn set_list(&self, l: Vec<PadInfo>) {
        let mut g = self.list.lock().unwrap();
        if *g != l {
            *g = l;
        }
    }
}

/// Ligne d'état pour l'overlay : « Manettes : 1 Xbox (XInput), 2 DualSense (USB) ».
pub fn status_line(list: &[PadInfo]) -> String {
    if list.is_empty() {
        return "Manettes : aucune détectée".into();
    }
    let parts: Vec<String> =
        list.iter().map(|p| format!("{} {} ({})", p.index + 1, p.kind.label(), p.source)).collect();
    format!("Manettes : {}", parts.join(", "))
}

// ─────────────────────────────── sources ───────────────────────────────

/// Une manette vue par une source à un instant donné.
#[derive(Clone, Debug)]
struct Seen {
    key: String,
    kind: GamepadKind,
    name: String,
    source: &'static str,
    /// Index ignoré.
    state: GamepadState,
}

trait Source {
    /// Ajoute à `out` les manettes branchées et leur état courant.
    fn poll(&mut self, now: Instant, out: &mut Vec<Seen>);
    /// Applique une vibration (0..=255) à la manette `key` si elle appartient à cette source.
    fn rumble(&mut self, _key: &str, _large: u8, _small: u8) {}
}

// ── XInput ──

#[cfg(windows)]
mod xinput {
    use super::*;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct XGamepad {
        buttons: u16,
        lt: u8,
        rt: u8,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct XState {
        packet: u32,
        pad: XGamepad,
    }
    #[repr(C)]
    struct XVibration {
        left: u16,
        right: u16,
    }
    type GetState = unsafe extern "system" fn(u32, *mut XState) -> u32;
    type SetState = unsafe extern "system" fn(u32, *mut XVibration) -> u32;
    const ERROR_SUCCESS: u32 = 0;
    /// Un emplacement vide coûte cher à interroger : au plus une fois par seconde.
    const EMPTY_SLOT_PERIOD: Duration = Duration::from_secs(1);

    pub struct XInput {
        get: GetState,
        set: Option<SetState>,
        ex: bool,
        connected: [bool; 4],
        next_probe: [Instant; 4],
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    impl XInput {
        pub fn load() -> Option<XInput> {
            for dll in ["xinput1_4.dll", "xinput1_3.dll", "xinput9_1_0.dll"] {
                let w = wide(dll);
                // SAFETY : chaîne terminée par un zéro ; la DLL reste chargée pour toute la durée du processus.
                let h = unsafe { LoadLibraryW(w.as_ptr()) };
                if h.is_null() {
                    continue;
                }
                // SAFETY : GetProcAddress avec un nom ou un ordinal (MAKEINTRESOURCE) ; signatures documentées.
                unsafe {
                    let ex = GetProcAddress(h, 100usize as *const u8); // XInputGetStateEx (bouton Guide)
                    let std = GetProcAddress(h, c"XInputGetState".as_ptr().cast());
                    let set = GetProcAddress(h, c"XInputSetState".as_ptr().cast());
                    let (get, ex) = match (ex, std) {
                        (Some(f), _) => {
                            (std::mem::transmute::<unsafe extern "system" fn() -> isize, GetState>(f), true)
                        }
                        (None, Some(f)) => {
                            (std::mem::transmute::<unsafe extern "system" fn() -> isize, GetState>(f), false)
                        }
                        (None, None) => continue,
                    };
                    let set = set.map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, SetState>(f));
                    let now = Instant::now();
                    return Some(XInput { get, set, ex, connected: [false; 4], next_probe: [now; 4] });
                }
            }
            None
        }
    }

    impl Source for XInput {
        fn poll(&mut self, now: Instant, out: &mut Vec<Seen>) {
            for slot in 0..4u32 {
                let i = slot as usize;
                if !self.connected[i] && now < self.next_probe[i] {
                    continue;
                }
                let mut st = XState::default();
                // SAFETY : pointeur valide vers une structure XINPUT_STATE.
                let r = unsafe { (self.get)(slot, &mut st) };
                self.connected[i] = r == ERROR_SUCCESS;
                if !self.connected[i] {
                    self.next_probe[i] = now + EMPTY_SLOT_PERIOD;
                    continue;
                }
                let mut buttons = st.pad.buttons & grx_input::event::XINPUT_BUTTONS_MASK;
                if !self.ex {
                    buttons &= !xbutton::GUIDE;
                }
                out.push(Seen {
                    key: format!("xinput:{slot}"),
                    kind: GamepadKind::Xbox,
                    name: format!("Manette XInput {}", slot + 1),
                    source: "XInput",
                    state: GamepadState {
                        index: 0,
                        buttons,
                        left_trigger: st.pad.lt,
                        right_trigger: st.pad.rt,
                        thumb_lx: st.pad.lx,
                        thumb_ly: st.pad.ly,
                        thumb_rx: st.pad.rx,
                        thumb_ry: st.pad.ry,
                    },
                });
            }
        }

        fn rumble(&mut self, key: &str, large: u8, small: u8) {
            let (Some(set), Some(slot)) = (self.set, key.strip_prefix("xinput:").and_then(|s| s.parse::<u32>().ok()))
            else {
                return;
            };
            let mut v = XVibration { left: large as u16 * 257, right: small as u16 * 257 };
            // SAFETY : pointeur valide vers XINPUT_VIBRATION.
            unsafe { set(slot, &mut v) };
        }
    }
}

// ── HID Sony ──

#[cfg(windows)]
mod sony_hid {
    use super::*;
    use grx_input::sony::{self, SonyLink, SonyModel, SONY_VID};
    use hidapi::{BusType, HidApi, HidDevice};

    const ENUM_PERIOD: Duration = Duration::from_secs(2);

    struct Dev {
        dev: HidDevice,
        model: SonyModel,
        link: SonyLink,
        name: String,
        state: GamepadState,
        seq: u8,
        failed: bool,
    }

    pub struct SonyHid {
        api: HidApi,
        devs: HashMap<String, Dev>,
        next_enum: Instant,
    }

    impl SonyHid {
        pub fn new() -> Result<SonyHid, String> {
            let api = HidApi::new().map_err(|e| e.to_string())?;
            Ok(SonyHid { api, devs: HashMap::new(), next_enum: Instant::now() })
        }

        fn enumerate(&mut self) {
            self.api.reset_devices().ok();
            if let Err(e) = self.api.add_devices(SONY_VID, 0) {
                eprintln!("manettes : énumération HID impossible : {e}");
                return;
            }
            let mut present = Vec::new();
            for info in self.api.device_list() {
                let Some(model) = sony::model_from_pid(info.product_id()) else { continue };
                // Collection « manette » uniquement (le DualSense expose aussi d'autres collections).
                if info.usage_page() != 0x01 || !matches!(info.usage(), 0x04 | 0x05) {
                    continue;
                }
                let key = format!("hid:{}", info.path().to_string_lossy());
                present.push(key.clone());
                if self.devs.contains_key(&key) {
                    continue;
                }
                let dev = match info.open_device(&self.api) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("manettes : ouverture de {:?} impossible : {e}", info.product_string());
                        continue;
                    }
                };
                let _ = dev.set_blocking_mode(false);
                let link =
                    if matches!(info.bus_type(), BusType::Bluetooth) { SonyLink::Bluetooth } else { SonyLink::Usb };
                if link == SonyLink::Bluetooth {
                    // Lire le rapport de calibration fait passer la manette en rapports complets (0x11 / 0x31).
                    let mut buf = [0u8; 64];
                    buf[0] = if model == SonyModel::DualSense { 0x05 } else { 0x02 };
                    let _ = dev.get_feature_report(&mut buf);
                }
                let label = if model == SonyModel::DualSense { "DualSense" } else { "DualShock 4" };
                let name = info.product_string().filter(|s| !s.is_empty()).unwrap_or(label).to_string();
                self.devs
                    .insert(key, Dev { dev, model, link, name, state: GamepadState::default(), seq: 0, failed: false });
            }
            self.devs.retain(|k, _| present.contains(k));
        }
    }

    impl Source for SonyHid {
        fn poll(&mut self, now: Instant, out: &mut Vec<Seen>) {
            if now >= self.next_enum {
                self.next_enum = now + ENUM_PERIOD;
                self.enumerate();
            }
            let mut buf = [0u8; 128];
            for (key, d) in self.devs.iter_mut() {
                // Vide la file : seul le dernier rapport compte.
                for _ in 0..64 {
                    match d.dev.read_timeout(&mut buf, 0) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Some((s, link)) = sony::parse_input_report(d.model, &buf[..n]) {
                                d.state = sony::to_gamepad_state(&s, 0);
                                d.link = link;
                            }
                        }
                        Err(_) => {
                            d.failed = true;
                            break;
                        }
                    }
                }
                if d.failed {
                    continue;
                }
                out.push(Seen {
                    key: key.clone(),
                    kind: if d.model == SonyModel::DualSense {
                        GamepadKind::DualSense
                    } else {
                        GamepadKind::DualShock4
                    },
                    name: d.name.clone(),
                    source: if d.link == SonyLink::Bluetooth { "Bluetooth" } else { "USB" },
                    state: d.state,
                });
            }
            // Manette arrachée : oubliée tout de suite, retrouvée à la prochaine énumération si elle revient.
            self.devs.retain(|_, d| !d.failed);
        }

        fn rumble(&mut self, key: &str, large: u8, small: u8) {
            if let Some(d) = self.devs.get_mut(key) {
                d.seq = d.seq.wrapping_add(1);
                let r = sony::rumble_report(d.model, d.link, large, small, d.seq);
                let _ = d.dev.write(&r);
            }
        }
    }
}

// ── gilrs (génériques, Switch Pro) ──

mod generic {
    use super::*;
    use gilrs::{Axis, Button, Gilrs, MappingSource};

    pub struct Generic {
        g: Gilrs,
    }

    impl Generic {
        pub fn new() -> Result<Generic, String> {
            Gilrs::new().map(|g| Generic { g }).map_err(|e| e.to_string())
        }
    }

    fn axis(v: f32) -> i16 {
        (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
    }

    fn trigger(v: f32) -> u8 {
        (v.clamp(0.0, 1.0) * 255.0).round() as u8
    }

    impl Source for Generic {
        fn poll(&mut self, _now: Instant, out: &mut Vec<Seen>) {
            while self.g.next_event().is_some() {}
            for (id, gp) in self.g.gamepads() {
                let vid = gp.vendor_id().unwrap_or(0);
                // XInput (déjà lu, vibrations comprises) et Sony (HID) : pas de doublon.
                if !gp.is_connected()
                    || gp.is_ff_supported()
                    || vid == grx_input::sony::SONY_VID
                    || gp.mapping_source() == MappingSource::None
                {
                    continue;
                }
                const MAP: [(Button, u16); 15] = [
                    (Button::South, xbutton::A),
                    (Button::East, xbutton::B),
                    (Button::West, xbutton::X),
                    (Button::North, xbutton::Y),
                    (Button::LeftTrigger, xbutton::LEFT_SHOULDER),
                    (Button::RightTrigger, xbutton::RIGHT_SHOULDER),
                    (Button::Select, xbutton::BACK),
                    (Button::Start, xbutton::START),
                    (Button::Mode, xbutton::GUIDE),
                    (Button::LeftThumb, xbutton::LEFT_THUMB),
                    (Button::RightThumb, xbutton::RIGHT_THUMB),
                    (Button::DPadUp, xbutton::DPAD_UP),
                    (Button::DPadDown, xbutton::DPAD_DOWN),
                    (Button::DPadLeft, xbutton::DPAD_LEFT),
                    (Button::DPadRight, xbutton::DPAD_RIGHT),
                ];
                let mut buttons = 0u16;
                for (b, x) in MAP {
                    if gp.is_pressed(b) {
                        buttons |= x;
                    }
                }
                let trig = |b: Button| gp.button_data(b).map_or(0.0, |d| d.value());
                out.push(Seen {
                    key: format!("gilrs:{}", usize::from(id)),
                    kind: if vid == 0x057E { GamepadKind::Switch } else { GamepadKind::Generic },
                    name: gp.name().to_string(),
                    source: "WGI",
                    state: GamepadState {
                        index: 0,
                        buttons,
                        left_trigger: trigger(trig(Button::LeftTrigger2)),
                        right_trigger: trigger(trig(Button::RightTrigger2)),
                        thumb_lx: axis(gp.value(Axis::LeftStickX)),
                        thumb_ly: axis(gp.value(Axis::LeftStickY)),
                        thumb_rx: axis(gp.value(Axis::RightStickX)),
                        thumb_ry: axis(gp.value(Axis::RightStickY)),
                    },
                });
            }
        }
    }
}

// ── manette simulée ──

struct Sim {
    kind: GamepadKind,
    t0: Instant,
    verbose: bool,
}

impl Sim {
    /// État scripté et déterministe en fonction du temps : A appuyé 300 ms toutes les 2 s, stick
    /// gauche en cercle (période 4 s), gâchette droite en rampe, croix haut de 1 s à 1,5 s (modulo 4 s).
    fn state_at(dt: Duration) -> GamepadState {
        let ms = dt.as_millis() as u64;
        let mut buttons = 0;
        if ms % 2000 < 300 {
            buttons |= xbutton::A;
        }
        if (1000..1500).contains(&(ms % 4000)) {
            buttons |= xbutton::DPAD_UP;
        }
        let a = (ms % 4000) as f64 / 4000.0 * std::f64::consts::TAU;
        GamepadState {
            index: 0,
            buttons,
            left_trigger: 0,
            right_trigger: ((ms % 1000) * 255 / 1000) as u8,
            thumb_lx: (a.cos() * 30000.0) as i16,
            thumb_ly: (a.sin() * 30000.0) as i16,
            thumb_rx: 0,
            thumb_ry: 0,
        }
    }
}

impl Source for Sim {
    fn poll(&mut self, now: Instant, out: &mut Vec<Seen>) {
        let dt = now.saturating_duration_since(self.t0);
        if dt < Duration::from_millis(300) {
            return; // branchée « à chaud » juste après le démarrage
        }
        out.push(Seen {
            key: "sim:0".into(),
            kind: self.kind,
            name: "Manette simulée".into(),
            source: "simulée",
            state: Self::state_at(dt),
        });
    }

    fn rumble(&mut self, key: &str, large: u8, small: u8) {
        if key == "sim:0" {
            println!("manette simulée : vibration reçue gros={large} petit={small}");
        } else if self.verbose {
            println!("vibration ignorée pour {key}");
        }
    }
}

// ─────────────────────────────── gestionnaire ───────────────────────────────

struct Attached {
    key: String,
    source: usize,
    sched: SendScheduler,
}

/// Logique sans E/S (testable) : diff des manettes vues, index, cadence, état neutre si inactif.
struct Manager {
    slots: SlotAllocator<String>,
    attached: [Option<Attached>; MAX_GAMEPADS],
    pref: VirtualPref,
    ignored: Vec<String>,
}

impl Manager {
    fn new(pref: VirtualPref) -> Self {
        Self { slots: SlotAllocator::new(), attached: [None, None, None, None], pref, ignored: Vec::new() }
    }

    /// `seen` : (index de la source, manette). Renvoie les événements à envoyer, dans l'ordre.
    fn step(&mut self, seen: &[(usize, Seen)], active: bool, now: Instant, out: &mut Vec<InputEvent>) {
        // Débranchements.
        for i in 0..MAX_GAMEPADS {
            let gone = self.attached[i].as_ref().is_some_and(|a| !seen.iter().any(|(_, s)| s.key == a.key));
            if gone {
                let a = self.attached[i].take().expect("vérifié");
                self.slots.detach(&a.key);
                out.push(InputEvent::Gamepad(GamepadState { index: i as u8, ..Default::default() }));
                out.push(InputEvent::GamepadDisconnect { index: i as u8 });
            }
        }
        // Branchements + états.
        for (src, s) in seen {
            let idx = match self.slots.index_of(&s.key) {
                Some(i) => i,
                None => match self.slots.attach(&s.key) {
                    Some(i) => {
                        let virt = self.pref.for_kind(s.kind);
                        out.push(InputEvent::GamepadConnect { index: i, kind: s.kind, virt });
                        self.attached[i as usize] = Some(Attached {
                            key: s.key.clone(),
                            source: *src,
                            sched: SendScheduler::new(
                                SendScheduler::DEFAULT_MIN_INTERVAL,
                                SendScheduler::DEFAULT_REFRESH,
                            ),
                        });
                        i
                    }
                    None => {
                        if !self.ignored.contains(&s.key) {
                            self.ignored.push(s.key.clone());
                            eprintln!("manettes : plus de {MAX_GAMEPADS} manettes, « {} » ignorée", s.name);
                        }
                        continue;
                    }
                },
            };
            let st = if active {
                normalize(GamepadState { index: idx, ..s.state })
            } else {
                GamepadState { index: idx, ..Default::default() }
            };
            if let Some(a) = self.attached[idx as usize].as_mut() {
                if let Some(x) = a.sched.offer(st, now) {
                    out.push(InputEvent::Gamepad(x));
                }
            }
        }
        for a in self.attached.iter_mut().flatten() {
            if let Some(x) = a.sched.poll(now) {
                out.push(InputEvent::Gamepad(x));
            }
        }
    }

    /// Fin de session : neutre + débranchement de tout ce qui est attaché.
    fn release_all(&mut self, out: &mut Vec<InputEvent>) {
        for i in 0..MAX_GAMEPADS {
            if let Some(a) = self.attached[i].take() {
                self.slots.detach(&a.key);
                out.push(InputEvent::Gamepad(GamepadState { index: i as u8, ..Default::default() }));
                out.push(InputEvent::GamepadDisconnect { index: i as u8 });
            }
        }
    }
}

/// Démarre le fil des manettes. `rumble` renvoie les vibrations reçues du host depuis le dernier appel.
pub fn spawn(
    tx: Sender<Msg>,
    shared: Arc<PadShared>,
    opts: PadOpts,
    stop: Arc<AtomicBool>,
    rumble: impl Fn() -> Vec<Rumble> + Send + 'static,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("grx-client-pad".into())
        .spawn(move || pad_loop(tx, shared, opts, stop, rumble))
        .expect("thread manettes")
}

fn open_sources(opts: &PadOpts) -> Vec<Box<dyn Source>> {
    let mut v: Vec<Box<dyn Source>> = Vec::new();
    if let Some(sim) = opts.sim {
        let kind = match sim {
            SimPad::Xbox => GamepadKind::Xbox,
            SimPad::Ds4 => GamepadKind::DualShock4,
            SimPad::DualSense => GamepadKind::DualSense,
        };
        println!("manettes : manette simulée ({}) active", kind.label());
        v.push(Box::new(Sim { kind, t0: Instant::now(), verbose: opts.verbose }));
        if !opts.sim_with_physical {
            return v;
        }
    }
    #[cfg(windows)]
    {
        match xinput::XInput::load() {
            Some(x) => v.push(Box::new(x)),
            None => eprintln!("manettes : XInput indisponible (manettes Xbox non lues)"),
        }
        match sony_hid::SonyHid::new() {
            Ok(s) => v.push(Box::new(s)),
            Err(e) => eprintln!("manettes : HID indisponible ({e}) : DualShock 4 / DualSense non lues"),
        }
    }
    match generic::Generic::new() {
        Ok(g) => v.push(Box::new(g)),
        Err(e) => eprintln!("manettes : couche générique (gilrs) indisponible : {e}"),
    }
    v
}

fn pad_loop(
    tx: Sender<Msg>,
    shared: Arc<PadShared>,
    opts: PadOpts,
    stop: Arc<AtomicBool>,
    rumble: impl Fn() -> Vec<Rumble>,
) {
    let mut sources = open_sources(&opts);
    let mut mgr = Manager::new(opts.pref);
    let t0 = Instant::now();
    let loopback_grace = Duration::from_secs(1);
    let mut loopback_known: Vec<String> = Vec::new();
    let mut loopback_warned: Vec<String> = Vec::new();
    let mut seen: Vec<(usize, Seen)> = Vec::new();
    let mut scratch = Vec::new();
    let mut events = Vec::new();
    let mut last_active = true;
    let send = |events: &mut Vec<InputEvent>, verbose: bool| -> bool {
        for e in events.drain(..) {
            if verbose {
                println!("manettes -> host : {e:?}");
            }
            if tx.send(Msg::Event(e)).is_err() {
                return false;
            }
        }
        true
    };

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        seen.clear();
        for (i, s) in sources.iter_mut().enumerate() {
            scratch.clear();
            s.poll(now, &mut scratch);
            seen.extend(scratch.drain(..).map(|x| (i, x)));
        }
        if opts.loopback && opts.sim.is_none() {
            // Boucle locale : n'accepte que les manettes présentes au démarrage.
            if now.saturating_duration_since(t0) < loopback_grace {
                for (_, s) in &seen {
                    if !loopback_known.contains(&s.key) {
                        loopback_known.push(s.key.clone());
                    }
                }
            }
            seen.retain(|(_, s)| {
                let ok = loopback_known.contains(&s.key);
                if !ok && !loopback_warned.contains(&s.key) {
                    loopback_warned.push(s.key.clone());
                    println!("manettes : boucle locale, « {} » apparue après le démarrage ignorée (manette virtuelle du host ?)", s.name);
                }
                ok
            });
        }
        let active = shared.is_active();
        if active != last_active {
            last_active = active;
            if opts.verbose {
                println!("manettes : {}", if active { "actives" } else { "au neutre (focus perdu ou menu ouvert)" });
            }
        }
        mgr.step(&seen, active, now, &mut events);
        if !send(&mut events, opts.verbose) {
            break;
        }

        // Vibrations reçues du host.
        for r in rumble() {
            if let Some(a) = mgr.attached.get(r.index as usize).and_then(|a| a.as_ref()) {
                if let Some(src) = sources.get_mut(a.source) {
                    src.rumble(&a.key, r.large, r.small);
                }
            }
        }

        let list: Vec<PadInfo> = mgr
            .attached
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                let a = a.as_ref()?;
                let s = seen.iter().find(|(_, s)| s.key == a.key)?;
                Some(PadInfo { index: i as u8, kind: s.1.kind, name: s.1.name.clone(), source: s.1.source })
            })
            .collect();
        shared.set_list(list);

        thread::sleep(Duration::from_millis(4));
    }

    // Arrêt : moteurs coupés, manettes virtuelles du host débranchées.
    for (i, a) in mgr.attached.iter().enumerate() {
        if let Some(a) = a {
            if let Some(src) = sources.get_mut(a.source) {
                src.rumble(&a.key, 0, 0);
            }
            let _ = i;
        }
    }
    mgr.release_all(&mut events);
    let _ = send(&mut events, opts.verbose);
}

/// `grx-client gamepads` : liste les manettes et affiche les changements d'état (normalisés)
/// pendant `secs` secondes. Aucune connexion, aucune vibration envoyée.
/// `rumble_test` : fait vibrer brièvement chaque manette détectée (gros moteur 200, petit 100, 0,5 s).
pub fn probe(secs: f64, sim: Option<SimPad>, rumble_test: bool) {
    let opts = PadOpts { sim, sim_with_physical: true, ..Default::default() };
    let mut sources = open_sources(&opts);
    let t0 = Instant::now();
    let mut last: HashMap<String, GamepadState> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut rumbling: Vec<(usize, String, Instant)> = Vec::new();
    let mut out = Vec::new();
    while t0.elapsed().as_secs_f64() < secs.max(0.1) {
        let now = Instant::now();
        let mut present = Vec::new();
        for (si, s) in sources.iter_mut().enumerate() {
            out.clear();
            s.poll(now, &mut out);
            for p in out.drain(..) {
                present.push(p.key.clone());
                if !names.contains_key(&p.key) {
                    println!("+ {} : {} [{}, {}]", p.key, p.name, p.kind.label(), p.source);
                    names.insert(p.key.clone(), p.name.clone());
                    if rumble_test {
                        println!("  vibration de test envoyée à {}", p.key);
                        s.rumble(&p.key, 200, 100);
                        rumbling.push((si, p.key.clone(), now));
                    }
                }
                let st = normalize(p.state);
                if last.get(&p.key) != Some(&st) {
                    println!(
                        "  {:>5.2} s {} : boutons {:#06x} LT {:>3} RT {:>3} G ({:>6}, {:>6}) D ({:>6}, {:>6})",
                        t0.elapsed().as_secs_f64(),
                        p.key,
                        st.buttons,
                        st.left_trigger,
                        st.right_trigger,
                        st.thumb_lx,
                        st.thumb_ly,
                        st.thumb_rx,
                        st.thumb_ry
                    );
                    last.insert(p.key.clone(), st);
                }
            }
        }
        rumbling.retain(|(si, key, t)| {
            let done = now.saturating_duration_since(*t) >= Duration::from_millis(500);
            if done {
                if let Some(s) = sources.get_mut(*si) {
                    s.rumble(key, 0, 0);
                }
            }
            !done
        });
        names.retain(|k, n| {
            let keep = present.contains(k);
            if !keep {
                println!("- {k} : {n} débranchée");
                last.remove(k);
            }
            keep
        });
        thread::sleep(Duration::from_millis(16));
    }
    if names.is_empty() {
        println!("Aucune manette détectée.");
    } else {
        println!("{} manette(s) détectée(s).", names.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(key: &str, kind: GamepadKind, buttons: u16) -> Seen {
        Seen {
            key: key.into(),
            kind,
            name: key.into(),
            source: "test",
            state: GamepadState { buttons, ..Default::default() },
        }
    }

    #[test]
    fn virtual_pref_choices() {
        assert_eq!(VirtualPref::default().for_kind(GamepadKind::DualSense), VirtualPad::Xbox360);
        assert_eq!(VirtualPref::Auto.for_kind(GamepadKind::DualSense), VirtualPad::DualShock4);
        assert_eq!(VirtualPref::Auto.for_kind(GamepadKind::DualShock4), VirtualPad::DualShock4);
        assert_eq!(VirtualPref::Auto.for_kind(GamepadKind::Xbox), VirtualPad::Xbox360);
        assert_eq!(VirtualPref::Ds4.for_kind(GamepadKind::Generic), VirtualPad::DualShock4);
        assert_eq!("AUTO".parse::<VirtualPref>(), Ok(VirtualPref::Auto));
        assert!("wii".parse::<VirtualPref>().is_err());
        assert_eq!("dualsense".parse::<SimPad>(), Ok(SimPad::DualSense));
    }

    #[test]
    fn manager_connects_sends_and_disconnects_with_stable_indexes() {
        let mut m = Manager::new(VirtualPref::Auto);
        let t = Instant::now();
        let mut out = Vec::new();
        let a = (0, seen("a", GamepadKind::Xbox, xbutton::A));
        let b = (1, seen("b", GamepadKind::DualSense, 0));
        m.step(&[a.clone(), b.clone()], true, t, &mut out);
        assert_eq!(
            out,
            vec![
                InputEvent::GamepadConnect { index: 0, kind: GamepadKind::Xbox, virt: VirtualPad::Xbox360 },
                InputEvent::Gamepad(GamepadState { index: 0, buttons: xbutton::A, ..Default::default() }),
                InputEvent::GamepadConnect { index: 1, kind: GamepadKind::DualSense, virt: VirtualPad::DualShock4 },
                InputEvent::Gamepad(GamepadState { index: 1, ..Default::default() }),
            ]
        );
        // a débranchée : neutre + Disconnect ; b garde l'index 1 ; une nouvelle prend l'index 0.
        out.clear();
        let c = (0, seen("c", GamepadKind::Generic, 0));
        m.step(&[b.clone(), c], true, t + Duration::from_millis(1), &mut out);
        assert_eq!(out[0], InputEvent::Gamepad(GamepadState { index: 0, ..Default::default() }));
        assert_eq!(out[1], InputEvent::GamepadDisconnect { index: 0 });
        assert_eq!(
            out[2],
            InputEvent::GamepadConnect { index: 0, kind: GamepadKind::Generic, virt: VirtualPad::Xbox360 }
        );
        assert_eq!(m.slots.index_of(&"b".to_string()), Some(1));
        // fin : tout est relâché
        out.clear();
        m.release_all(&mut out);
        assert_eq!(out.iter().filter(|e| matches!(e, InputEvent::GamepadDisconnect { .. })).count(), 2);
    }

    #[test]
    fn manager_sends_neutral_when_inactive_and_caps_at_four() {
        let mut m = Manager::new(VirtualPref::Xbox);
        let t = Instant::now();
        let mut out = Vec::new();
        let pads: Vec<(usize, Seen)> =
            (0..5).map(|i| (0, seen(&format!("p{i}"), GamepadKind::Xbox, xbutton::B))).collect();
        m.step(&pads, true, t, &mut out);
        assert_eq!(out.iter().filter(|e| matches!(e, InputEvent::GamepadConnect { .. })).count(), 4);
        out.clear();
        // Inactif (focus perdu) : chaque manette repasse au neutre (après l'intervalle minimal).
        m.step(&pads, false, t + Duration::from_millis(5), &mut out);
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(|e| matches!(e, InputEvent::Gamepad(s) if s.buttons == 0)));
        // Rien de neuf avant le rafraîchissement périodique.
        out.clear();
        m.step(&pads, false, t + Duration::from_millis(50), &mut out);
        assert!(out.is_empty());
        m.step(&pads, false, t + Duration::from_millis(110), &mut out);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn manager_applies_deadzone() {
        let mut m = Manager::new(VirtualPref::Xbox);
        let mut out = Vec::new();
        let mut s = seen("a", GamepadKind::Xbox, 0);
        s.state.thumb_lx = 900;
        s.state.left_trigger = 3;
        m.step(&[(0, s)], true, Instant::now(), &mut out);
        assert_eq!(out[1], InputEvent::Gamepad(GamepadState::default()));
    }

    #[test]
    fn sim_state_is_scripted() {
        let s = Sim::state_at(Duration::from_millis(100));
        assert_ne!(s.buttons & xbutton::A, 0);
        let s = Sim::state_at(Duration::from_millis(1200));
        assert_eq!(s.buttons & xbutton::A, 0);
        assert_ne!(s.buttons & xbutton::DPAD_UP, 0);
    }

    #[test]
    fn overlay_menu_shows_the_gamepad_line() {
        let mut o = crate::overlay::Overlay::new("host", "Ctrl+Alt+Maj+G");
        o.set_view(1280, 720, 1.0);
        let l = [PadInfo { index: 0, kind: GamepadKind::DualShock4, name: "d".into(), source: "Bluetooth" }];
        o.gamepads = status_line(&l);
        o.open_menu();
        let s = o.scene().unwrap();
        assert!(s.items.iter().any(|i| matches!(i, grx_render::OverlayItem::Text { text, .. } if text == "Manettes : 1 DualShock 4 (Bluetooth)")));
    }

    #[test]
    fn status_line_lists_pads() {
        assert_eq!(status_line(&[]), "Manettes : aucune détectée");
        let l = [
            PadInfo { index: 0, kind: GamepadKind::Xbox, name: "x".into(), source: "XInput" },
            PadInfo { index: 1, kind: GamepadKind::DualSense, name: "d".into(), source: "USB" },
        ];
        assert_eq!(status_line(&l), "Manettes : 1 Xbox (XInput), 2 DualSense (USB)");
    }
}
