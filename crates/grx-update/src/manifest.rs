//! `manifest.json` : description signée d'une release.
//!
//! ```json
//! { "version": "0.2.1", "published": "2026-11-13T12:00:00Z", "setup_name": "GRX-Setup-0.2.1.exe",
//!   "sha256": "<64 hex>", "size": 12345678, "notes": "…", "min_version": "0.2.0" }
//! ```
//! `min_version` (facultatif) : version minimale depuis laquelle la mise à jour automatique est possible.

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{UpdateError, MAX_SETUP_BYTES};

/// Longueur maximale des notes affichées.
pub const MAX_NOTES_CHARS: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub published: String,
    pub setup_name: String,
    pub sha256: String,
    pub size: u64,
    #[serde(default)]
    pub notes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
}

/// Manifeste validé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidManifest {
    pub version: Version,
    pub published: String,
    pub setup_name: String,
    pub sha256: [u8; 32],
    pub size: u64,
    /// Notes nettoyées (caractères de contrôle retirés sauf retours à la ligne, longueur bornée).
    pub notes: String,
    pub min_version: Option<Version>,
}

/// Nom attendu du setup pour une version.
pub fn setup_name_for(version: &Version) -> String {
    format!("GRX-Setup-{version}.exe")
}

fn strict_version(s: &str, what: &str) -> Result<Version, UpdateError> {
    let v = Version::parse(s).map_err(|_| UpdateError::BadManifest(format!("{what} « {} » n'est pas une version semver", clip(s))))?;
    if !v.build.is_empty() {
        return Err(UpdateError::BadManifest(format!("{what} : métadonnées de build refusées")));
    }
    Ok(v)
}

fn clip(s: &str) -> String {
    s.chars().take(40).filter(|c| !c.is_control()).collect()
}

pub fn sanitize_notes(s: &str) -> String {
    let mut out: String = s
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .filter(|c| !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .take(MAX_NOTES_CHARS)
        .collect();
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, UpdateError> {
        serde_json::from_slice(bytes).map_err(|e| UpdateError::BadManifest(format!("JSON : {}", clip(&e.to_string()))))
    }

    pub fn validate(&self) -> Result<ValidManifest, UpdateError> {
        let version = strict_version(&self.version, "version")?;
        let min_version = self.min_version.as_deref().map(|m| strict_version(m, "min_version")).transpose()?;
        if let Some(min) = &min_version {
            if *min > version {
                return Err(UpdateError::BadManifest("min_version supérieure à version".into()));
            }
        }
        let expected = setup_name_for(&version);
        if self.setup_name != expected {
            return Err(UpdateError::BadManifest(format!("setup_name attendu : {expected}")));
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(UpdateError::BadManifest("sha256 : 64 caractères hexadécimaux attendus".into()));
        }
        let raw = data_encoding::HEXLOWER_PERMISSIVE
            .decode(self.sha256.as_bytes())
            .map_err(|_| UpdateError::BadManifest("sha256 illisible".into()))?;
        let sha256: [u8; 32] = raw.try_into().map_err(|_| UpdateError::BadManifest("sha256 illisible".into()))?;
        if self.size == 0 || self.size > MAX_SETUP_BYTES {
            return Err(UpdateError::BadManifest("taille du setup hors limites".into()));
        }
        if self.published.len() > 64 {
            return Err(UpdateError::BadManifest("date de publication invalide".into()));
        }
        Ok(ValidManifest {
            version,
            published: clip(&self.published),
            setup_name: self.setup_name.clone(),
            sha256,
            size: self.size,
            notes: sanitize_notes(&self.notes),
            min_version,
        })
    }
}
