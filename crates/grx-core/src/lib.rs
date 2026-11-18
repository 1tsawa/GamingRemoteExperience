//! grx-core : briques communes host/client.
//!
//! - `identity` : paire de clés X25519 de la machine + empreinte lisible
//! - `invite`   : lien d'invitation `grx://ip:port/<clé host>/<jeton>`
//! - `trust`    : liste des clients autorisés + invitations à usage unique (côté host)
//! - `session`  : poignée de main Noise IK sur UDP + transport chiffré (AES-256-GCM)

pub mod error;
pub mod identity;
pub mod invite;
pub mod pairing;
pub mod codec_neg;
pub mod displays;
pub mod elevation;
pub mod profiles;
pub mod secure;
pub mod secure_notice;
pub mod session;
pub mod stream_settings;
pub mod trust;
pub mod util;

pub use error::{Error, Result};
pub use identity::{fingerprint, Identity, PublicKey};
pub use invite::{Invite, Token};
pub use profiles::{Profile, ProfileStore};
pub use pairing::{PairStatus, PKT_KEY_REQ, PKT_KEY_RESP};
pub use session::{ClientHandshake, HostAuth, InitOutcome, Session, PKT_DATA, PKT_INIT, PKT_RESP};
pub use trust::{Role, TrustStore};
