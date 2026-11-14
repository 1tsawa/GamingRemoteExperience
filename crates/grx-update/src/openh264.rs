//! Installation du codec OpenH264 de Cisco (encodeur/décodeur H.264 logiciel de secours).
//!
//! Conditions de la licence de brevets MPEG LA accordée par Cisco à son binaire
//! (`third_party/openh264/BINARY_LICENSE.txt`) : le binaire doit être **téléchargé séparément sur
//! le poste de l'utilisateur** depuis Cisco (jamais intégré au setup), l'utilisateur doit pouvoir
//! l'activer/le désactiver, et le texte « OpenH264 Video Codec provided by Cisco Systems, Inc. »
//! doit être affiché là où il le contrôle. D'où : tâche facultative de l'installateur (cochée par
//! défaut) qui lance `grx-host fetch-openh264`, désinstallable, et choix « Décodeur » / `--encoder`.
//!
//! Intégrité : l'archive `.bz2` et la DLL décompressée sont vérifiées par SHA-256 épinglé (le
//! MD5 signé publié par Cisco, `ce1282f5845f56761954282e4992730d`, a été recoupé à l'épinglage ; la
//! DLL porte une signature Authenticode « Cisco WebEx LLC »). Toute autre empreinte : rien n'est écrit.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DLL_NAME: &str = "openh264-2.6.0-win64.dll";
pub const DLL_SHA256: &str = "2076cb5675ec6c1a4c70e7a2a322552f547b6eeed649d6dfcd9e02a543b24691";
pub const BZ2_URL: &str = "https://ciscobinary.openh264.org/openh264-2.6.0-win64.dll.bz2";
pub const BZ2_SHA256: &str = "dab5f2a872777f9a58b69bfa9fbcf20d9f82f2d6ec91383fd70bff49bd34ac9f";
/// Texte imposé par la licence de Cisco.
pub const NOTICE: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";
/// Fichier écrit à côté de la DLL (mention + rappel de la licence).
pub const NOTICE_FILE: &str = "OpenH264-NOTICE.txt";

const MAX_BZ2: u64 = 4 << 20;
const MAX_DLL: u64 = 16 << 20;

fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// Vérifie l'archive, la décompresse, vérifie la DLL et l'écrit dans `dir` (fichier temporaire
/// puis renommage : jamais de DLL partielle). Renvoie le chemin de la DLL.
pub fn install_from_bz2(bz2: &[u8], dir: &Path) -> Result<PathBuf, String> {
    let h = sha256_hex(bz2);
    if h != BZ2_SHA256 {
        return Err(format!("archive OpenH264 : SHA-256 inattendu ({h}), rien n'est installé"));
    }
    let mut dll = Vec::new();
    bzip2::read::BzDecoder::new(bz2)
        .take(MAX_DLL + 1)
        .read_to_end(&mut dll)
        .map_err(|e| format!("décompression bzip2 : {e}"))?;
    if dll.len() as u64 > MAX_DLL {
        return Err("DLL OpenH264 trop grande".into());
    }
    let h = sha256_hex(&dll);
    if h != DLL_SHA256 {
        return Err(format!("DLL OpenH264 : SHA-256 inattendu ({h}), rien n'est installé"));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{} : {e}", dir.display()))?;
    let dst = dir.join(DLL_NAME);
    let tmp = dir.join(format!("{DLL_NAME}.part"));
    std::fs::write(&tmp, &dll).map_err(|e| format!("{} : {e}", tmp.display()))?;
    std::fs::rename(&tmp, &dst).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{} : {e}", dst.display())
    })?;
    let notice = format!(
        "{NOTICE}\r\n\r\nBinaire officiel de Cisco téléchargé depuis {BZ2_URL}\r\nSHA-256 {DLL_SHA256}\r\n\
         Licence : BSD + licence de brevets AVC/H.264 de MPEG LA accordée par Cisco, voir\r\n\
         licenses\\OpenH264-BINARY_LICENSE.txt ou https://www.openh264.org/BINARY_LICENSE.txt\r\n\
         Désactiver : supprimez {DLL_NAME} (ou désinstallez), ou choisissez un décodeur/encodeur matériel.\r\n"
    );
    let _ = std::fs::write(dir.join(NOTICE_FILE), notice);
    Ok(dst)
}

/// Télécharge l'archive officielle depuis Cisco (HTTPS) et l'installe dans `dir`.
pub fn fetch(dir: &Path) -> Result<PathBuf, String> {
    let dst = dir.join(DLL_NAME);
    if std::fs::read(&dst).is_ok_and(|b| sha256_hex(&b) == DLL_SHA256) {
        return Ok(dst);
    }
    let agent = ureq::AgentBuilder::new()
        .user_agent(crate::config::USER_AGENT)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .https_only(true)
        .build();
    let resp = agent.get(BZ2_URL).call().map_err(|e| format!("téléchargement de {BZ2_URL} : {e}"))?;
    let mut bz2 = Vec::new();
    resp.into_reader().take(MAX_BZ2 + 1).read_to_end(&mut bz2).map_err(|e| format!("téléchargement : {e}"))?;
    if bz2.len() as u64 > MAX_BZ2 {
        return Err("archive OpenH264 trop grande".into());
    }
    install_from_bz2(&bz2, dir)
}

/// Supprime la DLL et la mention (désactivation par l'utilisateur).
pub fn remove(dir: &Path) -> std::io::Result<()> {
    for f in [DLL_NAME, NOTICE_FILE] {
        match std::fs::remove_file(dir.join(f)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_archive_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("grx-oh264-fetch-{}", std::process::id()));
        let e = install_from_bz2(b"BZh9 pas l'archive de Cisco", &dir).unwrap_err();
        assert!(e.contains("SHA-256 inattendu"), "{e}");
        assert!(!dir.join(DLL_NAME).exists());
        assert!(remove(&dir).is_ok(), "suppression idempotente");
    }

    #[test]
    fn constants_are_consistent() {
        assert!(BZ2_URL.starts_with("https://ciscobinary.openh264.org/"));
        assert!(BZ2_URL.contains(DLL_NAME));
        assert_eq!(DLL_SHA256.len(), 64);
        assert_eq!(BZ2_SHA256.len(), 64);
    }
}
