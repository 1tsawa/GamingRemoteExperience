//! Curseur distant côté client : réception (forme fiable + position non fiable, voir
//! `grx_transport::cursor`), cache des formes, puis transmission au présentateur.
//!
//! * Le thread réseau appelle [`CursorInbox::on_control`] / [`CursorInbox::on_datagram`] (rapide,
//!   sous un verrou court) ;
//! * un thread `grx-client-cursor` attend les changements ([`CursorInbox::wait`]) et appelle
//!   `PresenterHandle::set_remote_cursor`, qui re-présente aussitôt la dernière image : le curseur
//!   bouge dès réception, sans attendre la prochaine image vidéo.
//! * Position : la plus récente gagne (`seq`) ; un datagramme perdu est réparé par le suivant ou
//!   par le rafraîchissement périodique du host (100 ms).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use grx_render::winit::event::WindowEvent;
use grx_render::winit::window::Window;
use grx_render::{CursorShape, PresenterEvent, PresenterHandle, RemoteCursor};
use crate::overlay_cursor::OverlayHover;
use grx_transport::cursor::{is_shape_chunk, seq_newer, CursorPos, FifoCache, Shape, ShapeAssembler, SHAPE_CACHE};

/// Le client garde un peu plus de formes que le host n'en suppose (marge, même ordre FIFO).
const CLIENT_CACHE: usize = SHAPE_CACHE * 2;

struct Inner {
    asm: ShapeAssembler,
    cache: FifoCache<Arc<CursorShape>>,
    pos: Option<CursorPos>,
    /// Dernière forme utilisée (gardée si la position annonce une forme pas encore reçue).
    shape: Option<Arc<CursorShape>>,
    dirty: bool,
    shapes_rx: u64,
    positions_rx: u64,
    stale_rx: u64,
}

/// Boîte aux lettres « dernier état gagnant » entre le thread réseau et le thread du curseur.
pub struct CursorInbox {
    inner: Mutex<Inner>,
    cv: Condvar,
}

impl Default for CursorInbox {
    fn default() -> Self {
        CursorInbox {
            inner: Mutex::new(Inner {
                asm: ShapeAssembler::default(),
                cache: FifoCache::new(CLIENT_CACHE),
                pos: None,
                shape: None,
                dirty: false,
                shapes_rx: 0,
                positions_rx: 0,
                stale_rx: 0,
            }),
            cv: Condvar::new(),
        }
    }
}

/// Compteurs de réception du curseur.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorRxStats {
    pub shapes: u64,
    pub positions: u64,
    /// Positions plus anciennes que la dernière reçue (réordonnancement), ignorées.
    pub stale: u64,
}

pub fn to_render_shape(id: u32, s: Shape) -> CursorShape {
    CursorShape {
        id,
        width: s.width as u32,
        height: s.height as u32,
        hot_x: s.hot_x as u32,
        hot_y: s.hot_y as u32,
        rgba: s.rgba,
        xor: s.xor,
    }
}

impl CursorInbox {
    /// Message Control : `true` s'il s'agissait d'un morceau de forme (consommé).
    pub fn on_control(&self, m: &[u8]) -> bool {
        if !is_shape_chunk(m) {
            return false;
        }
        let mut g = self.inner.lock().unwrap();
        if let Some((id, s)) = g.asm.push(m) {
            g.cache.insert(id, Arc::new(to_render_shape(id, s)));
            g.shapes_rx += 1;
            // Une position reçue avant sa forme peut maintenant l'utiliser.
            if g.pos.is_some_and(|p| p.shape_id == id) {
                g.dirty = true;
                self.cv.notify_all();
            }
        }
        true
    }

    /// Datagramme non fiable : `true` s'il s'agissait d'une position de curseur valide.
    pub fn on_datagram(&self, d: &[u8]) -> bool {
        let Some(p) = CursorPos::decode(d) else { return false };
        let mut g = self.inner.lock().unwrap();
        if g.pos.is_some_and(|old| !seq_newer(p.seq, old.seq)) {
            g.stale_rx += 1;
            return true;
        }
        g.positions_rx += 1;
        let key = |c: &CursorPos| (c.visible, c.shape_id, c.x, c.y, c.src_w, c.src_h);
        if g.pos.as_ref().map(key) != Some(key(&p)) {
            g.dirty = true;
            self.cv.notify_all();
        }
        g.pos = Some(p);
        true
    }

