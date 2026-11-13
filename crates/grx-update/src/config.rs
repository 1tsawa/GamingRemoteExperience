//! Configuration des mises à jour : dépôt GitHub, clé publique de signature, version courante.

use ed25519_dalek::VerifyingKey;
use semver::Version;

use crate::UpdateError;

/// Dépôt GitHub qui héberge les releases (`OWNER/REPO`). **Seul endroit à modifier** après
/// création du dépôt (voir docs/RELEASE.md). Tant qu'il vaut `CHANGE_ME/…`, aucune requête n'est faite.
pub const GITHUB_REPO: &str = "1tsawa/GamingRemoteExperience";

/// API GitHub par défaut.
pub const DEFAULT_API_BASE: &str = "https://api.github.com";

/// Variable d'environnement qui remplace l'URL de base de l'API (tests avec un faux serveur local).
/// La signature reste exigée : elle ne permet pas d'installer un setup non signé.
pub const ENV_API_BASE: &str = "GRX_UPDATE_URL";

/// User-Agent générique (aucune information sur la machine ou l'utilisateur).
pub const USER_AGENT: &str = concat!("grx-updater/", env!("CARGO_PKG_VERSION"));

/// Version de cette build (version unique du workspace, `[workspace.package]`).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Clé publique Ed25519 des releases (hexadécimal), écrite par `tools/new-signing-key.ps1`.
/// Une build de TEST peut la remplacer à la compilation par `GRX_UPDATE_PUBKEY_HEX`
/// (jamais à l'exécution : une variable d'environnement ne peut pas changer la clé de confiance).
const EMBEDDED_PUBKEY_HEX: &str = include_str!("../keys/release.pub");

pub fn public_key_hex() -> &'static str {
    match option_env!("GRX_UPDATE_PUBKEY_HEX") {
        Some(k) if !k.trim().is_empty() => k.trim(),
        _ => EMBEDDED_PUBKEY_HEX.trim(),
    }
}

pub fn parse_public_key(hex: &str) -> Result<VerifyingKey, UpdateError> {
    let bytes = data_encoding::HEXLOWER_PERMISSIVE
        .decode(hex.trim().as_bytes())
        .map_err(|_| UpdateError::BadPublicKey("hexadécimal invalide".into()))?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| UpdateError::BadPublicKey("32 octets attendus".into()))?;
    VerifyingKey::from_bytes(&arr).map_err(|_| UpdateError::BadPublicKey("point invalide".into()))
}

/// Clé publique intégrée au binaire.
pub fn release_public_key() -> Result<VerifyingKey, UpdateError> {
    parse_public_key(public_key_hex())
}

pub fn current_version() -> Version {
    Version::parse(CURRENT_VERSION).expect("version du workspace au format semver")
}

/// Le dépôt a-t-il été renseigné ?
pub fn repo_configured(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |s: Option<&str>| {
        s.is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)))
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none() && !repo.starts_with("CHANGE_ME/")
}

#[derive(Debug, Clone)]
pub struct UpdateConfig {
    /// URL de base de l'API, sans `/` final.
    pub api_base: String,
    pub repo: String,
    /// Autorise `http://127.0.0.1`/`localhost` (uniquement quand l'URL de base est surchargée).
    pub allow_loopback_http: bool,
    pub public_key: VerifyingKey,
    pub current: Version,
}

impl UpdateConfig {
    /// Configuration réelle : constantes ci-dessus, `GRX_UPDATE_URL` éventuel.
    pub fn from_env() -> Result<Self, UpdateError> {
        let override_base = std::env::var(ENV_API_BASE).ok().filter(|s| !s.trim().is_empty());
        Self::build(override_base, GITHUB_REPO, release_public_key()?, current_version())
    }

    pub fn build(
        override_base: Option<String>,
        repo: &str,
        public_key: VerifyingKey,
        current: Version,
    ) -> Result<Self, UpdateError> {
        let (api_base, allow_loopback_http) = match override_base {
            Some(b) => {
                let b = b.trim().trim_end_matches('/').to_string();
                crate::http::check_url(&b, true)?;
                (b, true)
            }
            None => (DEFAULT_API_BASE.to_string(), false),
        };
        Ok(Self { api_base, repo: repo.to_string(), allow_loopback_http, public_key, current })
    }

    /// Les mises à jour sont-elles utilisables ? (dépôt renseigné, ou serveur de test explicite)
    pub fn is_configured(&self) -> bool {
        repo_configured(&self.repo) || (self.allow_loopback_http && !self.repo.is_empty())
    }

    pub fn latest_release_url(&self) -> Result<String, UpdateError> {
        if !self.is_configured() {
            return Err(UpdateError::NotConfigured);
        }
        Ok(format!("{}/repos/{}/releases/latest", self.api_base, self.repo))
    }
}
