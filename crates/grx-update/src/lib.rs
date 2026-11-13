//! grx-update : mises à jour signées de GamingRemoteExperience, hébergées sur GitHub Releases.
//!
//! Chaque release contient `GRX-Setup-<ver>.exe`, `manifest.json` et `manifest.json.sig`
//! (signature Ed25519 du manifeste, clé publique intégrée au binaire, voir [`config`]).
//!
//! Ordre des vérifications (voir [`check`] et [`download`]) :
//! 1. réponse GitHub analysée strictement (tailles bornées, URL https uniquement) ;
//! 2. **signature du manifeste vérifiée AVANT toute interprétation** de son contenu ;
//! 3. manifeste validé (semver, nom du setup, sha256, taille), cohérent avec le tag de la release ;
//! 4. version strictement supérieure à la version courante (jamais de retour en arrière) ;
//! 5. setup téléchargé dans un dossier temporaire, taille et SHA-256 comparés au manifeste signé ;
//! 6. setup lancé en silencieux dans le même mode d'installation (utilisateur / machine).

pub mod config;
pub mod guard;
pub mod http;
pub mod install;
pub mod manifest;
pub mod sig;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use semver::Version;
use sha2::{Digest, Sha256};

pub use config::UpdateConfig;
pub use http::{Fetcher, UreqFetcher};
pub use install::InstallMode;
pub use manifest::{Manifest, ValidManifest};

/// Taille maximale de la réponse JSON de l'API GitHub.
pub const MAX_API_BYTES: u64 = 1024 * 1024;
/// Taille maximale du manifeste.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// Taille maximale du fichier de signature (texte hexadécimal).
pub const MAX_SIG_BYTES: u64 = 1024;
/// Taille maximale acceptée pour un setup (le manifeste signé donne la taille exacte).
pub const MAX_SETUP_BYTES: u64 = 512 * 1024 * 1024;

pub const MANIFEST_ASSET: &str = "manifest.json";
pub const SIG_ASSET: &str = "manifest.json.sig";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UpdateError {
    #[error("mises à jour non configurées (dépôt GitHub à renseigner dans grx-update/src/config.rs)")]
    NotConfigured,
    #[error("clé publique de mise à jour invalide : {0}")]
    BadPublicKey(String),
    #[error("réseau : {0}")]
    Network(String),
    #[error("le serveur a répondu {0}")]
    HttpStatus(u16),
    #[error("réponse trop volumineuse (> {0} octets)")]
    TooLarge(u64),
    #[error("URL refusée (https obligatoire) : {0}")]
    InsecureUrl(String),
    #[error("réponse GitHub inattendue : {0}")]
    UnexpectedResponse(String),
    #[error("fichier absent de la release : {0}")]
    MissingAsset(String),
    #[error("SIGNATURE INVALIDE : le manifeste n'a pas été signé par la clé de GamingRemoteExperience")]
    BadSignature,
    #[error("manifeste invalide : {0}")]
    BadManifest(String),
    #[error("le tag de la release ({tag}) ne correspond pas à la version signée ({version})")]
    TagMismatch { tag: String, version: String },
    #[error("intégrité du setup : {0}")]
    Integrity(String),
    #[error("fichier : {0}")]
    Io(String),
    #[error("lancement du setup impossible : {0}")]
    Launch(String),
}

/// Release disponible, déjà vérifiée (signature + manifeste + version supérieure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub manifest: ValidManifest,
    pub setup_url: String,
    pub current: Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Version publiée identique (ou inférieure : jamais de retour en arrière).
    UpToDate { latest: Version },
    /// Nouvelle version applicable automatiquement.
    Available(Available),
    /// Nouvelle version, mais la version courante est trop ancienne pour une mise à jour
    /// automatique (`min_version` du manifeste) : installation manuelle requise.
    ManualRequired { latest: Version, min_version: Version },
}

/// Setup téléchargé dont la taille et le SHA-256 correspondent au manifeste signé.
#[derive(Debug, Clone)]
pub struct VerifiedSetup {
    pub path: PathBuf,
    pub version: Version,
    sha256: [u8; 32],
    size: u64,
}

impl VerifiedSetup {
    /// Revérifie le fichier sur disque (juste avant le lancement).
    pub fn reverify(&self) -> Result<(), UpdateError> {
        let (size, hash) = hash_file(&self.path)?;
        check_integrity(size, &hash, self.size, &self.sha256)
    }
}

