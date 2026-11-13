//! grx-transport : couche de transport média temps réel, SANS I/O.
//!
//! Elle s'insère au-dessus du payload des paquets chiffrés de `grx_core::Session`
//! (`seal`/`open`) : le cœur ([`Endpoint`]) n'ouvre aucune socket, ne chiffre rien et reçoit
//! l'heure en paramètre, donc tout est testable de façon déterministe (voir [`sim`]).
//!
//! | Canal   | Fiabilité                                         | Contenu                    |
//! |---------|---------------------------------------------------|----------------------------|
//! | Video   | non fiable, FEC Reed-Solomon, pacing, péremption  | images encodées            |
//! | Audio   | non fiable, non paced                             | paquets Opus               |
//! | Input   | fiable, ordonné, ACK/SACK + retransmission        | clavier, souris, manettes  |
//! | Control | fiable, ordonné, ACK/SACK + retransmission        | demande d'IDR, divers      |
//! | Datagram| non fiable, non paced, dernier état gagnant       | position du curseur        |
//!
//! Modules : [`wire`] (format), [`fec`] (Reed-Solomon, CRC), [`video`] (fragmentation et
//! réassemblage), [`reliable`] (ARQ), [`netstats`] (RTT, perte), [`bitrate`] (contrôle de
//! débit), [`pacer`], [`endpoint`] (assemblage), [`sim`] (simulateur de lien).

pub mod bitrate;
pub mod cursor;
pub mod endpoint;
pub mod fec;
pub mod netstats;
pub mod pacer;
pub mod reliable;
pub mod sim;
pub mod video;
pub mod wire;

pub use bitrate::{BitrateConfig, BitrateController, NetworkSample};
pub use endpoint::{AudioPacket, Config, Endpoint, Event, Stats};
pub use pacer::Pacer;
pub use video::{FecConfig, VideoFrame, VideoRxConfig, VideoRxStats};
pub use wire::ControlMsg;

/// Temps monotone en microsecondes, fourni par l'appelant.
pub type Micros = u64;

/// Charge utile applicative maximale par défaut (sûr pour un MTU de 1280 avec IP/UDP et
/// les 29 octets de `Session`).
pub const DEFAULT_MAX_PAYLOAD: usize = 1200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    Video,
    Audio,
    Input,
    Control,
}

impl Channel {
    pub fn is_reliable(self) -> bool {
        matches!(self, Channel::Input | Channel::Control)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    Empty,
    TooLarge,
    /// File fiable pleine : le pair n'acquitte plus.
    Backlog,
    InvalidConfig,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SendError::Empty => "message vide",
            SendError::TooLarge => "message trop grand",
            SendError::Backlog => "file fiable pleine",
            SendError::InvalidConfig => "configuration invalide",
        })
    }
}

impl std::error::Error for SendError {}
