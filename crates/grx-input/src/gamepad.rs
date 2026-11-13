//! Manettes virtuelles cote host.
//!
//! [`VigemGamepads`] cree une manette virtuelle (ViGEmBus, via le crate `vigem-client`) par index
//! 0..=3, branchee paresseusement a la premiere mise a jour de l'index. Le type est choisi par
//! index avec [`GamepadSink::set_virtual`] : Xbox 360 (defaut, compatibilite maximale) ou
//! DualShock 4 (correspondance des boutons dans [`crate::ds4map`] : A = Croix, B = Rond, ...,
//! Guide = PS, [`xbutton::TOUCHPAD`](crate::event::xbutton::TOUCHPAD) = clic du pave tactile).
//! Si ViGEmBus est absent ou refuse l'acces, la premiere mise a jour renvoie une erreur
//! explicite ; les suivantes echouent de la meme maniere sans retenter en boucle trop souvent
//! (nouvelle tentative de connexion au plus toutes les 5 s).
//!
//! **Vibrations** : pour une Xbox 360 virtuelle, ViGEm notifie les commandes moteurs envoyees par
//! le jeu (`XInputSetState`). Avec [`VigemGamepads::set_rumble_sink`], chaque changement est
//! pousse (sans bloquer, canal borne) comme [`Rumble`] ; la boucle reseau du host le transmet au
//! client. Le crate `vigem-client` 0.1 n'expose pas les notifications DualShock 4 : pas de
//! vibration en mode DS4 (evolution possible).
//!
//! [`NullGamepads`] est le stub documente : il accepte tout et ne fait rien (a utiliser quand
//! ViGEm n'est pas installe et que l'on veut tout de meme faire tourner le host).
//!
//! Notes : `Xbox360Wired` contient des HANDLE bruts et n'est donc pas `Send` ; creez et utilisez
//! l'injecteur sur un meme thread (ou enveloppez-le dans un thread dedie recevant les evenements
//! par canal). Un processus tue sans passer par `Drop` laisse la manette virtuelle branchee
//! jusqu'a retrait manuel (limite de ViGEm, voir la doc du crate).

use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vigem_client::{Client, DS4Report, DualShock4Wired, TargetId, XButtons, XGamepad, Xbox360Wired};

use crate::ds4map;
use crate::event::{GamepadState, VirtualPad, MAX_GAMEPADS, XINPUT_BUTTONS_MASK};
use crate::injector::{GamepadSink, InjectError};
use crate::rumble::Rumble;

const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// `ERROR_NO_MORE_ITEMS` : voir `update` (rapport mis en cache par ViGEmBus, pas une erreur).
const ERROR_NO_MORE_ITEMS: u32 = 259;

/// Convertit notre etat en rapport XInput (champs 1:1 ; le bit pave tactile, inconnu de XInput, est retire).
pub fn to_xgamepad(s: &GamepadState) -> XGamepad {
    XGamepad {
        buttons: XButtons(s.buttons & XINPUT_BUTTONS_MASK),
        left_trigger: s.left_trigger,
        right_trigger: s.right_trigger,
        thumb_lx: s.thumb_lx,
        thumb_ly: s.thumb_ly,
        thumb_rx: s.thumb_rx,
        thumb_ry: s.thumb_ry,
    }
}

/// Convertit notre etat en rapport DualShock 4 ViGEm (voir [`ds4map::to_ds4`]).
pub fn to_ds4_report(s: &GamepadState) -> DS4Report {
    let r = ds4map::to_ds4(s);
    DS4Report {
        thumb_lx: r.thumb_lx,
        thumb_ly: r.thumb_ly,
        thumb_rx: r.thumb_rx,
        thumb_ry: r.thumb_ry,
        buttons: r.buttons,
        special: r.special,
        trigger_l: r.trigger_l,
        trigger_r: r.trigger_r,
    }
}

enum Pad {
    X360(Xbox360Wired<Arc<Client>>),
    Ds4(DualShock4Wired<Arc<Client>>),
}

impl Pad {
    fn kind(&self) -> VirtualPad {
        match self {
            Pad::X360(_) => VirtualPad::Xbox360,
            Pad::Ds4(_) => VirtualPad::DualShock4,
        }
    }
}

pub struct VigemGamepads {
    client: Option<Arc<Client>>,
    pads: [Option<Pad>; MAX_GAMEPADS],
    want: [VirtualPad; MAX_GAMEPADS],
    rumble_tx: Option<SyncSender<Rumble>>,
    last_connect_attempt: Option<Instant>,
}

impl Default for VigemGamepads {
    fn default() -> Self {
        Self::new()
    }
}

impl VigemGamepads {
    /// Ne se connecte pas encore au bus : la connexion se fait a la premiere manette.
    pub fn new() -> Self {
        Self {
            client: None,
            pads: [None, None, None, None],
            want: [VirtualPad::Xbox360; MAX_GAMEPADS],
            rumble_tx: None,
            last_connect_attempt: None,
        }
    }

