//! Protocole du curseur distant (host -> client), sans I/O.
//!
//! * **Forme** : canal Control (fiable, ordonné), découpée en morceaux [`SHAPE_MAGIC`]. Une forme
//!   est identifiée par un hachage de son contenu (`shape_id`) : le host ne renvoie pas une forme
//!   que le client a déjà en cache (flèche <-> main <-> texte...). Les deux côtés tiennent un cache
//!   FIFO de [`SHAPE_CACHE`] entrées ; comme le canal est ordonné, les évictions sont identiques.
//! * **Position/visibilité** : datagramme NON fiable ([`crate::Endpoint::send_datagram`]),
//!   [`POS_MAGIC`], numéro de séquence (le plus récent gagne), renvoyé périodiquement par le host
//!   pour réparer une perte.
//!
//! ```text
//! Forme (corps)  : w u16 | h u16 | hot_x u16 | hot_y u16 | flags u8 (bit0 : calque XOR)
//!                  | RLE(rgba) [| RLE(xor)]
//! Morceau        : "GCS1" | shape_id u32 | total u32 | offset u32 | octets du corps
//! Position       : "GCP1" | seq u32 | flags u8 (bit0 visible) | shape_id u32
//!                  | x i32 | y i32 (point chaud, fraction de la largeur/hauteur capturée x 65536)
//!                  | src_w u16 | src_h u16 (taille de l'image capturée) | ts_us u64 (horloge du host)
//! RLE (pixels 4 octets) : c < 128 -> c+1 pixels littéraux suivent ; c >= 128 -> le pixel
//!                  suivant est répété c-126 fois (2..=129).
//! ```
//! Tout est little-endian. Le décodage est borné (dimensions <= [`MAX_DIM`], corps <=
//! [`MAX_BODY`]) et ne panique jamais sur des données forgées.

use std::collections::VecDeque;

pub const SHAPE_MAGIC: &[u8; 4] = b"GCS1";
pub const POS_MAGIC: &[u8; 4] = b"GCP1";
/// Côté maximal d'une forme (pixels).
pub const MAX_DIM: u16 = 256;
/// Taille maximale du corps encodé (pire cas RLE de deux calques 256x256, plus l'en-tête).
pub const MAX_BODY: usize = 9 + 2 * (MAX_DIM as usize * MAX_DIM as usize * 4 * 129 / 128 + 4);
/// Octets de corps par morceau (tient dans un message Control de 1200 o avec les en-têtes).
pub const CHUNK_DATA: usize = 1100;
pub const CHUNK_HDR: usize = 16;
pub const POS_LEN: usize = 33;
/// Formes gardées en cache des deux côtés (FIFO).
pub const SHAPE_CACHE: usize = 32;

/// Forme de curseur : `rgba`/`xor` = `w*h*4` octets, R,G,B,A, alpha non prémultiplié.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
    /// Calque d'inversion (rendu `src*(1-dst) + dst*(1-src)`).
    pub xor: Option<Vec<u8>>,
}

/// État du curseur, en coordonnées normalisées par rapport à l'image capturée.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CursorPos {
    pub seq: u32,
    pub visible: bool,
    pub shape_id: u32,
    /// Point chaud, fraction de `src_w` (resp. `src_h`) x 65536 (peut sortir de [0, 65536]).
    pub x: i32,
    pub y: i32,
    pub src_w: u16,
    pub src_h: u16,
    /// Horodatage de la mise à jour côté host (µs, epoch Unix) : mesure de latence.
    pub ts_us: u64,
}

impl CursorPos {
    pub fn x_norm(&self) -> f32 {
        self.x as f32 / 65536.0
    }
    pub fn y_norm(&self) -> f32 {
        self.y as f32 / 65536.0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(POS_LEN);
        v.extend_from_slice(POS_MAGIC);
        v.extend_from_slice(&self.seq.to_le_bytes());
        v.push(self.visible as u8);
        v.extend_from_slice(&self.shape_id.to_le_bytes());
        v.extend_from_slice(&self.x.to_le_bytes());
        v.extend_from_slice(&self.y.to_le_bytes());
        v.extend_from_slice(&self.src_w.to_le_bytes());
        v.extend_from_slice(&self.src_h.to_le_bytes());
        v.extend_from_slice(&self.ts_us.to_le_bytes());
        v
    }

