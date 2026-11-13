//! Logique pure des manettes cote CLIENT (sans E/S, testable) :
//! * zones mortes : radiale avec remise a l'echelle pour les sticks ([`STICK_DEADZONE`]),
//!   seuil pour les gachettes ([`TRIGGER_DEADZONE`]) ;
//! * [`SlotAllocator`] : index 0..=3 stables a la connexion, liberes a la deconnexion ;
//! * [`SendScheduler`] : envoi sur changement (250 Hz max) + rafraichissement toutes les 100 ms.

use crate::event::{GamepadState, MAX_GAMEPADS};
use std::time::{Duration, Instant};

/// Zone morte radiale par defaut des sticks (~7.6 %).
pub const STICK_DEADZONE: i32 = 2500;
/// Gachettes : valeurs <= ce seuil ramenees a 0.
pub const TRIGGER_DEADZONE: u8 = 8;

/// Zone morte radiale avec remise a l'echelle : si la norme (calculee en f64 sur x, y) < dz -> (0, 0) ;
/// sinon la norme est remappee lineairement de [dz, 32767] vers [0, 32767] en gardant la direction.
/// Au-dela de 32767 (coins d'une manette a course carree), les valeurs sont gardees telles quelles
/// (continuite assuree a 32767). dz <= 0 : renvoie (x, y) inchanges. dz >= 32767 : renvoie (0, 0).
pub fn radial_deadzone(x: i16, y: i16, dz: i32) -> (i16, i16) {
    if dz <= 0 {
        return (x, y);
    }
    if dz >= 32767 {
        return (0, 0);
    }
    let norm = (x as f64).hypot(y as f64);
    if norm < dz as f64 {
        return (0, 0);
    }
    if norm >= 32767.0 {
        return (x, y);
    }
    let dz = dz as f64;
    let factor = (norm - dz) / (32767.0 - dz) * 32767.0 / norm;
    let scale = |v: i16| (v as f64 * factor).round().clamp(-32768.0, 32767.0) as i16;
    (scale(x), scale(y))
}

/// v <= dz -> 0 ; sinon remappage lineaire de [dz, 255] vers [0, 255] (arrondi). dz == 0 -> v.
pub fn trigger_deadzone(v: u8, dz: u8) -> u8 {
    if v <= dz {
        return 0;
    }
    if dz == 0 {
        return v;
    }
    let scale = (v as f64 - dz as f64) / (255.0 - dz as f64);
    (scale * 255.0).round() as u8
}

/// Applique les deux zones mortes par defaut aux sticks gauche/droit et aux gachettes ; boutons et index inchanges.
pub fn normalize(s: GamepadState) -> GamepadState {
    let (lx, ly) = radial_deadzone(s.thumb_lx, s.thumb_ly, STICK_DEADZONE);
    let (rx, ry) = radial_deadzone(s.thumb_rx, s.thumb_ry, STICK_DEADZONE);
    let lt = trigger_deadzone(s.left_trigger, TRIGGER_DEADZONE);
    let rt = trigger_deadzone(s.right_trigger, TRIGGER_DEADZONE);
    GamepadState {
        index: s.index,
        buttons: s.buttons,
        left_trigger: lt,
        right_trigger: rt,
        thumb_lx: lx,
        thumb_ly: ly,
        thumb_rx: rx,
        thumb_ry: ry,
    }
}

/// Allocation d'index stables 0..MAX_GAMEPADS aux peripheriques (cle K) : index libre le plus bas a la
/// connexion, conserve tant que le peripherique est la, libere a la deconnexion.
pub struct SlotAllocator<K: PartialEq + Clone> {
    slots: [Option<K>; MAX_GAMEPADS],
}

impl<K: PartialEq + Clone> SlotAllocator<K> {
    pub fn new() -> Self {
        Self { slots: std::array::from_fn(|_| None) }
    }

