//! Cote HOST : [`InputInjector`], l'objet qui rejoue les [`InputEvent`] recus du client.
//!
//! L'injecteur est generique sur deux "sinks" (clavier+souris, manettes) pour etre testable
//! sans toucher a la machine :
//! * [`InputSink`] : implemente par [`crate::sendinput::SendInputSink`] (Windows, `SendInput`).
//! * [`GamepadSink`] : implemente par [`crate::gamepad::VigemGamepads`] (ViGEmBus) ou
//!   [`crate::gamepad::NullGamepads`] (stub documente si ViGEm est absent).
//!
//! Protections :
//! * **Suivi des touches/boutons enfonces** + [`InputInjector::release_all`] (appele aussi au `Drop`)
//!   pour ne jamais laisser de touche coincee apres une deconnexion ou un crash du flux.
//! * **Limitation de debit** (seau a jetons). Les evenements de *relachement* (touche haut, bouton
//!   haut) ne sont jamais limites, sinon la limitation pourrait elle-meme coincer une touche.
//! * Plafond de touches maintenues simultanement, scancodes invalides rejetes, interrupteurs
//!   par categorie (invite en lecture seule : clavier/souris/manette desactivables).

use std::collections::HashSet;
use std::time::Instant;

use thiserror::Error;

use crate::event::{GamepadKind, GamepadState, InputEvent, MouseButton, VirtualPad, MAX_GAMEPADS};

#[derive(Debug, Error)]
pub enum InjectError {
    #[error("echec SendInput : {0}")]
    SendInput(String),
    #[error("manette indisponible : {0}")]
    Gamepad(String),
}

/// Rendu d'un appel a [`InputInjector::inject`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// L'evenement a ete injecte.
    Injected,
    /// Ignore volontairement (categorie desactivee, mouvement nul, scancode invalide, trop de touches).
    Ignored,
    /// Rejete par la limitation de debit.
    RateLimited,
}

/// Sortie clavier + souris. Toutes les methodes sont des appels bas niveau, sans politique.
pub trait InputSink {
    fn key(&mut self, scancode: u8, extended: bool, pressed: bool) -> Result<(), InjectError>;
    fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<(), InjectError>;
    fn mouse_move_abs(&mut self, x: u16, y: u16) -> Result<(), InjectError>;
    fn mouse_button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError>;
    fn wheel(&mut self, horizontal: i16, vertical: i16) -> Result<(), InjectError>;
}

/// Sortie manettes (une manette virtuelle par index 0..=3).
pub trait GamepadSink {
    /// Applique l'etat complet ; branche la manette virtuelle si necessaire.
    fn update(&mut self, state: &GamepadState) -> Result<(), InjectError>;
    /// Debranche la manette `index` (sans effet si absente).
    fn disconnect(&mut self, index: u8);
    /// Choisit le type de manette virtuelle de `index` pour les prochaines mises a jour ; si une
    /// manette d'un autre type est deja branchee a cet index, elle est debranchee. Defaut : sans effet.
    fn set_virtual(&mut self, _index: u8, _virt: VirtualPad) {}
}

/// Politique du host pour le type de manette virtuelle (`grx-host run --gamepad-type`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GamepadPolicy {
    /// Suit la demande du client (Xbox 360 par defaut, DualShock 4 si le client le demande).
    #[default]
    Auto,
    /// Toujours une Xbox 360 virtuelle.
    Xbox,
    /// Toujours une DualShock 4 virtuelle.
    Ds4,
}

