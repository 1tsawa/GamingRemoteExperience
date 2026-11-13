use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error("paquet invalide: {0}")]
    BadPacket(&'static str),
    #[error("version de protocole non supportée: {0}")]
    BadVersion(u16),
    #[error("horodatage hors tolérance (horloge désynchronisée ou rejeu)")]
    BadTimestamp,
    #[error("client non autorisé")]
    Unauthorized,
    #[error("une autre clé a une session active")]
    Busy,
    #[error("paquet rejoué ou trop ancien")]
    Replay,
    #[error("lien d'invitation invalide: {0}")]
    BadInvite(String),
    #[error("identité corrompue: {0}")]
    BadIdentity(&'static str),
}