    /// Les vibrations des manettes Xbox 360 virtuelles branchees ENSUITE seront poussees dans ce canal
    /// (`try_send` : jamais bloquant, une vibration est perdue si le canal est plein).
    pub fn set_rumble_sink(&mut self, tx: SyncSender<Rumble>) {
        self.rumble_tx = Some(tx);
    }

    /// Verifie (et etablit) la connexion a ViGEmBus. Utile pour un diagnostic au demarrage du host.
    pub fn probe() -> Result<(), InjectError> {
        Client::connect().map(|_| ()).map_err(|e| InjectError::Gamepad(format!("ViGEmBus : {e}")))
    }

    fn client(&mut self) -> Result<Arc<Client>, InjectError> {
        if let Some(c) = &self.client {
            return Ok(c.clone());
        }
        if let Some(t) = self.last_connect_attempt {
            if t.elapsed() < RECONNECT_DELAY {
                return Err(InjectError::Gamepad("ViGEmBus indisponible (nouvelle tentative bientot)".into()));
            }
        }
        self.last_connect_attempt = Some(Instant::now());
        let c = Arc::new(
            Client::connect().map_err(|e| InjectError::Gamepad(format!("connexion a ViGEmBus impossible : {e}")))?,
        );
        self.client = Some(c.clone());
        Ok(c)
    }

    /// Nombre de manettes virtuellement branchees.
    pub fn plugged_count(&self) -> usize {
        self.pads.iter().flatten().count()
    }

    /// Type de la manette virtuelle branchee a `index`, s'il y en a une.
    pub fn plugged_kind(&self, index: u8) -> Option<VirtualPad> {
        self.pads.get(index as usize)?.as_ref().map(Pad::kind)
    }

    fn plug(&mut self, i: usize) -> Result<Pad, InjectError> {
        let client = self.client()?;
        match self.want[i] {
            VirtualPad::Xbox360 => {
                let mut pad = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
                pad.plugin().map_err(|e| InjectError::Gamepad(format!("plugin manette {i} : {e}")))?;
                pad.wait_ready().map_err(|e| InjectError::Gamepad(format!("manette {i} non prete : {e}")))?;
                if let Some(tx) = self.rumble_tx.clone() {
                    match pad.request_notification() {
                        Ok(req) => {
                            let index = i as u8;
                            let mut last: Option<(u8, u8)> = None;
                            // Le fil se termine tout seul quand la manette est debranchee (requete annulee).
                            let _ = req.spawn_thread(move |_, n| {
                                let v = (n.large_motor, n.small_motor);
                                if last != Some(v) {
                                    last = Some(v);
                                    let _ = tx.try_send(Rumble { index, large: v.0, small: v.1 });
                                }
                            });
                        }
                        Err(e) => log::warn!("vibrations de la manette {i} indisponibles : {e}"),
                    }
                }
                log::info!("manette virtuelle Xbox 360 branchee (index client {i})");
                Ok(Pad::X360(pad))
            }
            VirtualPad::DualShock4 => {
                let mut pad = DualShock4Wired::new(client, TargetId::DUALSHOCK4_WIRED);
                pad.plugin().map_err(|e| InjectError::Gamepad(format!("plugin DS4 {i} : {e}")))?;
                pad.wait_ready().map_err(|e| InjectError::Gamepad(format!("DS4 {i} non prete : {e}")))?;
                log::info!("manette virtuelle DualShock 4 branchee (index client {i})");
                Ok(Pad::Ds4(pad))
            }
        }
    }
}

impl GamepadSink for VigemGamepads {
    fn update(&mut self, state: &GamepadState) -> Result<(), InjectError> {
        let i = state.index as usize;
        if i >= MAX_GAMEPADS {
            return Err(InjectError::Gamepad(format!("index de manette invalide : {i}")));
        }
        if self.pads[i].is_none() {
            let pad = self.plug(i)?;
            self.pads[i] = Some(pad);
        }
        match self.pads[i].as_mut() {
            // ViGEmBus garde le rapport en cache et renvoie STATUS_NO_MORE_ENTRIES (259) quand le pilote
            // (XUSB ou HID) n'a pas encore de lecture en attente, typiquement juste après le branchement :
            // le rapport est livré à la lecture suivante (le client ViGEm officiel ignore aussi ce code).
            Some(Pad::X360(p)) => match p.update(&to_xgamepad(state)) {
                Ok(()) | Err(vigem_client::Error::WinError(ERROR_NO_MORE_ITEMS)) => Ok(()),
                Err(e) => Err(InjectError::Gamepad(format!("update manette {i} : {e}"))),
            },
            Some(Pad::Ds4(p)) => match p.update(&to_ds4_report(state)) {
                Ok(()) | Err(vigem_client::Error::WinError(ERROR_NO_MORE_ITEMS)) => Ok(()),
                Err(e) => Err(InjectError::Gamepad(format!("update DS4 {i} : {e}"))),
            },
            None => Ok(()),
        }
    }

    fn disconnect(&mut self, index: u8) {
        if let Some(slot) = self.pads.get_mut(index as usize) {
            // Drop de Xbox360Wired / DualShock4Wired = unplug (le fil de notification se termine)
            if slot.take().is_some() {
                log::info!("manette virtuelle {index} debranchee");
                if let Some(tx) = &self.rumble_tx {
                    let _ = tx.try_send(Rumble { index, large: 0, small: 0 });
                }
            }
        }
    }