impl GamepadPolicy {
    /// Manette virtuelle retenue pour une manette physique `kind` dont le client demande `wanted`.
    pub fn resolve(self, _kind: GamepadKind, wanted: VirtualPad) -> VirtualPad {
        match self {
            GamepadPolicy::Auto => wanted,
            GamepadPolicy::Xbox => VirtualPad::Xbox360,
            GamepadPolicy::Ds4 => VirtualPad::DualShock4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct InjectorConfig {
    /// Debit soutenu maximal (evenements/s hors relachements). 0 = illimite.
    pub max_events_per_sec: u32,
    /// Rafale tolérée au-dela du debit soutenu.
    pub burst: u32,
    /// Nombre maximal de touches clavier maintenues en meme temps.
    pub max_held_keys: usize,
    pub allow_keyboard: bool,
    pub allow_mouse: bool,
    pub allow_gamepad: bool,
    /// Filtre les touches Windows gauche/droite (scancode etendu `E0 5B` / `E0 5C`) : un client ne
    /// peut alors pas ouvrir le menu Demarrer ni declencher les raccourcis `Win+...` du host.
    pub block_win_key: bool,
    /// Type de manette virtuelle (voir [`GamepadPolicy`]).
    pub gamepad_policy: GamepadPolicy,
}

impl Default for InjectorConfig {
    fn default() -> Self {
        Self {
            // Une souris 1000 Hz + clavier + manettes tiennent largement ; le client fusionne
            // les deltas (MouseAccumulator) donc 4000/s laisse de la marge sans autoriser l'inondation.
            max_events_per_sec: 4000,
            burst: 512,
            max_held_keys: 32,
            allow_keyboard: true,
            allow_mouse: true,
            allow_gamepad: true,
            block_win_key: false,
            gamepad_policy: GamepadPolicy::Auto,
        }
    }
}

/// Seau a jetons.
#[derive(Debug)]
struct TokenBucket {
    rate: f64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(rate: u32, burst: u32, now: Instant) -> Self {
        let capacity = burst.max(1) as f64;
        Self { rate: rate as f64, capacity, tokens: capacity, last: now }
    }

    fn try_take(&mut self, now: Instant) -> bool {
        if self.rate == 0.0 {
            return true; // illimite
        }
        let dt = now.saturating_duration_since(self.last);
        self.last = self.last.max(now);
        self.tokens = (self.tokens + dt.as_secs_f64() * self.rate).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

fn button_bit(b: MouseButton) -> u8 {
    1 << b.to_u8()
}

const ALL_BUTTONS: [MouseButton; 5] =
    [MouseButton::Left, MouseButton::Right, MouseButton::Middle, MouseButton::X1, MouseButton::X2];

pub struct InputInjector<K: InputSink, G: GamepadSink> {
    cfg: InjectorConfig,
    kb: K,
    pad: G,
    bucket: TokenBucket,
    held_keys: HashSet<(u8, bool)>,
    held_buttons: u8,
    active_pads: [bool; MAX_GAMEPADS],
    dropped: u64,
}

#[cfg(windows)]
impl InputInjector<crate::sendinput::SendInputSink, crate::gamepad::VigemGamepads> {
    /// Injecteur reel : `SendInput` + ViGEmBus (connexion paresseuse a la premiere manette).
    pub fn new(cfg: InjectorConfig) -> Self {
        Self::with_sinks(cfg, crate::sendinput::SendInputSink::new(), crate::gamepad::VigemGamepads::new())
    }
}

impl<K: InputSink, G: GamepadSink> InputInjector<K, G> {
    pub fn with_sinks(cfg: InjectorConfig, kb: K, pad: G) -> Self {
        let bucket = TokenBucket::new(cfg.max_events_per_sec, cfg.burst, Instant::now());
        Self {
            cfg,
            kb,
            pad,
            bucket,
            held_keys: HashSet::new(),
            held_buttons: 0,
            active_pads: [false; MAX_GAMEPADS],
            dropped: 0,
        }
    }

    pub fn keyboard_mouse(&self) -> &K {
        &self.kb
    }

    pub fn keyboard_mouse_mut(&mut self) -> &mut K {
        &mut self.kb
    }

    pub fn gamepads(&self) -> &G {
        &self.pad
    }

    pub fn gamepads_mut(&mut self) -> &mut G {
        &mut self.pad
    }

    /// Nombre d'evenements rejetes par la limitation de debit depuis la creation.
    pub fn rate_limited_count(&self) -> u64 {
        self.dropped
    }

    pub fn held_key_count(&self) -> usize {
        self.held_keys.len()
    }

    pub fn held_buttons_mask(&self) -> u8 {
        self.held_buttons
    }

    /// Decode puis injecte un evenement brut venant du reseau. Toute donnee malformee renvoie `Ok(Ignored)`
    /// apres un `warn!` : un client hostile ne doit pas pouvoir faire echouer la boucle host.
    pub fn inject_bytes(&mut self, data: &[u8]) -> Result<Outcome, InjectError> {
        match InputEvent::decode(data) {
            Ok(ev) => self.inject(&ev),
            Err(e) => {
                log::warn!("evenement d'entree malforme ignore : {e}");
                Ok(Outcome::Ignored)
            }
        }
    }

    pub fn inject(&mut self, ev: &InputEvent) -> Result<Outcome, InjectError> {
        self.inject_at(ev, Instant::now())
    }

    /// Comme [`inject`](Self::inject) avec une horloge explicite (tests deterministes).
    pub fn inject_at(&mut self, ev: &InputEvent, now: Instant) -> Result<Outcome, InjectError> {
        // 1. Categories autorisees
        let allowed = match ev {
            InputEvent::Key { .. } => self.cfg.allow_keyboard,
            InputEvent::MouseMove { .. }
            | InputEvent::MouseAbs { .. }
            | InputEvent::MouseButton { .. }
            | InputEvent::Wheel { .. } => self.cfg.allow_mouse,
            InputEvent::Gamepad(_) | InputEvent::GamepadConnect { .. } | InputEvent::GamepadDisconnect { .. } => {
                self.cfg.allow_gamepad
            }
        };
        if !allowed {
            return Ok(Outcome::Ignored);
        }

        // 2. Les relachements ne sont jamais limites (anti-touche-coincee).
        let is_release = match *ev {
            InputEvent::Key { pressed, .. } | InputEvent::MouseButton { pressed, .. } => !pressed,
            // Le debranchement remet au neutre : jamais limite non plus.
            InputEvent::GamepadDisconnect { .. } => true,
            _ => false,
        };
        if !is_release && !self.bucket.try_take(now) {
            self.dropped += 1;
            return Ok(Outcome::RateLimited);
        }

        // 3. Injection + suivi d'etat
        match *ev {
            InputEvent::Key { scancode, extended, pressed } => {
                if !crate::event::is_valid_scancode(scancode) {
                    return Ok(Outcome::Ignored);
                }
                if self.cfg.block_win_key && extended && matches!(scancode, 0x5B | 0x5C) {
                    return Ok(Outcome::Ignored);
                }
                let id = (scancode, extended);
                if pressed {
                    if !self.held_keys.contains(&id) && self.held_keys.len() >= self.cfg.max_held_keys {
                        return Ok(Outcome::Ignored);
                    }
                    // On marque AVANT d'injecter : si SendInput echoue a moitie, release_all rattrape.
                    self.held_keys.insert(id);
                    self.kb.key(scancode, extended, true)?;
                } else {
                    // Un relachement de touche non suivie est quand meme injecte (inoffensif, et
                    // couvre le cas ou un `release_all` a deja vide l'etat).
                    self.held_keys.remove(&id);
                    self.kb.key(scancode, extended, false)?;
                }
            }
            InputEvent::MouseMove { dx, dy } => {
                if dx == 0 && dy == 0 {
                    return Ok(Outcome::Ignored);
                }
                self.kb.mouse_move_rel(dx as i32, dy as i32)?;
            }
            InputEvent::MouseAbs { x, y } => self.kb.mouse_move_abs(x, y)?,
            InputEvent::MouseButton { button, pressed } => {
                if pressed {
                    self.held_buttons |= button_bit(button);
                } else {
                    self.held_buttons &= !button_bit(button);
                }
                self.kb.mouse_button(button, pressed)?;
            }
            InputEvent::Wheel { horizontal, vertical } => {
                if horizontal == 0 && vertical == 0 {
                    return Ok(Outcome::Ignored);
                }
                self.kb.wheel(horizontal, vertical)?;
            }
            InputEvent::Gamepad(state) => {
                let i = state.index as usize;
                if i >= MAX_GAMEPADS {
                    return Ok(Outcome::Ignored);
                }
                self.pad.update(&state)?;
                self.active_pads[i] = true;
            }
            InputEvent::GamepadConnect { index, kind, virt } => {
                if index as usize >= MAX_GAMEPADS {
                    return Ok(Outcome::Ignored);
                }
                let v = self.cfg.gamepad_policy.resolve(kind, virt);
                log::info!("manette client {index} : {} -> virtuelle {v:?}", kind.label());
                self.pad.set_virtual(index, v);
            }
            InputEvent::GamepadDisconnect { index } => {
                let i = index as usize;
                if i >= MAX_GAMEPADS {
                    return Ok(Outcome::Ignored);
                }
                if std::mem::replace(&mut self.active_pads[i], false) {
                    let neutral = GamepadState { index, ..Default::default() };
                    if let Err(e) = self.pad.update(&neutral) {
                        log::warn!("manette {i} non remise au neutre : {e}");
                    }
                }
                self.pad.disconnect(index);
            }
        }
        Ok(Outcome::Injected)
    }

    /// Relache toutes les touches et boutons de souris enfonces et debranche les manettes
    /// virtuelles (apres les avoir remises au neutre). A appeler a la deconnexion du client ;
    /// appele aussi automatiquement au `Drop`. Best effort : les erreurs sont journalisees.
    pub fn release_all(&mut self) {
        let keys: Vec<(u8, bool)> = self.held_keys.drain().collect();
        for (sc, ext) in keys {
            if let Err(e) = self.kb.key(sc, ext, false) {
                log::warn!("release_all : relachement touche {sc:#04x} impossible : {e}");
            }
        }
        let held = std::mem::take(&mut self.held_buttons);
        for b in ALL_BUTTONS {
            if held & button_bit(b) != 0 {
                if let Err(e) = self.kb.mouse_button(b, false) {
                    log::warn!("release_all : relachement bouton {b:?} impossible : {e}");
                }
            }
        }
        for i in 0..MAX_GAMEPADS {
            if std::mem::replace(&mut self.active_pads[i], false) {
                let neutral = GamepadState { index: i as u8, ..Default::default() };
                if let Err(e) = self.pad.update(&neutral) {
                    log::warn!("release_all : manette {i} non remise au neutre : {e}");
                }
                self.pad.disconnect(i as u8);
            }
        }
    }
}

impl<K: InputSink, G: GamepadSink> Drop for InputInjector<K, G> {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        Key(u8, bool, bool),
        MoveRel(i32, i32),
        MoveAbs(u16, u16),
        Button(MouseButton, bool),
        Wheel(i16, i16),
        PadUpdate(GamepadState),
        PadDisconnect(u8),
        PadVirtual(u8, VirtualPad),
    }

    pub type Log = Rc<RefCell<Vec<Call>>>;

    pub struct MockKb(pub Log);
    pub struct MockPad(pub Log);

    impl InputSink for MockKb {
        fn key(&mut self, sc: u8, ext: bool, p: bool) -> Result<(), InjectError> {
            self.0.borrow_mut().push(Call::Key(sc, ext, p));
            Ok(())
        }
        fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
            self.0.borrow_mut().push(Call::MoveRel(dx, dy));
            Ok(())
        }
        fn mouse_move_abs(&mut self, x: u16, y: u16) -> Result<(), InjectError> {
            self.0.borrow_mut().push(Call::MoveAbs(x, y));
            Ok(())
        }
        fn mouse_button(&mut self, b: MouseButton, p: bool) -> Result<(), InjectError> {
            self.0.borrow_mut().push(Call::Button(b, p));
            Ok(())
        }
        fn wheel(&mut self, h: i16, v: i16) -> Result<(), InjectError> {
            self.0.borrow_mut().push(Call::Wheel(h, v));
            Ok(())
        }
    }

    impl GamepadSink for MockPad {
        fn update(&mut self, s: &GamepadState) -> Result<(), InjectError> {
            self.0.borrow_mut().push(Call::PadUpdate(*s));
            Ok(())
        }
        fn disconnect(&mut self, i: u8) {
            self.0.borrow_mut().push(Call::PadDisconnect(i));
        }
        fn set_virtual(&mut self, i: u8, v: VirtualPad) {
            self.0.borrow_mut().push(Call::PadVirtual(i, v));
        }
    }

    pub fn injector(cfg: InjectorConfig) -> (InputInjector<MockKb, MockPad>, Log) {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        (InputInjector::with_sinks(cfg, MockKb(log.clone()), MockPad(log.clone())), log)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use std::time::Duration;

    fn key(sc: u8, ext: bool, pressed: bool) -> InputEvent {
        InputEvent::Key { scancode: sc, extended: ext, pressed }
    }

    #[test]
    fn injects_each_kind() {
        let (mut inj, log) = injector(InjectorConfig::default());
        assert_eq!(inj.inject(&key(0x1E, false, true)).unwrap(), Outcome::Injected);
        inj.inject(&InputEvent::MouseMove { dx: 3, dy: -4 }).unwrap();
        inj.inject(&InputEvent::MouseAbs { x: 100, y: 200 }).unwrap();
        inj.inject(&InputEvent::MouseButton { button: MouseButton::Right, pressed: true }).unwrap();
        inj.inject(&InputEvent::Wheel { horizontal: 0, vertical: 120 }).unwrap();
        let g = GamepadState { index: 1, buttons: 0x1000, ..Default::default() };
        inj.inject(&InputEvent::Gamepad(g)).unwrap();
        assert_eq!(
            *log.borrow(),
            vec![
                Call::Key(0x1E, false, true),
                Call::MoveRel(3, -4),
                Call::MoveAbs(100, 200),
                Call::Button(MouseButton::Right, true),
                Call::Wheel(0, 120),
                Call::PadUpdate(g),
            ]
        );
        // nettoyage fait au Drop : verifie plus bas
    }

    #[test]
    fn release_all_releases_everything_tracked() {
        let (mut inj, log) = injector(InjectorConfig::default());
        inj.inject(&key(0x1E, false, true)).unwrap();
        inj.inject(&key(0x1D, true, true)).unwrap();
        inj.inject(&key(0x1E, false, true)).unwrap(); // auto-repeat : pas de doublon d'etat
        inj.inject(&key(0x2A, false, true)).unwrap();
        inj.inject(&key(0x2A, false, false)).unwrap(); // relachee normalement
        inj.inject(&InputEvent::MouseButton { button: MouseButton::Left, pressed: true }).unwrap();
        inj.inject(&InputEvent::MouseButton { button: MouseButton::X1, pressed: true }).unwrap();
        inj.inject(&InputEvent::Gamepad(GamepadState { index: 2, buttons: 1, ..Default::default() })).unwrap();
        assert_eq!(inj.held_key_count(), 2);
        assert_eq!(inj.held_buttons_mask(), 0b01001);
        log.borrow_mut().clear();

        inj.release_all();
        let calls = log.borrow().clone();
        assert!(calls.contains(&Call::Key(0x1E, false, false)));
        assert!(calls.contains(&Call::Key(0x1D, true, false)));
        assert!(!calls.contains(&Call::Key(0x2A, false, false)), "deja relachee");
        assert!(calls.contains(&Call::Button(MouseButton::Left, false)));
        assert!(calls.contains(&Call::Button(MouseButton::X1, false)));
        assert!(!calls.contains(&Call::Button(MouseButton::Right, false)));
        // manette 2 : remise au neutre PUIS debranchee
        let n = calls.len();
        assert_eq!(calls[n - 2], Call::PadUpdate(GamepadState { index: 2, ..Default::default() }));
        assert_eq!(calls[n - 1], Call::PadDisconnect(2));
        assert_eq!(inj.held_key_count(), 0);
        assert_eq!(inj.held_buttons_mask(), 0);

        // idempotent
        log.borrow_mut().clear();
        inj.release_all();
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn drop_releases_everything() {
        let (mut inj, log) = injector(InjectorConfig::default());
        inj.inject(&key(0x11, false, true)).unwrap();
        inj.inject(&InputEvent::MouseButton { button: MouseButton::Middle, pressed: true }).unwrap();
        drop(inj);
        let calls = log.borrow();
        assert!(calls.contains(&Call::Key(0x11, false, false)));
        assert!(calls.contains(&Call::Button(MouseButton::Middle, false)));
    }

    #[test]
    fn rate_limit_drops_presses_but_never_releases() {
        let cfg = InjectorConfig { max_events_per_sec: 100, burst: 10, ..Default::default() };
        let (mut inj, log) = injector(cfg);
        let t0 = Instant::now();
        let mut injected = 0;
        let mut limited = 0;
        for i in 0..100 {
            match inj.inject_at(&InputEvent::MouseMove { dx: 1, dy: i }, t0).unwrap() {
                Outcome::Injected => injected += 1,
                Outcome::RateLimited => limited += 1,
                Outcome::Ignored => {}
            }
        }
        // dy == 0 est ignore ? non : dx=1 donc jamais nul
        assert_eq!(injected, 10);
        assert_eq!(limited, 90);
        assert_eq!(inj.rate_limited_count(), 90);

        // un relachement passe malgre le seau vide
        assert_eq!(inj.inject_at(&key(0x1E, false, false), t0).unwrap(), Outcome::Injected);
        assert_eq!(
            inj.inject_at(&InputEvent::MouseButton { button: MouseButton::Left, pressed: false }, t0).unwrap(),
            Outcome::Injected
        );
        // une pression est refusee
        assert_eq!(inj.inject_at(&key(0x1E, false, true), t0).unwrap(), Outcome::RateLimited);
        assert_eq!(inj.held_key_count(), 0, "une pression limitee ne doit pas etre suivie");
        let _ = log;

        // 55 ms plus tard : 5 jetons rendus
        let t1 = t0 + Duration::from_millis(55);
        let mut ok = 0;
        for _ in 0..20 {
            if inj.inject_at(&InputEvent::MouseMove { dx: 1, dy: 1 }, t1).unwrap() == Outcome::Injected {
                ok += 1;
            }
        }
        assert_eq!(ok, 5);
        // l'horloge qui recule ne panique pas
        let _ = inj.inject_at(&InputEvent::MouseMove { dx: 1, dy: 1 }, t0);
    }

    #[test]
    fn unlimited_when_rate_zero() {
        let cfg = InjectorConfig { max_events_per_sec: 0, ..Default::default() };
        let (mut inj, _log) = injector(cfg);
        let t = Instant::now();
        for _ in 0..10_000 {
            assert_eq!(inj.inject_at(&InputEvent::MouseMove { dx: 1, dy: 1 }, t).unwrap(), Outcome::Injected);
        }
    }

    #[test]
    fn caps_held_keys() {
        let cfg = InjectorConfig { max_held_keys: 3, max_events_per_sec: 0, ..Default::default() };
        let (mut inj, _log) = injector(cfg);
        for sc in 1..=3u8 {
            assert_eq!(inj.inject(&key(sc, false, true)).unwrap(), Outcome::Injected);
        }
        assert_eq!(inj.inject(&key(4, false, true)).unwrap(), Outcome::Ignored);
        // la repetition d'une touche deja enfoncee reste autorisee
        assert_eq!(inj.inject(&key(2, false, true)).unwrap(), Outcome::Injected);
        // relacher libere de la place
        inj.inject(&key(1, false, false)).unwrap();
        assert_eq!(inj.inject(&key(4, false, true)).unwrap(), Outcome::Injected);
    }

    #[test]
    fn ignores_noops_and_invalid() {
        let (mut inj, log) = injector(InjectorConfig::default());
        assert_eq!(inj.inject(&InputEvent::MouseMove { dx: 0, dy: 0 }).unwrap(), Outcome::Ignored);
        assert_eq!(inj.inject(&InputEvent::Wheel { horizontal: 0, vertical: 0 }).unwrap(), Outcome::Ignored);
        assert_eq!(inj.inject(&key(0, false, true)).unwrap(), Outcome::Ignored);
        assert_eq!(inj.inject(&key(0xE0, false, true)).unwrap(), Outcome::Ignored);
        let bad_pad = InputEvent::Gamepad(GamepadState { index: 9, ..Default::default() });
        assert_eq!(inj.inject(&bad_pad).unwrap(), Outcome::Ignored);
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn category_switches() {
        let cfg = InjectorConfig { allow_keyboard: false, allow_mouse: false, allow_gamepad: false, ..Default::default() };
        let (mut inj, log) = injector(cfg);
        assert_eq!(inj.inject(&key(0x1E, false, true)).unwrap(), Outcome::Ignored);
        assert_eq!(inj.inject(&InputEvent::MouseMove { dx: 1, dy: 1 }).unwrap(), Outcome::Ignored);
        assert_eq!(
            inj.inject(&InputEvent::Gamepad(GamepadState::default())).unwrap(),
            Outcome::Ignored
        );
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn win_key_is_filtered_only_when_requested() {
        let cfg = InjectorConfig { block_win_key: true, ..Default::default() };
        let (mut inj, log) = injector(cfg);
        for sc in [0x5B, 0x5C] {
            assert_eq!(inj.inject(&key(sc, true, true)).unwrap(), Outcome::Ignored);
            assert_eq!(inj.inject(&key(sc, true, false)).unwrap(), Outcome::Ignored);
        }
        assert_eq!(inj.held_key_count(), 0);
        // Le meme scancode sans le prefixe etendu n'est pas la touche Windows.
        assert_eq!(inj.inject(&key(0x5B, false, true)).unwrap(), Outcome::Injected);
        assert_eq!(inj.inject(&key(0x1E, true, true)).unwrap(), Outcome::Injected);
        assert_eq!(*log.borrow(), vec![Call::Key(0x5B, false, true), Call::Key(0x1E, true, true)]);

        // Par defaut rien n'est filtre.
        let (mut inj, log) = injector(InjectorConfig::default());
        assert_eq!(inj.inject(&key(0x5B, true, true)).unwrap(), Outcome::Injected);
        assert_eq!(*log.borrow(), vec![Call::Key(0x5B, true, true)]);
    }

    #[test]
    fn inject_bytes_rejects_garbage_without_error() {
        let (mut inj, log) = injector(InjectorConfig::default());
        assert_eq!(inj.inject_bytes(&[0xFF, 1, 2, 3]).unwrap(), Outcome::Ignored);
        assert_eq!(inj.inject_bytes(&[]).unwrap(), Outcome::Ignored);
        assert!(log.borrow().is_empty());
        let bytes = key(0x1E, false, true).encode();
        assert_eq!(inj.inject_bytes(&bytes).unwrap(), Outcome::Injected);
        assert_eq!(*log.borrow(), vec![Call::Key(0x1E, false, true)]);
    }

    #[test]
    fn gamepad_connect_follows_policy_and_disconnect_unplugs() {
        use crate::event::{GamepadKind, VirtualPad};
        let connect = InputEvent::GamepadConnect { index: 1, kind: GamepadKind::DualSense, virt: VirtualPad::DualShock4 };
        for (policy, want) in [
            (GamepadPolicy::Auto, VirtualPad::DualShock4),
            (GamepadPolicy::Xbox, VirtualPad::Xbox360),
            (GamepadPolicy::Ds4, VirtualPad::DualShock4),
        ] {
            let (mut inj, log) = injector(InjectorConfig { gamepad_policy: policy, ..Default::default() });
            assert_eq!(inj.inject(&connect).unwrap(), Outcome::Injected);
            assert_eq!(log.borrow().last(), Some(&Call::PadVirtual(1, want)));
        }
        assert_eq!(
            GamepadPolicy::Ds4.resolve(GamepadKind::Xbox, VirtualPad::Xbox360),
            VirtualPad::DualShock4
        );

        let (mut inj, log) = injector(InjectorConfig::default());
        let st = GamepadState { index: 2, buttons: crate::event::xbutton::A, ..Default::default() };
        inj.inject(&InputEvent::Gamepad(st)).unwrap();
        log.borrow_mut().clear();
        inj.inject(&InputEvent::GamepadDisconnect { index: 2 }).unwrap();
        assert_eq!(
            *log.borrow(),
            vec![Call::PadUpdate(GamepadState { index: 2, ..Default::default() }), Call::PadDisconnect(2)]
        );
        // deja debranchee : plus de remise au neutre a la fin
        log.borrow_mut().clear();
        inj.release_all();
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn gamepad_disconnect_is_never_rate_limited_and_respects_category() {
        let cfg = InjectorConfig { max_events_per_sec: 1, burst: 1, ..Default::default() };
        let (mut inj, _log) = injector(cfg);
        let t = Instant::now();
        inj.inject_at(&InputEvent::Gamepad(GamepadState::default()), t).unwrap();
        assert_eq!(inj.inject_at(&InputEvent::Gamepad(GamepadState::default()), t).unwrap(), Outcome::RateLimited);
        assert_eq!(inj.inject_at(&InputEvent::GamepadDisconnect { index: 0 }, t).unwrap(), Outcome::Injected);

        let (mut inj, log) = injector(InjectorConfig { allow_gamepad: false, ..Default::default() });
        let c = InputEvent::GamepadConnect {
            index: 0,
            kind: crate::event::GamepadKind::Xbox,
            virt: crate::event::VirtualPad::Xbox360,
        };
        assert_eq!(inj.inject(&c).unwrap(), Outcome::Ignored);
        assert_eq!(inj.inject(&InputEvent::GamepadDisconnect { index: 0 }).unwrap(), Outcome::Ignored);
        assert!(log.borrow().is_empty());
    }
}
