//! grx-input : entrees clavier/souris/manette pour GamingRemoteExperience.
//!
//! * [`event`] : [`InputEvent`] + format binaire compact (`encode`/`decode`, strict, sans panique).
//! * [`keymap`], [`client`] : cote CLIENT, conversion VK/scancode, messages fenetre et Raw Input
//!   vers `InputEvent`.
//! * [`padlogic`], [`sony`] : cote CLIENT, manettes physiques (zones mortes, index stables, cadence
//!   d'envoi, decodeur des rapports HID DualShock 4 / DualSense et vibrations).
//! * [`ds4map`], [`rumble`] : DualShock 4 virtuelle (correspondance des boutons) et message de vibration.
//! * [`injector`], [`sendinput`], [`gamepad`] : cote HOST, injection `SendInput` + ViGEmBus avec
//!   suivi des touches enfoncees (`release_all`) et limitation de debit.
//!
//! Cote host (Windows) :
//! ```no_run
//! use grx_input::{InputInjector, InjectorConfig};
//! let mut inj = InputInjector::new(InjectorConfig::default());
//! // pour chaque datagramme d'entree dechiffre :
//! # let datagram: &[u8] = &[];
//! let _ = inj.inject_bytes(datagram);
//! // a la deconnexion du client :
//! inj.release_all();
//! ```

pub mod client;
pub mod ds4map;
pub mod event;
pub mod injector;
pub mod keymap;
pub mod padlogic;
pub mod rumble;
pub mod sony;

#[cfg(windows)]
pub mod gamepad;
#[cfg(windows)]
pub mod sendinput;

pub use event::{
    decode_batch, encode_batch, xbutton, DecodeError, GamepadKind, GamepadState, InputEvent, MouseButton,
    VirtualPad, MAX_ENCODED_LEN, MAX_GAMEPADS,
};
pub use injector::{GamepadPolicy, GamepadSink, InjectError, InjectorConfig, InputInjector, InputSink, Outcome};
pub use rumble::Rumble;

#[cfg(windows)]
pub use gamepad::{NullGamepads, VigemGamepads};
#[cfg(windows)]
pub use sendinput::{SendInputSink, INJECTED_MAGIC};

/// Injecteur reel Windows (SendInput + ViGEmBus).
#[cfg(windows)]
pub type WindowsInjector = InputInjector<SendInputSink, VigemGamepads>;