    /// État à dessiner s'il a changé depuis le dernier appel (attend au plus `timeout`).
    pub fn wait(&self, timeout: Duration) -> Option<RemoteCursor> {
        let g = self.inner.lock().unwrap();
        let (mut g, _) = self.cv.wait_timeout_while(g, timeout, |g| !g.dirty).unwrap();
        if !g.dirty {
            return None;
        }
        g.dirty = false;
        let p = g.pos?;
        if let Some(s) = g.cache.get(p.shape_id).cloned() {
            g.shape = Some(s);
        }
        Some(RemoteCursor {
            visible: p.visible,
            shape: g.shape.clone(),
            x: p.x_norm(),
            y: p.y_norm(),
            src_w: p.src_w as u32,
            src_h: p.src_h as u32,
            ts_us: p.ts_us,
        })
    }

    pub fn stats(&self) -> CursorRxStats {
        let g = self.inner.lock().unwrap();
        CursorRxStats { shapes: g.shapes_rx, positions: g.positions_rx, stale: g.stale_rx }
    }

    /// Un curseur a-t-il déjà été reçu (le curseur local peut alors être masqué) ?
    pub fn active(&self) -> bool {
        self.inner.lock().unwrap().pos.is_some()
    }
}

/// Thread qui pousse chaque nouvel état du curseur au présentateur. Avec `print_stats`, une
/// ligne par seconde (positions reçues, latence « mise à jour sur le host -> dessin »), et un
/// résumé à la fin dans tous les cas.
pub fn spawn_forwarder(
    inbox: Arc<CursorInbox>,
    handle: PresenterHandle,
    stop: Arc<AtomicBool>,
    print_stats: bool,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("grx-client-cursor".into())
        .spawn(move || {
            let mut next = Instant::now() + Duration::from_secs(1);
            let mut last = inbox.stats();
            let mut all: Vec<f64> = Vec::new();
            let mut p99s: Vec<f64> = Vec::new();
            let mut seconds = 0u32;
            while !stop.load(Ordering::Relaxed) && !handle.is_closed() {
                if let Some(c) = inbox.wait(Duration::from_millis(50)) {
                    handle.set_remote_cursor(Some(c));
                }
                if Instant::now() >= next {
                    next += Duration::from_secs(1);
                    seconds += 1;
                    let l = handle.take_cursor_latency_stats();
                    let s = inbox.stats();
                    // 1re seconde exclue du résumé (état initial daté du début de session).
                    if l.count > 0 && seconds > 1 {
                        all.push(l.avg_us as f64 / 1000.0);
                        p99s.push(l.p99_us as f64 / 1000.0);
                    }
                    if print_stats && s.positions != last.positions {
                        println!(
                            "  curseur : {} positions/s, {} dessinées, host->dessin moy {:.1} ms (p99 {:.1}, max {:.1}), formes reçues {}",
                            s.positions - last.positions,
                            l.count,
                            l.avg_us as f64 / 1000.0,
                            l.p99_us as f64 / 1000.0,
                            l.max_us as f64 / 1000.0,
                            s.shapes
                        );
                    }
                    last = s;
                }
            }
            let s = inbox.stats();
            if s.positions > 0 {
                let avg = all.iter().sum::<f64>() / all.len().max(1) as f64;
                let p99 = p99s.iter().cloned().fold(0.0, f64::max);
                println!(
                    "curseur : {} positions reçues ({} périmées), {} formes ; mise à jour host -> dessin : moy {avg:.1} ms, pire p99/s {p99:.1} ms (horloges supposées synchrones)",
                    s.positions, s.stale, s.shapes
                );
            }
        })
        .expect("spawn cursor")
}

/// Masque le curseur Windows local au-dessus de l'image vidéo dès qu'un curseur distant est
/// reçu (il est dessiné à sa place) ; le laisse visible sur les bandes noires. En mode capture,
/// la visibilité reste gérée par `input.rs` (curseur local masqué, curseur distant affiché).
/// Sur l'overlay (survol d'un élément interactif, menu ouvert), c'est l'inverse : curseur local
/// visible et curseur distant non dessiné, voir [`crate::overlay_cursor::decide`].
pub struct LocalCursorHider {
    inbox: Arc<CursorInbox>,
    handle: PresenterHandle,
    over_video: bool,
    captured: bool,
    applied: Option<bool>,
    remote_suppressed: bool,
}

impl LocalCursorHider {
    pub fn new(inbox: Arc<CursorInbox>, handle: PresenterHandle) -> Self {
        LocalCursorHider { inbox, handle, over_video: false, captured: false, applied: None, remote_suppressed: false }
    }