// ---------------------------------------------------------------------------------------------
// Réponse de l'API GitHub
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub tag: String,
    pub draft: bool,
    pub prerelease: bool,
    pub assets: Vec<ReleaseAsset>,
}

impl ReleaseInfo {
    pub fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// Analyse la réponse de `GET /repos/<owner>/<repo>/releases/latest` (champs inconnus ignorés,
/// champs utiles obligatoires et typés). Toute URL non https est refusée sauf boucle locale
/// explicitement autorisée (tests).
pub fn parse_release(bytes: &[u8], allow_loopback_http: bool) -> Result<ReleaseInfo, UpdateError> {
    let bad = |m: &str| UpdateError::UnexpectedResponse(m.to_string());
    let v: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| bad("JSON illisible"))?;
    let obj = v.as_object().ok_or_else(|| bad("objet JSON attendu"))?;
    let tag = obj.get("tag_name").and_then(|t| t.as_str()).ok_or_else(|| bad("tag_name absent"))?;
    if tag.is_empty() || tag.len() > 64 {
        return Err(bad("tag_name invalide"));
    }
    let flag = |k: &str| obj.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
    let list = obj.get("assets").and_then(|a| a.as_array()).ok_or_else(|| bad("assets absent"))?;
    if list.len() > 100 {
        return Err(bad("trop de fichiers dans la release"));
    }
    let mut assets = Vec::with_capacity(list.len());
    for a in list {
        let name = a.get("name").and_then(|n| n.as_str()).ok_or_else(|| bad("asset sans nom"))?;
        let url = a.get("browser_download_url").and_then(|n| n.as_str()).ok_or_else(|| bad("asset sans URL"))?;
        let size = a.get("size").and_then(|s| s.as_u64());
        if name.is_empty() || name.len() > 200 {
            return Err(bad("nom d'asset invalide"));
        }
        // Les doublons rendraient le choix ambigu : refusés.
        if assets.iter().any(|x: &ReleaseAsset| x.name == name) {
            return Err(bad("asset en double"));
        }
        assets.push(ReleaseAsset { name: name.to_string(), url: url.to_string(), size });
    }
    for a in &assets {
        http::check_url(&a.url, allow_loopback_http)?;
    }
    Ok(ReleaseInfo { tag: tag.to_string(), draft: flag("draft"), prerelease: flag("prerelease"), assets })
}

// ---------------------------------------------------------------------------------------------
// Vérification
// ---------------------------------------------------------------------------------------------

/// Recherche une mise à jour. N'effectue AUCUNE requête si les mises à jour ne sont pas configurées.
pub fn check(cfg: &UpdateConfig, fetcher: &dyn Fetcher) -> Result<CheckOutcome, UpdateError> {
    let api = cfg.latest_release_url()?;
    let body = fetcher.get(&api, MAX_API_BYTES)?;
    let release = parse_release(&body, cfg.allow_loopback_http)?;
    if release.draft || release.prerelease {
        return Err(UpdateError::UnexpectedResponse("la dernière release est un brouillon ou une préversion".into()));
    }
    let man_asset = release.asset(MANIFEST_ASSET).ok_or_else(|| UpdateError::MissingAsset(MANIFEST_ASSET.into()))?;
    let sig_asset = release.asset(SIG_ASSET).ok_or_else(|| UpdateError::MissingAsset(SIG_ASSET.into()))?;
    let manifest_bytes = fetcher.get(&man_asset.url, MAX_MANIFEST_BYTES)?;
    let sig_bytes = fetcher.get(&sig_asset.url, MAX_SIG_BYTES)?;
    evaluate(cfg, &release, &manifest_bytes, &sig_bytes)
}

