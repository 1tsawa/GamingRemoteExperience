//! Envoi du curseur du host au client (voir `grx_transport::cursor` pour le format).
//!
//! Le thread vidéo publie l'état lu dans DXGI dans un [`CursorTracker`] (y compris les mises à
//! jour « curseur seul », sans nouvelle image) ; la boucle réseau appelle [`CursorSender::drive`]
//! à chaque tour (≤ 250 µs) :
//! * nouvelle forme pas encore connue du client -> morceaux sur le canal Control (fiable) ;
//! * position/visibilité changée -> datagramme non fiable immédiat, renvoyé toutes les
//!   [`REFRESH`] même sans changement (répare une perte ; « dernier état gagnant » côté client).

use std::sync::Arc;
use std::time::{Duration, Instant};

use grx_transport::cursor::{shape_chunks, shape_id, to_fixed, CursorPos, FifoCache, Shape, SHAPE_CACHE};
use grx_transport::Endpoint;
use grx_video::{CursorState, CursorTracker};

use crate::pipeline::unix_micros;

/// Renvoi périodique de l'état même s'il n'a pas changé.
pub const REFRESH: Duration = Duration::from_millis(100);
/// Écart minimal entre deux positions (plafond ~1000 Hz ; DXGI suit déjà la fréquence d'écran).
const MIN_GAP: Duration = Duration::from_millis(1);

pub struct CursorSender {
    tracker: Arc<CursorTracker>,
    /// Formes déjà envoyées : même cache FIFO que le client (canal ordonné => mêmes évictions).
    sent: FifoCache<()>,
    last_serial: u64,
    last_shape_serial: u64,
    shape_id: u32,
    seq: u32,
    last_send: Option<Instant>,
    pub positions_sent: u64,
    pub shapes_sent: u64,
    pub shape_bytes: u64,
}

/// Forme DXGI -> forme du protocole (dimensions déjà bornées à 256 par `grx_video`).
pub fn to_wire_shape(img: &grx_video::CursorImage) -> Option<Shape> {
    let d = |v: u32| u16::try_from(v).ok();
    let s = Shape {
        width: d(img.width)?,
        height: d(img.height)?,
        hot_x: d(img.hot_x)?,
        hot_y: d(img.hot_y)?,
        rgba: img.rgba.clone(),
        xor: img.xor.clone(),
    };
    s.is_valid().then_some(s)
}

/// Message de position pour un état donné.
pub fn pos_message(st: &CursorState, seq: u32, shape_id: u32) -> CursorPos {
    let (hx, hy) = st.hotspot_pos();
    let ts_us = unix_micros().saturating_sub(st.updated_at.elapsed().as_micros() as u64);
    CursorPos {
        seq,
        visible: st.visible && st.shape.is_some(),
        shape_id,
        x: to_fixed(hx, st.frame_w),
        y: to_fixed(hy, st.frame_h),
        src_w: st.frame_w.min(u16::MAX as u32) as u16,
        src_h: st.frame_h.min(u16::MAX as u32) as u16,
        ts_us,
    }
}

impl CursorSender {
    pub fn new(tracker: Arc<CursorTracker>) -> Self {
        CursorSender {
            tracker,
            sent: FifoCache::new(SHAPE_CACHE),
            last_serial: 0,
            last_shape_serial: 0,
            shape_id: 0,
            seq: 0,
            last_send: None,
            positions_sent: 0,
            shapes_sent: 0,
            shape_bytes: 0,
        }
    }

