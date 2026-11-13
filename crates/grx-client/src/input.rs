//! Entrées de `grx-client play` : événements winit -> [`InputEvent`] -> `ClientConnection::send_input`.
//!
//! Deux morceaux :
//!
//! * [`InputHandler`] vit dans le hook du présentateur (thread de la fenêtre). Il convertit
//!   clavier (scancodes physiques), souris (position absolue normalisée sur l'image vidéo, ou
//!   deltas bruts en mode capture), boutons et molette, suit l'état « enfoncé » et gère les
//!   raccourcis réservés du client (jamais transmis au host).
//! * [`InputSender`] est un thread qui fusionne les mouvements ([`MouseAccumulator`] + dernière
//!   position absolue) et les envoie à `interval` au plus ; les autres événements partent tout de
//!   suite, précédés d'un vidage des mouvements en attente pour garder l'ordre.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use grx_input::client::MouseAccumulator;
use grx_input::keymap::scancode_from_extended_u32;
use grx_input::{InputEvent, MouseButton};
use grx_render::winit::event::{
    DeviceEvent, ElementState, KeyEvent, MouseButton as WinButton, MouseScrollDelta, WindowEvent,
};
use grx_render::winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use grx_render::winit::platform::scancode::PhysicalKeyExtScancode;
use grx_render::winit::window::{CursorGrabMode, Window};
use grx_render::{PresenterEvent, PresenterHandle};

// ---------------------------------------------------------------------------------------------
// Thread d'envoi

pub enum Msg {
    /// Événement envoyé tel quel (après vidage des mouvements en attente).
    Event(InputEvent),
    /// Position absolue (seule la dernière est conservée entre deux envois).
    Abs(u16, u16),
    /// Delta relatif brut (accumulé entre deux envois).
    Rel(i32, i32),
    Quit,
}