    fn set_virtual(&mut self, index: u8, virt: VirtualPad) {
        let i = index as usize;
        if i >= MAX_GAMEPADS {
            return;
        }
        self.want[i] = virt;
        if self.pads[i].as_ref().is_some_and(|p| p.kind() != virt) {
            self.disconnect(index); // rebranchee avec le bon type a la prochaine mise a jour
        }
    }
}

/// Stub : accepte les mises a jour et ne fait rien. Documente pour les machines sans ViGEmBus.
#[derive(Debug, Default)]
pub struct NullGamepads;

impl GamepadSink for NullGamepads {
    fn update(&mut self, _state: &GamepadState) -> Result<(), InjectError> {
        Ok(())
    }
    fn disconnect(&mut self, _index: u8) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::xbutton;

    #[test]
    fn xgamepad_mapping_is_one_to_one() {
        let s = GamepadState {
            index: 0,
            buttons: xbutton::A | xbutton::LEFT_SHOULDER,
            left_trigger: 10,
            right_trigger: 20,
            thumb_lx: -1,
            thumb_ly: 2,
            thumb_rx: -3,
            thumb_ry: 4,
        };
        let x = to_xgamepad(&s);
        assert_eq!(x.buttons.raw, 0x1100);
        assert_eq!((x.left_trigger, x.right_trigger), (10, 20));
        assert_eq!((x.thumb_lx, x.thumb_ly, x.thumb_rx, x.thumb_ry), (-1, 2, -3, 4));
    }

    #[test]
    fn touchpad_bit_is_dropped_for_xbox_and_kept_for_ds4() {
        let s = GamepadState { buttons: xbutton::TOUCHPAD | xbutton::A | xbutton::GUIDE, ..Default::default() };
        assert_eq!(to_xgamepad(&s).buttons.raw, xbutton::A | xbutton::GUIDE);
        let d = to_ds4_report(&s);
        assert_eq!(d.special, ds4map::ds4::SPECIAL_PS | ds4map::ds4::SPECIAL_TOUCHPAD);
        assert_ne!(d.buttons & ds4map::ds4::CROSS, 0);
    }

    #[test]
    fn rejects_bad_index_without_touching_the_bus() {
        let mut g = VigemGamepads::new();
        assert!(g.update(&GamepadState { index: 7, ..Default::default() }).is_err());
        assert_eq!(g.plugged_count(), 0);
        g.disconnect(200); // ne panique pas
        g.set_virtual(9, VirtualPad::DualShock4); // ignore
        g.set_virtual(1, VirtualPad::DualShock4);
        assert_eq!(g.want[1], VirtualPad::DualShock4);
        assert_eq!(g.plugged_kind(1), None);
    }

    #[test]
    fn null_sink_accepts_everything() {
        let mut n = NullGamepads;
        n.update(&GamepadState::default()).unwrap();
        n.disconnect(0);
        n.set_virtual(0, VirtualPad::DualShock4);
    }

    /// Verifie que ViGEmBus est installe et joignable (connexion seule, aucun controleur cree).
    #[test]
    fn vigem_bus_is_reachable_on_this_machine() {
        match VigemGamepads::probe() {
            Ok(()) => {}
            Err(e) => eprintln!("ViGEmBus injoignable sur cette machine (test non bloquant) : {e}"),
        }
    }

    /// Branche reellement UNE manette virtuelle Xbox 360, envoie un etat neutre puis la debranche.
    /// Joue le son Windows de connexion/deconnexion : lance a la demande uniquement.
    #[test]
    #[ignore = "branche reellement une manette virtuelle (son Windows) ; ViGEmBus requis"]
    fn real_vigem_plug_update_unplug() {
        let mut g = VigemGamepads::new();
        g.update(&GamepadState { index: 0, ..Default::default() }).expect("plug + update neutre");
        assert_eq!(g.plugged_count(), 1);
        g.update(&GamepadState { index: 0, buttons: 0, left_trigger: 0, ..Default::default() }).unwrap();
        g.disconnect(0);
        assert_eq!(g.plugged_count(), 0);
    }

    /// Branche une DualShock 4 virtuelle, puis bascule l'index en Xbox 360. Lance a la demande.
    #[test]
    #[ignore = "branche reellement des manettes virtuelles (son Windows) ; ViGEmBus requis"]
    fn real_vigem_ds4_then_switch_to_xbox() {
        let mut g = VigemGamepads::new();
        g.set_virtual(0, VirtualPad::DualShock4);
        g.update(&GamepadState { index: 0, buttons: xbutton::A, ..Default::default() }).expect("DS4");
        assert_eq!(g.plugged_kind(0), Some(VirtualPad::DualShock4));
        g.set_virtual(0, VirtualPad::Xbox360);
        assert_eq!(g.plugged_count(), 0);
        g.update(&GamepadState { index: 0, ..Default::default() }).expect("X360");
        assert_eq!(g.plugged_kind(0), Some(VirtualPad::Xbox360));
        g.disconnect(0);
    }
}