    pub fn decode(p: &[u8]) -> Option<CursorPos> {
        if p.len() != POS_LEN || &p[..4] != POS_MAGIC {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]]);
        let u16_at = |i: usize| u16::from_le_bytes([p[i], p[i + 1]]);
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&p[25..33]);
        Some(CursorPos {
            seq: u32_at(4),
            visible: p[8] & 1 != 0,
            shape_id: u32_at(9),
            x: u32_at(13) as i32,
            y: u32_at(17) as i32,
            src_w: u16_at(21),
            src_h: u16_at(23),
            ts_us: u64::from_le_bytes(ts),
        })
    }
}

/// `seq` plus récent que `last` (avec rebouclage).
pub fn seq_newer(seq: u32, last: u32) -> bool {
    (seq.wrapping_sub(last) as i32) > 0
}

/// Convertit une position en pixels de l'image capturée en fraction x 65536 (saturant).
pub fn to_fixed(px: i32, size: u32) -> i32 {
    if size == 0 {
        return 0;
    }
    ((px as i64 * 65536) / size as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

// ───────────────────────────── RLE ─────────────────────────────

/// Compresse des pixels de 4 octets (`data.len()` multiple de 4).
pub fn rle_encode(data: &[u8], out: &mut Vec<u8>) {
    let px: Vec<&[u8]> = data.chunks_exact(4).collect();
    let mut i = 0;
    while i < px.len() {
        let mut run = 1;
        while i + run < px.len() && run < 129 && px[i + run] == px[i] {
            run += 1;
        }
        if run >= 2 {
            out.push((run + 126) as u8);
            out.extend_from_slice(px[i]);
            i += run;
            continue;
        }
        // Littéraux jusqu'au prochain doublon (ou 128 pixels).
        let start = i;
        let mut n = 0;
        while i < px.len() && n < 128 && !(i + 1 < px.len() && px[i + 1] == px[i]) {
            i += 1;
            n += 1;
        }
        if n == 0 {
            // px[i] == px[i+1] : traité par le cas répétition au tour suivant.
            continue;
        }
        out.push((n - 1) as u8);
        for p in &px[start..start + n] {
            out.extend_from_slice(p);
        }
    }
}

/// Décompresse exactement `npix` pixels depuis `src` ; renvoie les pixels et les octets lus.
pub fn rle_decode(src: &[u8], npix: usize) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::with_capacity(npix.checked_mul(4)?);
    let mut i = 0;
    while out.len() < npix * 4 {
        let c = *src.get(i)? as usize;
        i += 1;
        if c < 128 {
            let n = c + 1;
            let bytes = src.get(i..i.checked_add(n * 4)?)?;
            if out.len() + n * 4 > npix * 4 {
                return None;
            }
            out.extend_from_slice(bytes);
            i += n * 4;
        } else {
            let n = c - 126;
            let p = src.get(i..i + 4)?;
            if out.len() + n * 4 > npix * 4 {
                return None;
            }
            for _ in 0..n {
                out.extend_from_slice(p);
            }
            i += 4;
        }
    }
    Some((out, i))
}

// ───────────────────────────── forme ─────────────────────────────

impl Shape {
    /// Vérifie dimensions et longueurs (borne [`MAX_DIM`], hotspot dans l'image).
    pub fn is_valid(&self) -> bool {
        let n = self.width as usize * self.height as usize * 4;
        self.width > 0
            && self.height > 0
            && self.width <= MAX_DIM
            && self.height <= MAX_DIM
            && self.hot_x < self.width
            && self.hot_y < self.height
            && self.rgba.len() == n
            && self.xor.as_ref().is_none_or(|x| x.len() == n)
    }

    /// Corps encodé (`None` si la forme est invalide).
    pub fn encode_body(&self) -> Option<Vec<u8>> {
        if !self.is_valid() {
            return None;
        }
        let mut v = Vec::with_capacity(64);
        for x in [self.width, self.height, self.hot_x, self.hot_y] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.push(self.xor.is_some() as u8);
        rle_encode(&self.rgba, &mut v);
        if let Some(x) = &self.xor {
            rle_encode(x, &mut v);
        }
        Some(v)
    }

    pub fn decode_body(b: &[u8]) -> Option<Shape> {
        if b.len() < 9 || b.len() > MAX_BODY {
            return None;
        }
        let u = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let (width, height, hot_x, hot_y, flags) = (u(0), u(2), u(4), u(6), b[8]);
        if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM || flags > 1 {
            return None;
        }
        let npix = width as usize * height as usize;
        let (rgba, n1) = rle_decode(&b[9..], npix)?;
        let mut used = 9 + n1;
        let xor = if flags & 1 != 0 {
            let (x, n2) = rle_decode(&b[used..], npix)?;
            used += n2;
            Some(x)
        } else {
            None
        };
        if used != b.len() {
            return None;
        }
        let s = Shape { width, height, hot_x, hot_y, rgba, xor };
        s.is_valid().then_some(s)
    }
}

/// Identifiant d'une forme : FNV-1a 64 bits replié sur 32 bits, jamais 0 (0 = « aucune forme »).
pub fn shape_id(body: &[u8]) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in body {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let id = (h ^ (h >> 32)) as u32;
    if id == 0 {
        1
    } else {
        id
    }
}

/// Découpe un corps en messages Control (`"GCS1"` + en-tête + morceau).
pub fn shape_chunks(id: u32, body: &[u8]) -> Vec<Vec<u8>> {
    let total = body.len() as u32;
    body.chunks(CHUNK_DATA)
        .enumerate()
        .map(|(i, c)| {
            let mut v = Vec::with_capacity(CHUNK_HDR + c.len());
            v.extend_from_slice(SHAPE_MAGIC);
            v.extend_from_slice(&id.to_le_bytes());
            v.extend_from_slice(&total.to_le_bytes());
            v.extend_from_slice(&((i * CHUNK_DATA) as u32).to_le_bytes());
            v.extend_from_slice(c);
            v
        })
        .collect()
}

pub fn is_shape_chunk(m: &[u8]) -> bool {
    m.len() >= 4 && &m[..4] == SHAPE_MAGIC
}

/// Réassemblage côté client. Les morceaux arrivent dans l'ordre (canal fiable ordonné) ; tout
/// écart (offset inattendu, taille hors bornes, id qui change en cours) abandonne la forme en
/// cours sans paniquer.
#[derive(Default)]
pub struct ShapeAssembler {
    cur: Option<(u32, u32, Vec<u8>)>,
}

impl ShapeAssembler {
    /// Ajoute un morceau ; renvoie `(id, forme)` quand une forme complète et valide est reçue.
    pub fn push(&mut self, m: &[u8]) -> Option<(u32, Shape)> {
        if m.len() < CHUNK_HDR || !is_shape_chunk(m) {
            return None;
        }
        let u = |i: usize| u32::from_le_bytes([m[i], m[i + 1], m[i + 2], m[i + 3]]);
        let (id, total, off) = (u(4), u(8), u(12));
        let data = &m[CHUNK_HDR..];
        if total as usize > MAX_BODY || total == 0 || data.is_empty() {
            self.cur = None;
            return None;
        }
        if off == 0 {
            self.cur = Some((id, total, Vec::with_capacity(total as usize)));
        }
        let (cid, ctotal, buf) = self.cur.as_mut()?;
        if *cid != id || *ctotal != total || buf.len() != off as usize || buf.len() + data.len() > total as usize {
            self.cur = None;
            return None;
        }
        buf.extend_from_slice(data);
        if buf.len() < total as usize {
            return None;
        }
        let (id, _, body) = self.cur.take()?;
        let s = Shape::decode_body(&body)?;
        // Le hachage du contenu doit correspondre : un id forgé ne peut pas empoisonner le cache.
        (shape_id(&body) == id).then_some((id, s))
    }
}

/// Cache FIFO borné `id -> valeur` (même politique d'éviction côté host et client).
pub struct FifoCache<T> {
    cap: usize,
    items: VecDeque<(u32, T)>,
}

impl<T> FifoCache<T> {
    pub fn new(cap: usize) -> Self {
        FifoCache { cap: cap.max(1), items: VecDeque::new() }
    }
    pub fn get(&self, id: u32) -> Option<&T> {
        self.items.iter().find(|(k, _)| *k == id).map(|(_, v)| v)
    }
    pub fn contains(&self, id: u32) -> bool {
        self.get(id).is_some()
    }
    /// Insère (ou ignore si déjà présent, sans changer l'ordre d'éviction).
    pub fn insert(&mut self, id: u32, v: T) {
        if self.contains(id) {
            return;
        }
        if self.items.len() >= self.cap {
            self.items.pop_front();
        }
        self.items.push_back((id, v));
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn clear(&mut self) {
        self.items.clear();
    }
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
