//! « Verrouiller le clavier » (comme le mode immersif de Parsec) : les combinaisons que Windows
//! intercepte avant toute fenêtre (touches Windows, Alt+Tab, Alt+Échap, Ctrl+Échap, Alt+Espace)
//! sont envoyées au host au lieu d'agir sur le PC client.
//!
//! Mécanisme : crochet clavier bas niveau `WH_KEYBOARD_LL`, installé seulement quand l'option est
//! activée, sur le thread de la fenêtre (qui pompe les messages, condition des crochets LL). Il
//! n'agit que si la fenêtre du client est au premier plan, que le menu est fermé, et ignore les
//! frappes injectées. Une touche prise à l'appui l'est aussi au relâchement (même si le focus a
//! changé entre-temps), pour ne jamais laisser une touche « enfoncée » d'un côté ou de l'autre.
//!
//! Limite : Ctrl+Alt+Suppr (séquence sécurisée) et Win+L restent gérés par Windows.

use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::sync::Mutex;

use grx_input::InputEvent;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_RWIN, VK_SPACE, VK_TAB,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
    LLKHF_ALTDOWN, LLKHF_EXTENDED, LLKHF_INJECTED, WH_KEYBOARD_LL,
};

use crate::input::Msg;

const WM_KEYDOWN: usize = 0x0100;
const WM_SYSKEYDOWN: usize = 0x0104;

/// Faut-il prendre cette frappe pour le host ? `held` : touche déjà prise à l'appui.
pub fn should_intercept(vk: u16, down: bool, alt: bool, ctrl: bool, held: bool) -> bool {
    if held {
        return true;
    }
    if !down {
        return false;
    }
    match vk {
        VK_LWIN | VK_RWIN => true,
        VK_TAB | VK_SPACE => alt,
        VK_ESCAPE => alt || ctrl,
        _ => false,
    }
}

struct State {
    tx: Sender<Msg>,
    hwnd: usize,
    hook: usize,
    /// Menu ouvert : tout passe.
    suspended: bool,
    held: HashSet<(u8, bool)>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn send_key(st: &State, (scancode, extended): (u8, bool), pressed: bool) {
    let _ = st.tx.send(Msg::Event(InputEvent::Key { scancode, extended, pressed }));
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam != 0 {
        // SAFETY : pour WH_KEYBOARD_LL, lparam pointe sur un KBDLLHOOKSTRUCT valide pendant l'appel.
        let k = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if let Ok(mut g) = STATE.try_lock() {
            if let Some(st) = g.as_mut() {
                let down = matches!(wparam, WM_KEYDOWN | WM_SYSKEYDOWN);
                let key = ((k.scanCode & 0xFF) as u8, k.flags & LLKHF_EXTENDED != 0);
                let held = st.held.contains(&key);
                let active = !st.suspended && unsafe { GetForegroundWindow() } as usize == st.hwnd;
                let injected = k.flags & LLKHF_INJECTED != 0;
                if !injected && key.0 != 0 && (held || active) {
                    let alt = k.flags & LLKHF_ALTDOWN != 0;
                    let ctrl = unsafe { GetAsyncKeyState(VK_CONTROL as i32) } < 0;
                    if should_intercept(k.vkCode as u16, down, alt, ctrl, held) {
                        if down {
                            if st.held.insert(key) {
                                send_key(st, key, true);
                            }
                        } else {
                            st.held.remove(&key);
                            send_key(st, key, false);
                        }
                        return 1; // avalée : Windows ne la voit pas
                    }
                }
            }
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// Verrou clavier d'une fenêtre. À utiliser sur le thread de la fenêtre uniquement.
pub struct KeyboardLock {
    tx: Option<Sender<Msg>>,
    hwnd: usize,
}

impl KeyboardLock {
    /// `tx = None` (`--no-input`) : le verrou ne s'active jamais.
    pub fn new(tx: Option<Sender<Msg>>, hwnd: usize) -> Self {
        Self { tx, hwnd }
    }

    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        STATE.lock().unwrap().is_some()
    }

    /// Installe ou retire le crochet. Renvoie l'état effectif.
    pub fn set_enabled(&mut self, on: bool) -> bool {
        let mut g = STATE.lock().unwrap();
        if on == g.is_some() {
            return on;
        }
        if on {
            let Some(tx) = self.tx.clone() else { return false };
            if self.hwnd == 0 {
                return false;
            }
            // SAFETY : appel Win32 simple ; la procédure vit aussi longtemps que le programme.
            let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), GetModuleHandleW(std::ptr::null()), 0) };
            if hook.is_null() {
                eprintln!("verrouillage du clavier impossible : {}", std::io::Error::last_os_error());
                return false;
            }
            *g = Some(State { tx, hwnd: self.hwnd, hook: hook as usize, suspended: false, held: HashSet::new() });
            eprintln!("clavier verrouillé : Win, Alt+Tab, Alt/Ctrl+Échap, Alt+Espace vont au host");
            true
        } else {
            if let Some(st) = g.take() {
                for k in &st.held {
                    send_key(&st, *k, false);
                }
                unsafe { UnhookWindowsHookEx(st.hook as HHOOK) };
                eprintln!("clavier déverrouillé");
            }
            false
        }
    }

