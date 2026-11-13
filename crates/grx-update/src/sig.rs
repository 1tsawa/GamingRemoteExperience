//! Signature Ed25519 du manifeste.
//!
//! Message signé = `CONTEXT || octets exacts de manifest.json` (séparation de domaine : la clé ne
//! peut pas servir à signer autre chose par erreur). `manifest.json.sig` = 64 octets en hexadécimal
//! minuscule (128 caractères), espaces/fin de ligne tolérés autour.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use crate::UpdateError;

pub const CONTEXT: &[u8] = b"GRX-UPDATE-MANIFEST-V1\0";

fn message(manifest: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(CONTEXT.len() + manifest.len());
    m.extend_from_slice(CONTEXT);
    m.extend_from_slice(manifest);
    m
}

/// Signe un manifeste ; renvoie le contenu de `manifest.json.sig`.
pub fn sign_manifest(key: &SigningKey, manifest: &[u8]) -> String {
    let sig = key.sign(&message(manifest));
    data_encoding::HEXLOWER.encode(&sig.to_bytes()) + "\n"
}

/// Vérifie la signature (forme stricte : rejette les signatures malléables et les clés faibles).
pub fn verify_manifest(key: &VerifyingKey, manifest: &[u8], sig_file: &[u8]) -> Result<(), UpdateError> {
    let text = std::str::from_utf8(sig_file).map_err(|_| UpdateError::BadSignature)?.trim();
    if text.len() != 128 {
        return Err(UpdateError::BadSignature);
    }
    let raw = data_encoding::HEXLOWER_PERMISSIVE.decode(text.as_bytes()).map_err(|_| UpdateError::BadSignature)?;
    let arr: [u8; 64] = raw.try_into().map_err(|_| UpdateError::BadSignature)?;
    let sig = Signature::from_bytes(&arr);
    key.verify_strict(&message(manifest), &sig).map_err(|_| UpdateError::BadSignature)
}

/// Lit une clé privée (graine de 32 octets en hexadécimal).
pub fn parse_signing_key(text: &str) -> Result<SigningKey, UpdateError> {
    use zeroize::Zeroize;
    let mut raw = data_encoding::HEXLOWER_PERMISSIVE
        .decode(text.trim().as_bytes())
        .map_err(|_| UpdateError::BadPublicKey("clé privée : hexadécimal invalide".into()))?;
    let res = <[u8; 32]>::try_from(raw.as_slice())
        .map(|mut seed| {
            let k = SigningKey::from_bytes(&seed);
            seed.zeroize();
            k
        })
        .map_err(|_| UpdateError::BadPublicKey("clé privée : 32 octets attendus".into()));
    raw.zeroize();
    res
}

/// Génère une nouvelle clé (OsRng).
pub fn generate_signing_key() -> SigningKey {
    use rand::RngCore;
    use zeroize::Zeroize;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    let k = SigningKey::from_bytes(&seed);
    seed.zeroize();
    k
}

pub fn public_hex(key: &SigningKey) -> String {
    data_encoding::HEXLOWER.encode(key.verifying_key().as_bytes())
}
