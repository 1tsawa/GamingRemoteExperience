//! Écrans du host : énumération, annonce au client (`GRX-DSP`), choix demandé par le client
//! (`GRX-MON`) et cible de la souris absolue. Format des messages : [`grx_core::displays`].
//!
//! * Le thread `grx-displays` relit la liste toutes les [`WATCH_PERIOD`] (DXGI + DisplayConfig,
//!   quelques ms, hors des boucles critiques) ; un changement incrémente [`Displays::generation`]
//!   et la boucle réseau réannonce la liste.
//! * Le thread vidéo possède le capturer : il lit la demande ([`Displays::take_request`]), bascule,
//!   puis publie l'écran actif ([`Displays::set_active`]) et le rectangle visé par la souris absolue.
//! * La boucle réseau ne fait que lire des atomiques / copier une petite liste.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use grx_core::displays::{DisplayInfo, DisplayList, MAX_DISPLAYS};
use grx_input::absmap::{bounding, DeskRect};
use grx_video::OutputInfo;

/// Période de relecture de la configuration d'affichage.
pub const WATCH_PERIOD: Duration = Duration::from_secs(2);
const NO_REQUEST: u32 = u32::MAX;

/// Rectangle d'une sortie dans le bureau virtuel.
pub fn rect_of(o: &OutputInfo) -> DeskRect {
    DeskRect::new(o.left, o.top, o.width, o.height)
}

/// Bureau virtuel = union des écrans actifs (pixels physiques, cohérent avec DXGI).
pub fn virtual_desk(list: &[OutputInfo]) -> Option<DeskRect> {
    bounding(&list.iter().map(rect_of).collect::<Vec<_>>())
}

/// Deux listes décrivent-elles la même configuration ?
pub fn same_layout(a: &[OutputInfo], b: &[OutputInfo]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            (x.adapter_index, x.output_index, x.left, x.top, x.width, x.height, x.primary, x.is_virtual)
                == (y.adapter_index, y.output_index, y.left, y.top, y.width, y.height, y.primary, y.is_virtual)
                && x.device_name == y.device_name
                && x.friendly_name == y.friendly_name
        })
}

/// Message `GRX-DSP` pour une liste et l'écran actif (nom GDI).
pub fn to_message(list: &[OutputInfo], active: Option<&str>) -> DisplayList {
    let displays: Vec<DisplayInfo> = list
        .iter()
        .take(MAX_DISPLAYS)
        .map(|o| DisplayInfo {
            name: o.display_name(),
            left: o.left,
            top: o.top,
            width: o.width,
            height: o.height,
            primary: o.primary,
            is_virtual: o.is_virtual,
        })
        .collect();
    let active = active.and_then(|n| list.iter().take(MAX_DISPLAYS).position(|o| o.device_name == n)).map(|i| i as u8);
    DisplayList { active, displays }
}

/// Écran de départ : `--display N` (1-based, nom GDI résolu au lancement), sinon le principal,
/// sinon le premier.
pub fn pick_initial<'a>(list: &'a [OutputInfo], wanted: Option<&str>) -> Option<&'a OutputInfo> {
    wanted
        .and_then(|n| list.iter().find(|o| o.device_name == n))
        .or_else(|| list.iter().find(|o| o.primary))
        .or_else(|| list.first())
}

/// Résumé d'une ligne pour le journal : « 1: DELL U2719D 2560x1440 @(0,0) [principal] ».
pub fn describe(i: usize, o: &OutputInfo) -> String {
    format!(
        "{}: {} {}x{} @({},{}){}{} [{}]",
        i + 1,
        o.display_name(),
        o.width,
        o.height,
        o.left,
        o.top,
        if o.primary { " [principal]" } else { "" },
        if o.is_virtual { " [virtuel]" } else { "" },
        o.adapter_name
    )
}

#[derive(Default)]
struct State {
    list: Vec<OutputInfo>,
    active: Option<String>,
}

/// État partagé des écrans d'une session.
pub struct Displays {
    state: Mutex<State>,
    generation: AtomicU64,
    request: AtomicU32,
    /// Sortie visée par la souris absolue (+ bureau virtuel), lue par le thread d'entrée.
    pub abs_target: Arc<grx_input::absmap::AbsTarget>,
}

impl Default for Displays {
    fn default() -> Self {
        Self::new()
    }
}

impl Displays {
    pub fn new() -> Self {
        Displays {
            state: Mutex::new(State::default()),
            generation: AtomicU64::new(0),
            request: AtomicU32::new(NO_REQUEST),
            abs_target: Arc::new(grx_input::absmap::AbsTarget::default()),
        }
    }

