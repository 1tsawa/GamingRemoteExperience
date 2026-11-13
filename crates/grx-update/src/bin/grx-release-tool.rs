//! Outil de publication (développeur uniquement, jamais installé chez l'utilisateur).
//!
//! ```text
//! grx-release-tool keygen   --key <signing.key> --pub <release.pub>
//! grx-release-tool pubkey   --key <signing.key>
//! grx-release-tool manifest --version 0.2.1 --setup GRX-Setup-0.2.1.exe [--notes-file notes.txt] [--min-version 0.2.0] --out manifest.json
//! grx-release-tool sign     --key <signing.key> --in manifest.json --out manifest.json.sig [--expect-pub <release.pub>]
//! grx-release-tool verify   --pub <release.pub> --in manifest.json --sig manifest.json.sig
//! grx-release-tool hash     <fichier>
//! ```
//! La clé privée n'est jamais affichée ni journalisée.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use grx_update::manifest::{setup_name_for, Manifest};
use grx_update::{config, sig};
use zeroize::Zeroize;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("erreur : {e}");
            ExitCode::FAILURE
        }
    }
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn req(args: &[String], name: &str) -> Result<String, String> {
    opt(args, name).ok_or_else(|| format!("option {name} manquante"))
}

fn read_key(path: &Path) -> Result<ed25519_dalek::SigningKey, String> {
    let mut text = std::fs::read_to_string(path).map_err(|e| format!("lecture de la clé {} : {e}", path.display()))?;
    let k = sig::parse_signing_key(&text).map_err(|e| e.to_string());
    text.zeroize();
    k
}

fn read_pub(arg: &str) -> Result<ed25519_dalek::VerifyingKey, String> {
    let text = if Path::new(arg).is_file() { std::fs::read_to_string(arg).map_err(|e| e.to_string())? } else { arg.to_string() };
    config::parse_public_key(&text).map_err(|e| e.to_string())
}

fn run(args: &[String]) -> Result<(), String> {
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[1.min(args.len())..];
    match cmd {
        "keygen" => {
            let key = PathBuf::from(req(rest, "--key")?);
            let pub_out = PathBuf::from(req(rest, "--pub")?);
            if key.exists() {
                return Err(format!(
                    "{} existe déjà : refus de l'écraser (une nouvelle clé rendrait les mises à jour impossibles pour les installations existantes)",
                    key.display()
                ));
            }
            if let Some(dir) = key.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            let k = sig::generate_signing_key();
            let mut hex = data_encoding::HEXLOWER.encode(k.as_bytes()) + "\n";
            let res = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&key)
                .and_then(|mut f| std::io::Write::write_all(&mut f, hex.as_bytes()));
            hex.zeroize();
            res.map_err(|e| format!("écriture de {} : {e}", key.display()))?;
            std::fs::write(&pub_out, sig::public_hex(&k) + "\n").map_err(|e| e.to_string())?;
            println!("Clé privée écrite : {}", key.display());
            println!("Clé publique     : {} -> {}", sig::public_hex(&k), pub_out.display());
            Ok(())
        }
        "pubkey" => {
            let k = read_key(Path::new(&req(rest, "--key")?))?;
            println!("{}", sig::public_hex(&k));
            Ok(())
        }
        "manifest" => {
            let version = semver::Version::parse(&req(rest, "--version")?).map_err(|e| format!("version : {e}"))?;
            let setup = PathBuf::from(req(rest, "--setup")?);
            let name = setup.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            if name != setup_name_for(&version) {
                return Err(format!("le setup doit s'appeler {}", setup_name_for(&version)));
            }
            let (size, hash) = grx_update::hash_file(&setup).map_err(|e| e.to_string())?;
            let notes = match opt(rest, "--notes-file") {
                Some(p) => std::fs::read_to_string(&p).map_err(|e| format!("notes : {e}"))?,
                None => String::new(),
            };
            let m = Manifest {
                version: version.to_string(),
                published: now_rfc3339(),
                setup_name: name,
                sha256: data_encoding::HEXLOWER.encode(&hash),
                size,
                notes: notes.trim_start_matches('\u{feff}').trim().to_string(),
                min_version: opt(rest, "--min-version"),
            };
            m.validate().map_err(|e| e.to_string())?;
            let json = serde_json::to_vec_pretty(&m).map_err(|e| e.to_string())?;
            std::fs::write(req(rest, "--out")?, json).map_err(|e| e.to_string())?;
            println!("manifeste : version {} sha256 {} taille {}", m.version, m.sha256, m.size);
            Ok(())
        }
        "sign" => {
            let k = read_key(Path::new(&req(rest, "--key")?))?;
            if let Some(p) = opt(rest, "--expect-pub") {
                if read_pub(&p)? != k.verifying_key() {
                    return Err(format!(
                        "la clé privée ne correspond pas à la clé publique intégrée ({p}) : les clients refuseraient cette release"
                    ));
                }
            }
            let input = std::fs::read(req(rest, "--in")?).map_err(|e| e.to_string())?;
            Manifest::parse(&input).and_then(|m| m.validate()).map_err(|e| e.to_string())?;
            let s = sig::sign_manifest(&k, &input);
            sig::verify_manifest(&k.verifying_key(), &input, s.as_bytes()).map_err(|e| e.to_string())?;
            std::fs::write(req(rest, "--out")?, s).map_err(|e| e.to_string())?;
            println!("signature écrite (clé publique {})", sig::public_hex(&k));
            Ok(())
        }
        "verify" => {
            let key = read_pub(&req(rest, "--pub")?)?;
            let input = std::fs::read(req(rest, "--in")?).map_err(|e| e.to_string())?;
            let s = std::fs::read(req(rest, "--sig")?).map_err(|e| e.to_string())?;
            sig::verify_manifest(&key, &input, &s).map_err(|e| e.to_string())?;
            println!("signature valide");
            Ok(())
        }
        "hash" => {
            let p = rest.first().ok_or("fichier manquant")?;
            let (size, hash) = grx_update::hash_file(Path::new(p)).map_err(|e| e.to_string())?;
            println!("{} {}", data_encoding::HEXLOWER.encode(&hash), size);
            Ok(())
        }
        _ => Err("commande attendue : keygen | pubkey | manifest | sign | verify | hash".into()),
    }
}

/// Date UTC courante au format RFC 3339 (sans dépendance de date).
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Algorithme « civil_from_days » (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}
