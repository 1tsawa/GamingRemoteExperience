//! Format sur le fil du payload applicatif (le contenu d'un paquet `Session::seal`).
//!
//! Octet 0 : `kind` (3 bits bas) | `flags` (5 bits hauts).
//!
//! ```text
//! VIDEO    b0[flags: 0=keyframe 1=parité] tseq u16 | frame_id u32 | frame_len u32 |
//!          shard_size u16 | kmax u8 | m u8 | group u16 | idx u8 | fragment...   (18 octets d'en-tête)
//! AUDIO    b0 | tseq u16 | aseq u16 | pts_us u64 | données Opus...              (13 octets)
//! RELIABLE b0[flags: 0=canal (0 Input, 1 Control)] | seq u16 | message...       (3 octets)
//! DATAGRAM b0 = RELIABLE avec flags bit 1 (F_DATAGRAM) | message...  (1 octet, non fiable)
//! ACK      b0[flags: 0=canal] | cum u16 (prochain seq attendu) | sack u64      (11 octets)
//! PING     b0[flags: 0=pong] | ts_us u64 (écho tel quel)                        (9 octets)
//! REPORT   b0 | loss u8 (x/255) | recv_kbps u32 | frames_lost u16               (8 octets)
//! PATH_CHALLENGE / PATH_RESPONSE  b0 | token 8 octets (écho tel quel)                  (9 octets)
//! ```
//! Tous les entiers sont little-endian. `tseq` est un compteur de paquets média (vidéo+audio)
//! attribué à l'émission réelle : il sert uniquement à mesurer la perte côté récepteur.

use crate::Channel;

pub const K_VIDEO: u8 = 0;
pub const K_AUDIO: u8 = 1;
pub const K_RELIABLE: u8 = 2;
pub const K_ACK: u8 = 3;
pub const K_PING: u8 = 4;
pub const K_REPORT: u8 = 5;
/// Validation de chemin (migration d'adresse) : le challenge est émis par le host vers la nouvelle
/// adresse, la réponse (écho des 8 octets) revient de cette adresse.
pub const K_PATH_CHALLENGE: u8 = 6;
pub const K_PATH_RESPONSE: u8 = 7;
/// Drapeau d'un paquet `K_RELIABLE` : datagramme applicatif NON fiable (pas de seq, pas d'ACK).
/// Les 8 types sont pris, d'où ce drapeau plutôt qu'un nouveau `kind`.
pub const F_DATAGRAM: u8 = 2;

pub const VIDEO_HDR: usize = 18;
pub const AUDIO_HDR: usize = 13;
pub const RELIABLE_HDR: usize = 3;
pub const ACK_LEN: usize = 11;
pub const PING_LEN: usize = 9;
pub const REPORT_LEN: usize = 8;
pub const PATH_LEN: usize = 9;

/// Paquet `PATH_CHALLENGE` portant 8 octets aléatoires.
pub fn path_challenge(token: [u8; 8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(PATH_LEN);
    v.push(b0(K_PATH_CHALLENGE, 0));
    v.extend_from_slice(&token);
    v
}

/// Les 8 octets d'un paquet `PATH_RESPONSE`, s'il en est un.
pub fn parse_path_response(p: &[u8]) -> Option<[u8; 8]> {
    if p.len() == PATH_LEN && kind_of(p[0]) == K_PATH_RESPONSE {
        p[1..9].try_into().ok()
    } else {
        None
    }
}

#[inline]
pub fn b0(kind: u8, flags: u8) -> u8 {
    kind | (flags << 3)
}
#[inline]
pub fn kind_of(b: u8) -> u8 {
    b & 0x07
}
#[inline]
pub fn flags_of(b: u8) -> u8 {
    b >> 3
}

/// Index 0/1 du canal fiable (Input / Control).
pub fn rel_index(c: Channel) -> Option<usize> {
    match c {
        Channel::Input => Some(0),
        Channel::Control => Some(1),
        _ => None,
    }
}

// ───────────── comparaison de numéros de séquence avec rebouclage ─────────────

#[inline]
pub fn seq16_lt(a: u16, b: u16) -> bool {
    (a.wrapping_sub(b) as i16) < 0
}
#[inline]
pub fn seq32_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

// ───────────────────────────── vidéo ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoHdr {
    pub keyframe: bool,
    pub parity: bool,
    pub tseq: u16,
    pub frame_id: u32,
    pub frame_len: u32,
    pub shard_size: u16,
    pub kmax: u8,
    pub m: u8,
    pub group: u16,
    pub idx: u8,
}