pub struct InputSender {
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl InputSender {
    /// `send` reçoit un événement encodé et renvoie `false` si la connexion est fermée.
    pub fn spawn(interval: Duration, send: impl FnMut(&[u8]) -> bool + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("grx-client-input".into())
            .spawn(move || sender_loop(rx, interval, Batcher::new(send)))
            .expect("thread d'entrée");
        Self { tx, thread: Some(thread) }
    }

    pub fn sender(&self) -> Sender<Msg> {
        self.tx.clone()
    }

    /// Vide ce qui reste puis arrête le thread.
    pub fn finish(mut self) {
        let _ = self.tx.send(Msg::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Batcher<F> {
    send: F,
    acc: MouseAccumulator,
    rel_pending: bool,
    abs: Option<(u16, u16)>,
}

impl<F: FnMut(&[u8]) -> bool> Batcher<F> {
    fn new(send: F) -> Self {
        Self { send, acc: MouseAccumulator::new(), rel_pending: false, abs: None }
    }

    fn pending(&self) -> bool {
        self.rel_pending || self.abs.is_some()
    }

    fn flush(&mut self) {
        if let Some((x, y)) = self.abs.take() {
            (self.send)(&InputEvent::MouseAbs { x, y }.encode());
        }
        if std::mem::take(&mut self.rel_pending) {
            for ev in self.acc.drain() {
                (self.send)(&ev.encode());
            }
        }
    }

    fn event(&mut self, ev: InputEvent) {
        if matches!(ev, InputEvent::MouseButton { .. } | InputEvent::Wheel { .. }) {
            self.flush();
        }
        (self.send)(&ev.encode());
    }
}

fn sender_loop<F: FnMut(&[u8]) -> bool>(rx: mpsc::Receiver<Msg>, interval: Duration, mut b: Batcher<F>) {
    let mut next_flush = Instant::now();
    loop {
        let timeout =
            if b.pending() { next_flush.saturating_duration_since(Instant::now()) } else { Duration::from_millis(100) };
        match rx.recv_timeout(timeout) {
            Ok(Msg::Event(ev)) => b.event(ev),
            Ok(Msg::Abs(x, y)) => b.abs = Some((x, y)),
            Ok(Msg::Rel(dx, dy)) => {
                b.acc.add(dx, dy);
                b.rel_pending = true;
            }
            Ok(Msg::Quit) | Err(RecvTimeoutError::Disconnected) => {
                b.flush();
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if b.pending() && Instant::now() >= next_flush {
            b.flush();
            next_flush = Instant::now() + interval;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Raccourci de capture

/// Raccourci « modificateurs + touche » (touche physique, indépendante du layout).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub code: KeyCode,
}

impl Hotkey {
    fn matches(&self, code: KeyCode, m: ModifiersState) -> bool {
        code == self.code
            && (!self.ctrl || m.control_key())
            && (!self.alt || m.alt_key())
            && (!self.shift || m.shift_key())
    }
}

impl std::fmt::Display for Hotkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.ctrl {
            f.write_str("Ctrl+")?;
        }
        if self.alt {
            f.write_str("Alt+")?;
        }
        if self.shift {
            f.write_str("Maj+")?;
        }
        let name = format!("{:?}", self.code);
        f.write_str(name.strip_prefix("Key").filter(|k| k.len() == 1).unwrap_or(&name))
    }
}

impl FromStr for Hotkey {
    type Err = String;

    /// `ctrl+alt+shift+g`, `f9`, `scrolllock`, ... (F10 et F11 sont réservées).
    fn from_str(s: &str) -> Result<Self, String> {
        let mut h = Hotkey { ctrl: false, alt: false, shift: false, code: KeyCode::KeyG };
        let mut code = None;
        for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
            match part.as_str() {
                "ctrl" | "control" => h.ctrl = true,
                "alt" => h.alt = true,
                "shift" | "maj" => h.shift = true,
                "" => return Err(format!("raccourci invalide : « {s} »")),
                k => {
                    if code.replace(key_by_name(k).ok_or_else(|| format!("touche inconnue : « {k} »"))?).is_some() {
                        return Err("une seule touche (en plus des modificateurs) est permise".into());
                    }
                }
            }
        }
        h.code = code.ok_or("il faut une touche en plus des modificateurs, ex. ctrl+alt+shift+g")?;
        if matches!(h.code, KeyCode::F10 | KeyCode::F11) {
            return Err("F10 (stats) et F11 (plein écran) sont déjà réservées".into());
        }
        Ok(h)
    }
}

fn key_by_name(n: &str) -> Option<KeyCode> {
    use KeyCode::*;
    const LETTERS: [KeyCode; 26] = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR,
        KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    const FKEYS: [KeyCode; 12] = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
    let b = n.as_bytes();
    if b.len() == 1 && b[0].is_ascii_lowercase() {
        return Some(LETTERS[(b[0] - b'a') as usize]);
    }
    if let Some(num) = n.strip_prefix('f').and_then(|v| v.parse::<usize>().ok()) {
        if (1..=12).contains(&num) {
            return Some(FKEYS[num - 1]);
        }
    }
    Some(match n {
        "scrolllock" => ScrollLock,
        "pause" => Pause,
        "insert" => Insert,
        "home" => Home,
        "end" => End,
        "pageup" => PageUp,
        "pagedown" => PageDown,
        "backquote" | "grave" => Backquote,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Conversion des événements de la fenêtre

/// Ce que le hook doit faire suite à un événement.
#[derive(Default)]
pub struct Reaction {
    /// F10 : bascule l'affichage des stats dans le titre.
    pub toggle_stats: bool,
    /// Le mode capture souris a changé (nouvelle valeur).
    pub capture: Option<bool>,
}

pub struct InputHandler {
    tx: Option<Sender<Msg>>,
    handle: PresenterHandle,
    hotkey: Hotkey,
    mods: ModifiersState,
    pressed_keys: HashSet<(u8, bool)>,
    /// Touches réservées au client dont on avale aussi le relâchement.
    swallowed: HashSet<(u8, bool)>,
    buttons: HashSet<MouseButton>,
    capture: bool,
    cursor: Option<(f64, f64)>,
    rel_frac: (f64, f64),
}

impl InputHandler {
    /// `tx = None` : aucune entrée n'est envoyée (`--no-input`), seuls F10/F11 restent actifs.
    pub fn new(tx: Option<Sender<Msg>>, handle: PresenterHandle, hotkey: Hotkey) -> Self {
        Self {
            tx,
            handle,
            hotkey,
            mods: ModifiersState::empty(),
            pressed_keys: HashSet::new(),
            swallowed: HashSet::new(),
            buttons: HashSet::new(),
            capture: false,
            cursor: None,
            rel_frac: (0.0, 0.0),
        }
    }

    pub fn handle(&mut self, ev: &PresenterEvent, win: &Window) -> Reaction {
        let mut r = Reaction::default();
        match ev {
            PresenterEvent::Window(we) => self.window_event(we, win, &mut r),
            PresenterEvent::Device(DeviceEvent::MouseMotion { delta }) if self.capture => {
                // Les deltas bruts peuvent être fractionnaires : on garde le reste.
                self.rel_frac.0 += delta.0;
                self.rel_frac.1 += delta.1;
                let (ix, iy) = (self.rel_frac.0.trunc(), self.rel_frac.1.trunc());
                self.rel_frac.0 -= ix;
                self.rel_frac.1 -= iy;
                if ix != 0.0 || iy != 0.0 {
                    self.send(Msg::Rel(ix as i32, iy as i32));
                }
            }
            _ => {}
        }
        r
    }

    fn send(&self, m: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(m);
        }
    }

    fn send_ev(&self, e: InputEvent) {
        self.send(Msg::Event(e));
    }

    fn window_event(&mut self, we: &WindowEvent, win: &Window, r: &mut Reaction) {
        match we {
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::Focused(false) => {
                self.release_all();
                self.set_capture(false, win, r);
            }
            WindowEvent::CloseRequested => self.release_all(),
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => self.key(event, *is_synthetic, win, r),
            WindowEvent::CursorMoved { position, .. } => self.cursor_moved(position.x, position.y),
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    WinButton::Left => MouseButton::Left,
                    WinButton::Right => MouseButton::Right,
                    WinButton::Middle => MouseButton::Middle,
                    WinButton::Back => MouseButton::X1,
                    WinButton::Forward => MouseButton::X2,
                    WinButton::Other(_) => return,
                };
                if *state == ElementState::Pressed {
                    if self.tx.is_some() && (self.capture || self.inside()) && self.buttons.insert(b) {
                        self.send_ev(InputEvent::MouseButton { button: b, pressed: true });
                    }
                } else if self.buttons.remove(&b) {
                    self.send_ev(InputEvent::MouseButton { button: b, pressed: false });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.tx.is_some() && (self.capture || self.inside()) {
                    let (x, y) = match delta {
                        MouseScrollDelta::LineDelta(x, y) => (x * 120.0, y * 120.0),
                        MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
                    };
                    let c = |v: f32| v.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                    let (h, v) = (c(x), c(y));
                    if h != 0 || v != 0 {
                        self.send_ev(InputEvent::Wheel { horizontal: h, vertical: v });
                    }
                }
            }
            _ => {}
        }
    }

    /// Le curseur est-il sur l'image vidéo (bandes noires exclues) ?
    fn inside(&self) -> bool {
        self.cursor.is_some_and(|(x, y)| self.handle.window_to_video(x, y).is_some())
    }

    fn cursor_moved(&mut self, x: f64, y: f64) {
        self.cursor = Some((x, y));
        if self.capture || self.tx.is_none() {
            return;
        }
        let q = |u: f64| (u * 65535.0).round().clamp(0.0, 65535.0) as u16;
        if let Some((u, v)) = self.handle.window_to_video(x, y) {
            self.send(Msg::Abs(q(u as f64), q(v as f64)));
        } else if !self.buttons.is_empty() {
            // Glisser hors de l'image : on colle la position au bord pour ne pas perdre le geste.
            if let Some((rx, ry, rw, rh)) = self.handle.video_rect() {
                let u = ((x - rx as f64) / rw as f64).clamp(0.0, 1.0);
                let v = ((y - ry as f64) / rh as f64).clamp(0.0, 1.0);
                self.send(Msg::Abs(q(u), q(v)));
            }
        }
    }

    fn key(&mut self, ke: &KeyEvent, synthetic: bool, win: &Window, r: &mut Reaction) {
        let pressed = ke.state == ElementState::Pressed;
        let code = match ke.physical_key {
            PhysicalKey::Code(c) => Some(c),
            _ => None,
        };
        let sc = ke.physical_key.to_scancode().and_then(scancode_from_extended_u32);

        // Touche réservée déjà enfoncée : ni répétitions ni relâchement ne partent vers le host.
        if let Some(k) = sc {
            if self.swallowed.contains(&k) {
                if !pressed {
                    self.swallowed.remove(&k);
                }
                return;
            }
        }
        if pressed && !synthetic {
            let capture_hk = self.tx.is_some() && code.is_some_and(|c| self.hotkey.matches(c, self.mods));
            let reserved = capture_hk
                || matches!(code, Some(KeyCode::F10 | KeyCode::F11))
                // Alt+F4 ferme la fenêtre du client, pas une appli du host.
                || (code == Some(KeyCode::F4) && self.mods.alt_key());
            if reserved {
                if !ke.repeat {
                    if code == Some(KeyCode::F10) {
                        r.toggle_stats = true;
                    }
                    if capture_hk {
                        let on = !self.capture;
                        self.set_capture(on, win, r);
                    }
                }
                if let Some(k) = sc {
                    self.swallowed.insert(k);
                }
                return;
            }
        }

        if self.tx.is_none() {
            return;
        }
        let Some((scancode, extended)) = sc else { return };
        if pressed {
            // Les presses synthétiques (touches déjà tenues au retour du focus) ne sont pas les nôtres.
            if !synthetic && self.pressed_keys.insert((scancode, extended)) {
                self.send_ev(InputEvent::Key { scancode, extended, pressed: true });
            }
        } else if self.pressed_keys.remove(&(scancode, extended)) {
            self.send_ev(InputEvent::Key { scancode, extended, pressed: false });
        }
    }

    fn set_capture(&mut self, on: bool, win: &Window, r: &mut Reaction) {
        if on == self.capture {
            return;
        }
        if on {
            let g = win
                .set_cursor_grab(CursorGrabMode::Confined)
                .or_else(|_| win.set_cursor_grab(CursorGrabMode::Locked));
            if let Err(e) = g {
                eprintln!("capture souris impossible : {e}");
                return;
            }
            win.set_cursor_visible(false);
        } else {
            let _ = win.set_cursor_grab(CursorGrabMode::None);
            win.set_cursor_visible(true);
        }
        self.capture = on;
        self.rel_frac = (0.0, 0.0);
        r.capture = Some(on);
        eprintln!(
            "{} (raccourci : {})",
            if on { "souris capturée (mode relatif)" } else { "souris libérée (mode absolu)" },
            self.hotkey
        );
    }

    /// Capture souris relative active ?
    pub fn is_captured(&self) -> bool {
        self.capture
    }

    /// Active/désactive la capture souris (menu de l'overlay). Renvoie l'état effectif.
    pub fn set_capture_mode(&mut self, on: bool, win: &Window) -> bool {
        let mut r = Reaction::default();
        self.set_capture(on, win, &mut r);
        self.capture
    }

    /// Relâche tout ce que le host croit enfoncé (perte de focus, fermeture).
    pub fn release_all(&mut self) {
        for (scancode, extended) in std::mem::take(&mut self.pressed_keys) {
            self.send_ev(InputEvent::Key { scancode, extended, pressed: false });
        }
        for button in std::mem::take(&mut self.buttons) {
            self.send_ev(InputEvent::MouseButton { button, pressed: false });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn hotkey_parsing() {
        let h: Hotkey = "ctrl+alt+shift+g".parse().unwrap();
        assert_eq!(h, Hotkey { ctrl: true, alt: true, shift: true, code: KeyCode::KeyG });
        assert_eq!("F9".parse::<Hotkey>().unwrap().code, KeyCode::F9);
        assert_eq!("Ctrl+ScrollLock".parse::<Hotkey>().unwrap().code, KeyCode::ScrollLock);
        assert!("ctrl+alt".parse::<Hotkey>().is_err());
        assert!("f11".parse::<Hotkey>().is_err());
        assert!("f10".parse::<Hotkey>().is_err());
        assert!("a+b".parse::<Hotkey>().is_err());
        assert!("ctrl+zzz".parse::<Hotkey>().is_err());
        assert!("ctrl++g".parse::<Hotkey>().is_err());
    }

    #[test]
    fn hotkey_matching() {
        let h: Hotkey = "ctrl+shift+g".parse().unwrap();
        assert!(h.matches(KeyCode::KeyG, ModifiersState::CONTROL | ModifiersState::SHIFT));
        assert!(h.matches(KeyCode::KeyG, ModifiersState::CONTROL | ModifiersState::SHIFT | ModifiersState::ALT));
        assert!(!h.matches(KeyCode::KeyG, ModifiersState::CONTROL));
        assert!(!h.matches(KeyCode::KeyH, ModifiersState::CONTROL | ModifiersState::SHIFT));
    }

    fn run_sender(interval_ms: u64, script: impl FnOnce(&Sender<Msg>)) -> Vec<InputEvent> {
        let out = Arc::new(Mutex::new(Vec::new()));
        let o = out.clone();
        let s = InputSender::spawn(Duration::from_millis(interval_ms), move |b| {
            o.lock().unwrap().push(InputEvent::decode(b).unwrap());
            true
        });
        script(&s.sender());
        s.finish();
        let v = out.lock().unwrap().clone();
        v
    }

    #[test]
    fn sender_coalesces_moves_and_keeps_order() {
        let ev = run_sender(1000, |tx| {
            // Premier mouvement : part tout de suite (aucun envoi récent) ; les suivants fusionnent.
            tx.send(Msg::Rel(1, 1)).unwrap();
            thread::sleep(Duration::from_millis(50));
            for _ in 0..10 {
                tx.send(Msg::Rel(2, 0)).unwrap();
                tx.send(Msg::Abs(10, 20)).unwrap();
            }
            tx.send(Msg::Event(InputEvent::MouseButton { button: MouseButton::Left, pressed: true })).unwrap();
            tx.send(Msg::Event(InputEvent::Key { scancode: 0x1E, extended: false, pressed: true })).unwrap();
        });
        assert_eq!(
            ev,
            vec![
                InputEvent::MouseMove { dx: 1, dy: 1 },
                // vidage avant le bouton : position puis delta cumulé, dans cet ordre
                InputEvent::MouseAbs { x: 10, y: 20 },
                InputEvent::MouseMove { dx: 20, dy: 0 },
                InputEvent::MouseButton { button: MouseButton::Left, pressed: true },
                InputEvent::Key { scancode: 0x1E, extended: false, pressed: true },
            ]
        );
    }

    #[test]
    fn sender_rate_is_bounded() {
        let ev = run_sender(8, |tx| {
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_millis(200) {
                tx.send(Msg::Rel(1, 0)).unwrap();
                thread::sleep(Duration::from_micros(200));
            }
        });
        // 200 ms à <= 125 Hz : au plus ~26 messages (marge), bien moins que les ~1000 d'entrée
        assert!(ev.len() <= 30 && ev.len() >= 10, "{}", ev.len());
        let total: i32 = ev.iter().map(|e| if let InputEvent::MouseMove { dx, .. } = e { *dx as i32 } else { 0 }).sum();
        assert!(total > 100, "aucun mouvement perdu : {total}");
    }
}