    /// Deja present -> Some(son index) ; sinon plus petit index libre ; plein -> None.
    pub fn attach(&mut self, key: &K) -> Option<u8> {
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some(k) = slot {
                if k == key {
                    return Some(i as u8);
                }
            }
        }
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(key.clone());
                return Some(i as u8);
            }
        }
        None
    }

    /// Libere et renvoie l'index, None si inconnu.
    pub fn detach(&mut self, key: &K) -> Option<u8> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if let Some(k) = slot {
                if k == key {
                    *slot = None;
                    return Some(i as u8);
                }
            }
        }
        None
    }

    pub fn index_of(&self, key: &K) -> Option<u8> {
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some(k) = slot {
                if k == key {
                    return Some(i as u8);
                }
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// (index, cle) des emplacements occupes, par index croissant.
    pub fn iter(&self) -> impl Iterator<Item = (u8, &K)> {
        self.slots.iter().enumerate().filter_map(|(i, slot)| slot.as_ref().map(|k| (i as u8, k)))
    }
}

impl<K: PartialEq + Clone> Default for SlotAllocator<K> {
    fn default() -> Self {
        Self::new()
    }
}

/// Cadence d'envoi d'UN index de manette : envoi sur changement, au plus toutes les `min_interval`
/// (250 Hz -> 4 ms), plus un rafraichissement periodique toutes les `refresh` meme sans changement.
pub struct SendScheduler {
    last_sent: Option<GamepadState>,
    last_time: Option<Instant>,
    pending: Option<GamepadState>,
    min_interval: Duration,
    refresh: Duration,
}

impl SendScheduler {
    pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(4);
    pub const DEFAULT_REFRESH: Duration = Duration::from_millis(100);

    pub fn new(min_interval: Duration, refresh: Duration) -> Self {
        Self { last_sent: None, last_time: None, pending: None, min_interval, refresh }
    }

    /// Nouvel etat lu au temps `now`. Renvoie Some(etat a envoyer maintenant) si :
    ///  - rien n'a encore ete envoye ; ou
    ///  - l'etat differe du dernier envoye et now - dernier_envoi >= min_interval ; ou
    ///  - now - dernier_envoi >= refresh (meme etat).
    ///
    /// Si l'etat differe mais que min_interval n'est pas ecoule, il est memorise comme `pending` et None est renvoye.
    /// Un etat egal au dernier envoye efface `pending`.
    pub fn offer(&mut self, state: GamepadState, now: Instant) -> Option<GamepadState> {
        let last = self.last_sent;
        let last_time = self.last_time;
        let state_changed = last != Some(state);

        if state_changed {
            if let Some(lt) = last_time {
                if now.saturating_duration_since(lt) < self.min_interval {
                    self.pending = Some(state);
                    return None;
                }
            }
            self.last_sent = Some(state);
            self.last_time = Some(now);
            self.pending = None;
            Some(state)
        } else {
            self.pending = None;
            if let Some(lt) = last_time {
                if now.saturating_duration_since(lt) >= self.refresh {
                    self.last_time = Some(now);
                    Some(state)
                } else {
                    None
                }
            } else {
                None
            }
        }
    }

    /// A appeler regulierement : renvoie l'etat `pending` (ou le dernier envoye si refresh est ecoule)
    /// des que le delai le permet, sinon None.
    pub fn poll(&mut self, now: Instant) -> Option<GamepadState> {
        if let Some(pending) = self.pending {
            if let Some(lt) = self.last_time {
                if now.saturating_duration_since(lt) >= self.min_interval {
                    self.last_sent = Some(pending);
                    self.last_time = Some(now);
                    self.pending = None;
                    return Some(pending);
                }
            }
            None
        } else {
            if let Some(lt) = self.last_time {
                if now.saturating_duration_since(lt) >= self.refresh {
                    self.last_time = Some(now);
                    self.last_sent
                } else {
                    None
                }
            } else {
                None
            }
        }
    }

    /// Prochain instant ou `poll` pourrait renvoyer quelque chose (None si rien n'a jamais ete envoye).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.last_time.map(|lt| if self.pending.is_some() { lt + self.min_interval } else { lt + self.refresh })
    }

    /// Oublie tout (prochain offer envoie immediatement).
    pub fn reset(&mut self) {
        self.last_sent = None;
        self.last_time = None;
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_radial_deadzone() {
        assert_eq!(radial_deadzone(0, 0, STICK_DEADZONE), (0, 0));
        assert_eq!(radial_deadzone(100, 100, STICK_DEADZONE), (0, 0));
        assert_eq!(radial_deadzone(32767, 0, STICK_DEADZONE), (32767, 0));
        let (x, _) = radial_deadzone(-32768, 0, STICK_DEADZONE);
        assert!(x <= -32767);
        let (x, y) = radial_deadzone(30000, 30000, STICK_DEADZONE);
        assert!(x > 0 && y > 0 && x == y);
        let (x, _) = radial_deadzone(STICK_DEADZONE as i16 + 1000, 0, STICK_DEADZONE);
        assert!(x > 0 && x < 1200, "{x}");
        assert_eq!(radial_deadzone(1000, -2000, 0), (1000, -2000));
        let _ = radial_deadzone(32767, 32767, STICK_DEADZONE);
        let _ = radial_deadzone(-32768, -32768, STICK_DEADZONE);
        let _ = radial_deadzone(0, 32767, STICK_DEADZONE);
        let _ = radial_deadzone(-32768, 0, STICK_DEADZONE);
        // remappage lineaire de la norme : mi-course apres la zone morte -> ~16384, direction gardee
        let mid = (STICK_DEADZONE + (32767 - STICK_DEADZONE) / 2) as i16;
        let (x, y) = radial_deadzone(0, -mid, STICK_DEADZONE);
        assert_eq!(x, 0);
        assert!((y + 16383).abs() <= 2, "{y}");
        // monotone sur l'axe
        let mut prev = 0;
        for v in (0..=32767).step_by(97) {
            let (x, _) = radial_deadzone(v as i16, 0, STICK_DEADZONE);
            assert!(x >= prev);
            prev = x;
        }
    }

    #[test]
    fn test_trigger_deadzone() {
        for v in 0..=TRIGGER_DEADZONE {
            assert_eq!(trigger_deadzone(v, TRIGGER_DEADZONE), 0);
        }
        assert_eq!(trigger_deadzone(255, TRIGGER_DEADZONE), 255);
        let mut prev = 0u8;
        for v in 0..=255 {
            let res = trigger_deadzone(v, TRIGGER_DEADZONE);
            assert!(res >= prev);
            prev = res;
        }
    }

    #[test]
    fn test_normalize() {
        let s = GamepadState {
            index: 2,
            buttons: 0x1234,
            left_trigger: 5,
            right_trigger: 10,
            thumb_lx: 100,
            thumb_ly: 100,
            thumb_rx: 0,
            thumb_ry: 0,
        };
        let n = normalize(s);
        assert_eq!(n.index, 2);
        assert_eq!(n.buttons, 0x1234);
        assert_eq!(n.left_trigger, 0);
        assert_eq!(n.thumb_lx, 0);
        assert_eq!(n.thumb_ly, 0);
    }

    #[test]
    fn test_slot_allocator() {
        let mut alloc: SlotAllocator<&str> = SlotAllocator::new();
        assert!(alloc.is_empty());
        assert_eq!(alloc.attach(&"a"), Some(0));
        assert_eq!(alloc.attach(&"b"), Some(1));
        assert_eq!(alloc.attach(&"c"), Some(2));
        assert_eq!(alloc.attach(&"d"), Some(3));
        assert_eq!(alloc.attach(&"e"), None);
        assert_eq!(alloc.detach(&"b"), Some(1));
        assert_eq!(alloc.attach(&"f"), Some(1));
        assert_eq!(alloc.index_of(&"f"), Some(1));
        assert_eq!(alloc.attach(&"a"), Some(0));
        assert_eq!(alloc.detach(&"z"), None);
        let mut indices = alloc.iter().map(|(i, _)| i).collect::<Vec<_>>();
        indices.sort();
        assert_eq!(indices, vec![0, 1, 2, 3]);
    }

    #[test]
    fn test_send_scheduler() {
        let base = Instant::now();
        let mut sched = SendScheduler::new(SendScheduler::DEFAULT_MIN_INTERVAL, SendScheduler::DEFAULT_REFRESH);
        let s1 = GamepadState::default();
        let s2 = GamepadState { thumb_lx: 100, ..s1 };

        assert_eq!(sched.offer(s1, base), Some(s1));
        assert_eq!(sched.next_deadline(), Some(base + SendScheduler::DEFAULT_REFRESH));

        assert_eq!(sched.offer(s2, base + Duration::from_millis(1)), None);
        assert_eq!(sched.next_deadline(), Some(base + Duration::from_millis(4)));

        assert_eq!(sched.poll(base + Duration::from_millis(4)), Some(s2));
        assert_eq!(sched.next_deadline(), Some(base + Duration::from_millis(4) + SendScheduler::DEFAULT_REFRESH));

        assert_eq!(sched.offer(s2, base + Duration::from_millis(50)), None);
        assert_eq!(sched.offer(s2, base + Duration::from_millis(104)), Some(s2));

        sched.reset();
        assert_eq!(sched.next_deadline(), None);
        assert_eq!(sched.offer(s1, base + Duration::from_millis(200)), Some(s1));
    }
}