/// Cœur de la décision (sans réseau) : signature d'abord, puis manifeste, tag et versions.
pub fn evaluate(
    cfg: &UpdateConfig,
    release: &ReleaseInfo,
    manifest_bytes: &[u8],
    sig_bytes: &[u8],
) -> Result<CheckOutcome, UpdateError> {
    // 1. Signature AVANT tout : aucun champ du manifeste n'est lu avant.
    sig::verify_manifest(&cfg.public_key, manifest_bytes, sig_bytes)?;
    // 2. Contenu.
    let m = Manifest::parse(manifest_bytes)?.validate()?;
    // 3. Cohérence avec la release : tag = v<version>.
    let tag_ver = release.tag.strip_prefix('v').unwrap_or(&release.tag);
    if Version::parse(tag_ver).ok().as_ref() != Some(&m.version) {
        return Err(UpdateError::TagMismatch { tag: release.tag.clone(), version: m.version.to_string() });
    }
    // 4. Versions : strictement supérieure, sinon « à jour » (refus de tout retour en arrière).
    if m.version <= cfg.current {
        return Ok(CheckOutcome::UpToDate { latest: m.version });
    }
    if let Some(min) = &m.min_version {
        if cfg.current < *min {
            return Ok(CheckOutcome::ManualRequired { latest: m.version.clone(), min_version: min.clone() });
        }
    }
    // 5. Le setup annoncé doit être dans la release, avec la même taille si GitHub la donne.
    let setup = release.asset(&m.setup_name).ok_or_else(|| UpdateError::MissingAsset(m.setup_name.clone()))?;
    if let Some(sz) = setup.size {
        if sz != m.size {
            return Err(UpdateError::Integrity(format!("taille annoncée par GitHub {sz} ≠ manifeste {}", m.size)));
        }
    }
    Ok(CheckOutcome::Available(Available { setup_url: setup.url.clone(), manifest: m, current: cfg.current.clone() }))
}

// ---------------------------------------------------------------------------------------------
// Téléchargement
// ---------------------------------------------------------------------------------------------

/// Crée un dossier temporaire neuf pour le téléchargement.
pub fn fresh_download_dir() -> Result<PathBuf, UpdateError> {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut b);
    let dir = std::env::temp_dir().join(format!("grx-update-{}", data_encoding::HEXLOWER.encode(&b)));
    std::fs::create_dir_all(&dir).map_err(|e| UpdateError::Io(e.to_string()))?;
    Ok(dir)
}

/// Télécharge le setup dans `dir` et vérifie taille + SHA-256 (fichier supprimé en cas d'échec).
/// La version a déjà été comparée par [`evaluate`] ; on la revérifie ici (aucun downgrade possible
/// même si l'appelant fabrique un `Available` à la main).
pub fn download(
    avail: &Available,
    fetcher: &dyn Fetcher,
    dir: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<VerifiedSetup, UpdateError> {
    let m = &avail.manifest;
    if m.version <= avail.current {
        return Err(UpdateError::BadManifest("version non supérieure à la version installée".into()));
    }
    if m.size == 0 || m.size > MAX_SETUP_BYTES {
        return Err(UpdateError::BadManifest("taille du setup hors limites".into()));
    }
    let path = dir.join(&m.setup_name);
    let res = (|| {
        let mut file = std::fs::File::create(&path).map_err(|e| UpdateError::Io(e.to_string()))?;
        let mut reader = fetcher.open(&avail.setup_url)?;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf).map_err(|e| UpdateError::Network(e.to_string()))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > m.size {
                return Err(UpdateError::Integrity(format!("fichier plus gros que prévu (> {} octets)", m.size)));
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n]).map_err(|e| UpdateError::Io(e.to_string()))?;
            progress(total, m.size);
        }
        file.sync_all().map_err(|e| UpdateError::Io(e.to_string()))?;
        let hash: [u8; 32] = hasher.finalize().into();
        check_integrity(total, &hash, m.size, &m.sha256)
    })();
    match res {
        Ok(()) => Ok(VerifiedSetup { path, version: m.version.clone(), sha256: m.sha256, size: m.size }),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(e)
        }
    }
}

fn check_integrity(size: u64, hash: &[u8; 32], want_size: u64, want_hash: &[u8; 32]) -> Result<(), UpdateError> {
    if size != want_size {
        return Err(UpdateError::Integrity(format!("taille {size} ≠ {want_size} attendue")));
    }
    // Comparaison sans court-circuit (le hash n'est pas secret, mais ça ne coûte rien).
    let diff = hash.iter().zip(want_hash).fold(0u8, |acc, (a, b)| acc | (a ^ b));
    if diff != 0 {
        return Err(UpdateError::Integrity("SHA-256 différent de celui du manifeste signé".into()));
    }
    Ok(())
}

pub fn hash_file(path: &Path) -> Result<(u64, [u8; 32]), UpdateError> {
    let mut f = std::fs::File::open(path).map_err(|e| UpdateError::Io(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf).map_err(|e| UpdateError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((total, hasher.finalize().into()))
}

#[cfg(test)]
mod tests;