    /// Change à chaque modification de la liste ou de l'écran actif (0 = rien de connu).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn list(&self) -> Vec<OutputInfo> {
        self.state.lock().unwrap().list.clone()
    }

    /// Message `GRX-DSP` courant.
    pub fn message(&self) -> DisplayList {
        let s = self.state.lock().unwrap();
        to_message(&s.list, s.active.as_deref())
    }

    /// Nouvelle liste (renvoie vrai si elle a changé). Met aussi à jour la cible souris.
    pub fn set_list(&self, list: Vec<OutputInfo>) -> bool {
        let mut s = self.state.lock().unwrap();
        if same_layout(&s.list, &list) {
            return false;
        }
        s.list = list;
        self.update_abs(&s);
        self.generation.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// Écran réellement capturé (nom GDI).
    pub fn set_active(&self, name: &str) {
        let mut s = self.state.lock().unwrap();
        if s.active.as_deref() == Some(name) {
            return;
        }
        s.active = Some(name.to_string());
        self.update_abs(&s);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn update_abs(&self, s: &State) {
        let out = s.active.as_deref().and_then(|n| s.list.iter().find(|o| o.device_name == n));
        match (out, virtual_desk(&s.list)) {
            (Some(o), Some(v)) => self.abs_target.set(Some((rect_of(o), v))),
            _ => self.abs_target.set(None),
        }
    }

    /// Demande du client (index dans la dernière liste annoncée).
    pub fn request(&self, index: u8) {
        self.request.store(index as u32, Ordering::Release);
    }

    /// Demande en attente, résolue en sortie (ignorée si l'index n'existe pas/plus).
    pub fn take_request(&self) -> Option<OutputInfo> {
        let i = self.request.swap(NO_REQUEST, Ordering::AcqRel);
        if i == NO_REQUEST {
            return None;
        }
        self.state.lock().unwrap().list.get(i as usize).cloned()
    }
}

/// Thread de surveillance de la configuration d'affichage (jusqu'à `stop`).
pub fn watch(d: Arc<Displays>, stop: &std::sync::atomic::AtomicBool) {
    let mut waited = Duration::ZERO;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
        waited += Duration::from_millis(100);
        if waited < WATCH_PERIOD {
            continue;
        }
        waited = Duration::ZERO;
        if let Ok(list) = grx_video::list_displays() {
            // Liste encore vide : le thread vidéo ne l'a pas publiée, rien n'a « changé ».
            let known = !d.list().is_empty();
            if !list.is_empty() && d.set_list(list.clone()) && known {
                println!("  écrans : configuration changée");
                for (i, o) in list.iter().enumerate() {
                    println!("    {}", describe(i, o));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(name: &str, ai: u32, oi: u32, left: i32, top: i32, w: u32, h: u32) -> OutputInfo {
        OutputInfo {
            adapter_index: ai,
            output_index: oi,
            adapter_name: "GPU".into(),
            vendor_id: 0x10DE,
            width: w,
            height: h,
            left,
            top,
            attached_to_desktop: true,
            primary: left == 0 && top == 0,
            device_name: format!(r"\\.\{name}"),
            friendly_name: String::new(),
            is_virtual: false,
        }
    }

    #[test]
    fn capturable_filters_and_sorts_left_to_right() {
        let mut inactive = out("DISPLAY3", 0, 2, 0, 0, 0, 0);
        inactive.attached_to_desktop = false;
        let mut detached = out("DISPLAY4", 0, 3, 9000, 0, 1920, 1080);
        detached.attached_to_desktop = false;
        let raw = vec![
            out("DISPLAY1", 0, 0, 0, 0, 2560, 1440),
            inactive,
            out("DISPLAY2", 0, 1, -2560, 0, 2560, 1440),
            detached,
            out("DISPLAY2", 1, 0, -2560, 0, 2560, 1440), // même sortie vue par un 2e adaptateur
            out("DISPLAY5", 0, 4, 0, -1440, 2560, 1440),
        ];
        let v = grx_video::capturable(&raw);
        let names: Vec<_> = v.iter().map(|o| o.display_name()).collect();
        assert_eq!(names, ["DISPLAY2", "DISPLAY5", "DISPLAY1"]);
        assert_eq!(v[0].adapter_index, 0, "première occurrence gardée");
    }

    #[test]
    fn message_initial_choice_and_request() {
        let list = vec![out("DISPLAY2", 0, 1, -2560, 0, 2560, 1440), out("DISPLAY1", 0, 0, 0, 0, 2560, 1440)];
        assert_eq!(pick_initial(&list, None).unwrap().device_name, r"\\.\DISPLAY1");
        assert_eq!(pick_initial(&list, Some(r"\\.\DISPLAY2")).unwrap().device_name, r"\\.\DISPLAY2");
        assert_eq!(pick_initial(&list, Some(r"\\.\DISPLAY9")).unwrap().device_name, r"\\.\DISPLAY1");
        assert!(pick_initial(&[], None).is_none());

        let d = Displays::new();
        assert_eq!(d.generation(), 0);
        assert!(d.set_list(list.clone()));
        assert!(!d.set_list(list.clone()), "liste identique : pas de réannonce");
        let g = d.generation();
        d.set_active(r"\\.\DISPLAY1");
        assert!(d.generation() > g);
        let m = d.message();
        assert_eq!(m.active, Some(1));
        assert_eq!(m.displays.len(), 2);
        assert_eq!((m.displays[0].left, m.displays[0].name.as_str()), (-2560, "DISPLAY2"));
        assert!(m.displays[1].primary);
        // Cible souris : écran principal dans un bureau virtuel de 5120 px de large.
        assert_eq!(d.abs_target.get(), Some((DeskRect::new(0, 0, 2560, 1440), DeskRect::new(-2560, 0, 5120, 1440))));

        assert!(d.take_request().is_none());
        d.request(0);
        assert_eq!(d.take_request().unwrap().device_name, r"\\.\DISPLAY2");
        assert!(d.take_request().is_none(), "consommée");
        d.request(7);
        assert!(d.take_request().is_none(), "index inexistant ignoré");

        // L'écran actif disparaît de la liste : plus de cible souris (pas de mauvais écran).
        assert!(d.set_list(vec![out("DISPLAY2", 0, 1, -2560, 0, 2560, 1440)]));
        assert_eq!(d.message().active, None);
        assert_eq!(d.abs_target.get(), None);
    }
}