    /// Visibilité voulue pour le curseur local hors overlay (`None` : ne pas y toucher, mode capture).
    #[cfg(test)]
    pub fn desired(remote_active: bool, over_video: bool, captured: bool) -> Option<bool> {
        crate::overlay_cursor::decide(crate::overlay_cursor::CursorInputs {
            remote_active,
            over_video,
            captured,
            ..Default::default()
        })
        .local_visible
    }

    /// À appeler pour chaque événement, une fois l'overlay et `InputHandler` passés
    /// (`captured` = état effectif de la capture souris, `menu_open`/`hover` = état de l'overlay).
    pub fn update(&mut self, ev: &PresenterEvent, win: &Window, captured: bool, menu_open: bool, hover: OverlayHover) {
        if captured != self.captured {
            self.captured = captured;
            self.applied = None; // input.rs vient de changer la visibilité
        }
        match ev {
            PresenterEvent::Window(WindowEvent::CursorMoved { position, .. }) => {
                self.over_video = self.handle.window_to_video(position.x, position.y).is_some();
            }
            PresenterEvent::Window(WindowEvent::CursorLeft { .. }) => self.over_video = false,
            _ => {}
        }
        let d = crate::overlay_cursor::decide(crate::overlay_cursor::CursorInputs {
            menu_open,
            hover,
            captured: self.captured,
            remote_active: self.inbox.active(),
            over_video: self.over_video,
        });
        if let Some(v) = d.local_visible {
            if self.applied != Some(v) {
                win.set_cursor_visible(v);
                self.applied = Some(v);
            }
        }
        if self.remote_suppressed == d.draw_remote {
            self.remote_suppressed = !d.draw_remote;
            self.handle.set_remote_cursor_suppressed(self.remote_suppressed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grx_transport::cursor::{shape_chunks, shape_id};

    fn shape(c: u8) -> (u32, Vec<Vec<u8>>) {
        let s = Shape { width: 4, height: 4, hot_x: 1, hot_y: 1, rgba: vec![c; 64], xor: None };
        let body = s.encode_body().unwrap();
        let id = shape_id(&body);
        (id, shape_chunks(id, &body))
    }

    fn pos(seq: u32, id: u32, x: i32) -> Vec<u8> {
        CursorPos { seq, visible: true, shape_id: id, x, y: 0, src_w: 1920, src_h: 1080, ts_us: 1 }.encode()
    }

    #[test]
    fn local_cursor_hidden_only_over_video_when_remote_active() {
        assert_eq!(LocalCursorHider::desired(false, true, false), Some(true));
        assert_eq!(LocalCursorHider::desired(true, true, false), Some(false));
        assert_eq!(LocalCursorHider::desired(true, false, false), Some(true), "bandes noires / hors fenêtre");
        assert_eq!(LocalCursorHider::desired(true, true, true), None, "mode capture : géré par input.rs");
    }

    #[test]
    fn latest_position_wins_and_shape_is_cached() {
        let ib = CursorInbox::default();
        let (id, chunks) = shape(9);
        assert!(!ib.on_control(b"GRX-BYE"), "autre message Control : pas pour nous");
        assert!(!ib.on_datagram(b"junk"));
        assert!(ib.wait(Duration::ZERO).is_none());

        // Position avant la forme : état publié sans forme, puis complété à l'arrivée de la forme.
        assert!(ib.on_datagram(&pos(5, id, 32768)));
        let c = ib.wait(Duration::ZERO).unwrap();
        assert!(c.shape.is_none() && (c.x - 0.5).abs() < 1e-6);
        for ch in &chunks {
            assert!(ib.on_control(ch));
        }
        let c = ib.wait(Duration::ZERO).unwrap();
        assert_eq!(c.shape.as_ref().unwrap().id, id);

        // Datagramme plus ancien : ignoré ; identique : pas de réveil.
        ib.on_datagram(&pos(4, id, 0));
        assert!(ib.wait(Duration::ZERO).is_none());
        ib.on_datagram(&pos(6, id, 32768));
        assert!(ib.wait(Duration::ZERO).is_none(), "même état (seul seq change)");
        ib.on_datagram(&pos(7, id, 0));
        assert!(ib.wait(Duration::ZERO).unwrap().x.abs() < 1e-6);

        // Position annonçant une forme inconnue : on garde la précédente.
        ib.on_datagram(&pos(8, 12345, 100));
        assert_eq!(ib.wait(Duration::ZERO).unwrap().shape.unwrap().id, id);
        assert_eq!(ib.stats(), CursorRxStats { shapes: 1, positions: 4, stale: 1 });
        assert!(ib.active());
    }
}