    /// Un tour : envoie ce qui a changé (ou le rafraîchissement périodique).
    pub fn drive(&mut self, ep: &mut Endpoint) {
        let serial = self.tracker.serial();
        if serial == 0 {
            return; // rien lu dans DXGI pour l'instant (ou pas de capture vidéo)
        }
        let now = Instant::now();
        let changed = serial != self.last_serial;
        let due = match self.last_send {
            None => true,
            Some(t) if changed => now.duration_since(t) >= MIN_GAP,
            Some(t) => now.duration_since(t) >= REFRESH,
        };
        if !due {
            return;
        }
        let st = self.tracker.snapshot();
        if st.shape_serial != self.last_shape_serial {
            if let Some(shape) = st.shape.as_deref().and_then(to_wire_shape) {
                if let Some(body) = shape.encode_body() {
                    let id = shape_id(&body);
                    if !self.sent.contains(id) {
                        let chunks = shape_chunks(id, &body);
                        // Tout ou rien : si la file fiable est saturée, on réessaiera au tour suivant.
                        if chunks.iter().all(|c| ep.send_control(c).is_ok()) {
                            self.sent.insert(id, ());
                            self.shapes_sent += 1;
                            self.shape_bytes += body.len() as u64;
                        } else {
                            return;
                        }
                    }
                    self.shape_id = id;
                }
            }
            self.last_shape_serial = st.shape_serial;
        }
        self.seq = self.seq.wrapping_add(1);
        let msg = pos_message(&st, self.seq, self.shape_id);
        if ep.send_datagram(&msg.encode()).is_ok() {
            self.positions_sent += 1;
        }
        self.last_serial = st.serial;
        self.last_send = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grx_transport::cursor::ShapeAssembler;
    use grx_transport::{Config, Event};
    use grx_video::CursorImage;

    fn img(color: u8) -> CursorImage {
        CursorImage { width: 4, height: 4, hot_x: 1, hot_y: 2, rgba: vec![color; 64], xor: None }
    }

    /// Fait passer tout ce que `host` émet vers `client` ; renvoie (formes, positions) reçues.
    fn pump(host: &mut Endpoint, client: &mut Endpoint, asm: &mut ShapeAssembler) -> (Vec<u32>, Vec<CursorPos>) {
        let (mut shapes, mut pos) = (vec![], vec![]);
        while let Some(p) = host.poll_transmit(0) {
            client.handle_datagram(0, &p);
        }
        while let Some(ev) = client.poll_event() {
            match ev {
                Event::Control(m) => {
                    if let Some((id, _)) = asm.push(&m) {
                        shapes.push(id);
                    }
                }
                Event::Datagram(d) => pos.extend(CursorPos::decode(&d)),
                _ => {}
            }
        }
        (shapes, pos)
    }

    #[test]
    fn shape_sent_once_then_positions_only() {
        let t = Arc::new(CursorTracker::new());
        let mut s = CursorSender::new(t.clone());
        let (mut h, mut c) = (Endpoint::new(Config::default()), Endpoint::new(Config::default()));
        let mut asm = ShapeAssembler::default();
        s.drive(&mut h);
        assert_eq!(pump(&mut h, &mut c, &mut asm), (vec![], vec![]), "rien avant la première lecture DXGI");

        t.update(Some((true, 959, 538)), Some(img(200)), 1920, 1080);
        s.drive(&mut h);
        let (shapes, pos) = pump(&mut h, &mut c, &mut asm);
        assert_eq!(shapes.len(), 1);
        assert_eq!(pos.len(), 1);
        let p = pos[0];
        assert!(p.visible);
        assert_eq!(p.shape_id, shapes[0]);
        assert_eq!((p.x, p.y), (to_fixed(960, 1920), to_fixed(540, 1080)), "point chaud = coin + hotspot");
        assert_eq!((p.src_w, p.src_h), (1920, 1080));

        // Changement de forme, puis retour à la première : celle-ci n'est pas renvoyée.
        std::thread::sleep(MIN_GAP);
        t.update(None, Some(img(10)), 1920, 1080);
        s.drive(&mut h);
        assert_eq!(pump(&mut h, &mut c, &mut asm).0.len(), 1);
        std::thread::sleep(MIN_GAP);
        t.update(None, Some(img(200)), 1920, 1080);
        s.drive(&mut h);
        let (shapes2, pos2) = pump(&mut h, &mut c, &mut asm);
        assert!(shapes2.is_empty(), "forme déjà en cache côté client");
        assert_eq!(pos2[0].shape_id, shapes[0]);
        assert_eq!(s.shapes_sent, 2);

        // Curseur caché par le jeu.
        std::thread::sleep(MIN_GAP);
        t.update(Some((false, 959, 538)), None, 1920, 1080);
        s.drive(&mut h);
        let (_, pos3) = pump(&mut h, &mut c, &mut asm);
        assert!(!pos3[0].visible);
        assert!(pos3[0].seq > pos2[0].seq);

        // Sans changement : rien avant REFRESH.
        s.drive(&mut h);
        assert!(pump(&mut h, &mut c, &mut asm).1.is_empty());
    }
}