impl VideoHdr {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(b0(K_VIDEO, (self.keyframe as u8) | ((self.parity as u8) << 1)));
        out.extend_from_slice(&self.tseq.to_le_bytes());
        out.extend_from_slice(&self.frame_id.to_le_bytes());
        out.extend_from_slice(&self.frame_len.to_le_bytes());
        out.extend_from_slice(&self.shard_size.to_le_bytes());
        out.push(self.kmax);
        out.push(self.m);
        out.extend_from_slice(&self.group.to_le_bytes());
        out.push(self.idx);
    }

    pub fn parse(p: &[u8]) -> Option<(VideoHdr, &[u8])> {
        if p.len() < VIDEO_HDR || kind_of(p[0]) != K_VIDEO {
            return None;
        }
        let f = flags_of(p[0]);
        Some((
            VideoHdr {
                keyframe: f & 1 != 0,
                parity: f & 2 != 0,
                tseq: u16::from_le_bytes([p[1], p[2]]),
                frame_id: u32::from_le_bytes([p[3], p[4], p[5], p[6]]),
                frame_len: u32::from_le_bytes([p[7], p[8], p[9], p[10]]),
                shard_size: u16::from_le_bytes([p[11], p[12]]),
                kmax: p[13],
                m: p[14],
                group: u16::from_le_bytes([p[15], p[16]]),
                idx: p[17],
            },
            &p[VIDEO_HDR..],
        ))
    }
}

// ───────────────────────────── rapport récepteur ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Report {
    /// Fraction de paquets média perdus sur l'intervalle, en 1/255.
    pub loss_q8: u8,
    pub recv_kbps: u32,
    /// Images abandonnées par le récepteur sur l'intervalle.
    pub frames_lost: u16,
}

impl Report {
    pub fn loss(&self) -> f32 {
        self.loss_q8 as f32 / 255.0
    }
    pub fn write(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(REPORT_LEN);
        v.push(b0(K_REPORT, 0));
        v.push(self.loss_q8);
        v.extend_from_slice(&self.recv_kbps.to_le_bytes());
        v.extend_from_slice(&self.frames_lost.to_le_bytes());
        v
    }
    pub fn parse(p: &[u8]) -> Option<Report> {
        if p.len() < REPORT_LEN || kind_of(p[0]) != K_REPORT {
            return None;
        }
        Some(Report {
            loss_q8: p[1],
            recv_kbps: u32::from_le_bytes([p[2], p[3], p[4], p[5]]),
            frames_lost: u16::from_le_bytes([p[6], p[7]]),
        })
    }
}

// ───────────────────────────── messages de contrôle ─────────────────────────────

const CTL_KEYFRAME: u8 = 1;
const CTL_CUSTOM: u8 = 2;

/// Message du canal Control (fiable, ordonné).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlMsg {
    /// Le récepteur vidéo demande une image clé (IDR).
    KeyframeRequest,
    /// Octets applicatifs opaques (ex : changement de résolution, presse-papiers...).
    Custom(Vec<u8>),
}

impl ControlMsg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ControlMsg::KeyframeRequest => vec![CTL_KEYFRAME],
            ControlMsg::Custom(b) => {
                let mut v = Vec::with_capacity(1 + b.len());
                v.push(CTL_CUSTOM);
                v.extend_from_slice(b);
                v
            }
        }
    }
    pub fn decode(p: &[u8]) -> Option<ControlMsg> {
        match p.first()? {
            &CTL_KEYFRAME => Some(ControlMsg::KeyframeRequest),
            &CTL_CUSTOM => Some(ControlMsg::Custom(p[1..].to_vec())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_hdr_roundtrip() {
        let h = VideoHdr {
            keyframe: true,
            parity: true,
            tseq: 513,
            frame_id: 0xdead_beef,
            frame_len: 123456,
            shard_size: 1182,
            kmax: 100,
            m: 12,
            group: 3,
            idx: 111,
        };
        let mut v = Vec::new();
        h.write(&mut v);
        assert_eq!(v.len(), VIDEO_HDR);
        v.extend_from_slice(b"xyz");
        let (h2, body) = VideoHdr::parse(&v).unwrap();
        assert_eq!(h, h2);
        assert_eq!(body, b"xyz");
        assert!(VideoHdr::parse(&v[..VIDEO_HDR - 1]).is_none());
    }

    #[test]
    fn report_and_control_roundtrip() {
        let r = Report { loss_q8: 7, recv_kbps: 99_000, frames_lost: 3 };
        assert_eq!(Report::parse(&r.write()), Some(r));
        for m in [ControlMsg::KeyframeRequest, ControlMsg::Custom(vec![1, 2, 3])] {
            assert_eq!(ControlMsg::decode(&m.encode()), Some(m));
        }
        assert_eq!(ControlMsg::decode(&[]), None);
        assert_eq!(ControlMsg::decode(&[99]), None);
    }

    #[test]
    fn seq_compare_wraps() {
        assert!(seq16_lt(65535, 0));
        assert!(!seq16_lt(0, 65535));
        assert!(seq32_lt(u32::MAX, 3));
    }
}