    /// Menu ouvert : on laisse tout passer (les touches tenues sont relâchées côté host).
    pub fn set_suspended(&mut self, on: bool) {
        if let Some(st) = STATE.lock().unwrap().as_mut() {
            st.suspended = on;
            if on {
                Self::release(st);
            }
        }
    }

    /// Perte de focus : relâche côté host ce que le crochet a pris.
    pub fn release_all(&mut self) {
        if let Some(st) = STATE.lock().unwrap().as_mut() {
            Self::release(st);
        }
    }

    fn release(st: &mut State) {
        for k in std::mem::take(&mut st.held) {
            send_key(st, k, false);
        }
    }
}

impl Drop for KeyboardLock {
    fn drop(&mut self) {
        self.set_enabled(false);
    }
}

/// HWND d'une fenêtre winit (0 si indisponible).
pub fn hwnd_of(win: &grx_render::winit::window::Window) -> usize {
    use grx_render::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match win.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get() as usize,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intercept_rules() {
        assert!(should_intercept(VK_LWIN, true, false, false, false));
        assert!(should_intercept(VK_RWIN, true, false, false, false));
        assert!(should_intercept(VK_TAB, true, true, false, false), "Alt+Tab");
        assert!(!should_intercept(VK_TAB, true, false, false, false), "Tab seul : chemin normal");
        assert!(should_intercept(VK_ESCAPE, true, false, true, false), "Ctrl+Échap");
        assert!(should_intercept(VK_ESCAPE, true, true, false, false), "Alt+Échap");
        assert!(!should_intercept(VK_ESCAPE, true, false, false, false));
        assert!(should_intercept(VK_SPACE, true, true, false, false), "Alt+Espace");
        assert!(!should_intercept(0x41, true, true, true, false), "lettres : chemin normal");
        // Relâchement : seulement si la touche a été prise à l'appui (Alt déjà relâché ou pas).
        assert!(should_intercept(VK_TAB, false, false, false, true));
        assert!(!should_intercept(VK_TAB, false, true, false, false));
        assert!(!should_intercept(VK_LWIN, false, false, false, false));
    }

    #[test]
    fn hook_installs_and_uninstalls_without_input() {
        // Sans fenêtre au premier plan correspondante, le crochet laisse tout passer ; on vérifie
        // seulement l'installation/désinstallation (aucune frappe simulée).
        let (tx, rx) = std::sync::mpsc::channel();
        let mut l = KeyboardLock::new(Some(tx), 1);
        assert!(l.set_enabled(true));
        assert!(l.is_active());
        l.set_suspended(true);
        l.release_all();
        assert!(!l.set_enabled(false));
        assert!(!l.is_active());
        assert!(rx.try_recv().is_err(), "rien n'a été envoyé");
        let mut none = KeyboardLock::new(None, 1);
        assert!(!none.set_enabled(true), "--no-input : jamais actif");
    }
}
