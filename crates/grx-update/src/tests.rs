use super::*;
use crate::config::UpdateConfig;
use crate::sig;
use crate::manifest::{Manifest, sanitize_notes};
use crate::http::check_url;
use crate::install::{detect, setup_args, is_within, InstallMode};
use ed25519_dalek::SigningKey;
use semver::Version;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn cfg(current: &str, k: &SigningKey) -> UpdateConfig {
    UpdateConfig::build(Some("http://127.0.0.1:9".into()), "o/r", k.verifying_key(), Version::parse(current).unwrap()).unwrap()
}

fn manifest_json(version: &str, setup: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(setup);
    let hash = hasher.finalize();
    let m = Manifest {
        version: version.to_string(),
        published: "2026-11-13T12:00:00Z".to_string(),
        setup_name: format!("GRX-Setup-{}.exe", version),
        sha256: data_encoding::HEXLOWER.encode(&hash),
        size: setup.len() as u64,
        notes: "notes".to_string(),
        min_version: None,
    };
    serde_json::to_vec_pretty(&m).unwrap()
}

struct MockFetcher {
    files: HashMap<String, Vec<u8>>,
    calls: Mutex<u32>,
}

impl MockFetcher {
    fn new(files: HashMap<String, Vec<u8>>) -> Self {
        Self { files, calls: Mutex::new(0) }
    }
    fn calls(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

impl crate::http::Fetcher for MockFetcher {
    fn get(&self, url: &str, max: u64) -> Result<Vec<u8>, UpdateError> {
        let mut c = self.calls.lock().unwrap();
        *c += 1;
        match self.files.get(url) {
            Some(data) if data.len() as u64 > max => Err(UpdateError::TooLarge(max)),
            Some(data) => Ok(data.clone()),
            None => Err(UpdateError::HttpStatus(404)),
        }
    }
    fn open(&self, url: &str) -> Result<Box<dyn std::io::Read + Send>, UpdateError> {
        let mut c = self.calls.lock().unwrap();
        *c += 1;
        match self.files.get(url) {
            Some(data) => Ok(Box::new(std::io::Cursor::new(data.clone()))),
            None => Err(UpdateError::HttpStatus(404)),
        }
    }
}

fn release_json(tag: &str, names_sizes: &[(&str, u64)]) -> Vec<u8> {
    let assets: Vec<serde_json::Value> = names_sizes.iter().map(|(name, size)| {
        serde_json::json!({
            "name": name,
            "size": size,
            "browser_download_url": format!("http://127.0.0.1:9/d/{}", name)
        })
    }).collect();
    serde_json::to_vec_pretty(&serde_json::json!({
        "tag_name": tag,
        "draft": false,
        "prerelease": false,
        "assets": assets
    })).unwrap()
}

fn setup_world(version: &str, signer: &SigningKey, setup: &[u8]) -> MockFetcher {
    let manifest = manifest_json(version, setup);
    let sig = sig::sign_manifest(signer, &manifest);
    let setup_name = format!("GRX-Setup-{}.exe", version);
    let mut files = HashMap::new();
    files.insert(
        "http://127.0.0.1:9/repos/o/r/releases/latest".to_string(),
        release_json(&format!("v{}", version), &[
            ("manifest.json", manifest.len() as u64),
            ("manifest.json.sig", sig.len() as u64),
            (&setup_name, setup.len() as u64),
        ])
    );
    files.insert("http://127.0.0.1:9/d/manifest.json".to_string(), manifest);
    files.insert("http://127.0.0.1:9/d/manifest.json.sig".to_string(), sig.into_bytes());
    files.insert(format!("http://127.0.0.1:9/d/{}", setup_name), setup.to_vec());
    MockFetcher::new(files)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn valid_release_is_available() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let world = setup_world("0.2.1", &k, setup);
    let cfg = cfg("0.2.0", &k);
    let outcome = check(&cfg, &world).unwrap();
    match outcome {
        CheckOutcome::Available(avail) => {
            assert_eq!(avail.manifest.version, Version::parse("0.2.1").unwrap());
            assert!(avail.setup_url.ends_with("GRX-Setup-0.2.1.exe"));
            let dir = std::env::temp_dir().join(format!("grx-update-test-{}-{}", std::process::id(), "valid_release_is_available"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let mut progress = |_, _| {};
            let verified = download(&avail, &world, &dir, &mut progress).unwrap();
            assert!(verified.path.exists());
            verified.reverify().unwrap();
            let _ = std::fs::remove_dir_all(&dir);
        }
        _ => panic!("Expected Available"),
    }
}

#[test]
fn signature_from_other_key_is_rejected() {
    let k1 = key(1);
    let k2 = key(2);
    let setup = b"fake-setup-data";
    let world = setup_world("0.2.1", &k2, setup);
    let cfg = cfg("0.2.0", &k1);
    assert_eq!(check(&cfg, &world), Err(UpdateError::BadSignature));
}

#[test]
fn modified_manifest_is_rejected() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let mut world = setup_world("0.2.1", &k, setup);
    let mut manifest = world.files.get("http://127.0.0.1:9/d/manifest.json").unwrap().clone();
    if let Some(pos) = manifest.iter().position(|&b| b == b'n') {
        manifest[pos] ^= 0x01;
    }
    world.files.insert("http://127.0.0.1:9/d/manifest.json".to_string(), manifest);
    let cfg = cfg("0.2.0", &k);
    assert_eq!(check(&cfg, &world), Err(UpdateError::BadSignature));
}

#[test]
fn garbage_signatures_are_rejected() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let manifest = manifest_json("0.2.1", setup);
    let valid_sig = sig::sign_manifest(&k, &manifest);
    let test_cases: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"zz".to_vec(),
        "g".repeat(128).into_bytes(),
        "0".repeat(127).into_bytes(),
        "0".repeat(130).into_bytes(),
        {
            let mut s = valid_sig.clone().into_bytes();
            s[0] = if s[0] == b'0' { b'1' } else { b'0' };
            s
        },
    ];
    for sig_bytes in test_cases {
        assert_eq!(sig::verify_manifest(&k.verifying_key(), &manifest, &sig_bytes), Err(UpdateError::BadSignature));
    }
}

#[test]
fn wrong_hash_is_rejected_and_file_removed() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let mut world = setup_world("0.2.1", &k, setup);
    let mut served_setup = world.files.get("http://127.0.0.1:9/d/GRX-Setup-0.2.1.exe").unwrap().clone();
    if !served_setup.is_empty() {
        served_setup[0] ^= 0x01;
    }
    world.files.insert("http://127.0.0.1:9/d/GRX-Setup-0.2.1.exe".to_string(), served_setup);
    let cfg = cfg("0.2.0", &k);
    let outcome = check(&cfg, &world).unwrap();
    let dir = std::env::temp_dir().join(format!("grx-update-test-{}-{}", std::process::id(), "wrong_hash_is_rejected_and_file_removed"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    match outcome {
        CheckOutcome::Available(avail) => {
            let mut progress = |_, _| {};
            let res = download(&avail, &world, &dir, &mut progress);
            assert!(matches!(res, Err(UpdateError::Integrity(_))));
            let path = dir.join(&avail.manifest.setup_name);
            assert!(!path.exists());
        }
        _ => panic!("Expected Available"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn oversized_download_is_rejected() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let mut world = setup_world("0.2.1", &k, setup);
    let mut served_setup = setup.to_vec();
    served_setup.push(0x00);
    world.files.insert("http://127.0.0.1:9/d/GRX-Setup-0.2.1.exe".to_string(), served_setup);
    let cfg = cfg("0.2.0", &k);
    let outcome = check(&cfg, &world).unwrap();
    let dir = std::env::temp_dir().join(format!("grx-update-test-{}-{}", std::process::id(), "oversized_download_is_rejected"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    match outcome {
        CheckOutcome::Available(avail) => {
            let mut progress = |_, _| {};
            let res = download(&avail, &world, &dir, &mut progress);
            assert!(matches!(res, Err(UpdateError::Integrity(_))));
            let path = dir.join(&avail.manifest.setup_name);
            assert!(!path.exists());
        }
        _ => panic!("Expected Available"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn downgrade_is_never_offered() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let world = setup_world("0.2.1", &k, setup);
    let cfg = cfg("0.3.0", &k);
    assert_eq!(check(&cfg, &world), Ok(CheckOutcome::UpToDate { latest: Version::parse("0.2.1").unwrap() }));
}

#[test]
fn equal_version_is_up_to_date() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let world = setup_world("0.2.1", &k, setup);
    let cfg = cfg("0.2.1", &k);
    assert_eq!(check(&cfg, &world), Ok(CheckOutcome::UpToDate { latest: Version::parse("0.2.1").unwrap() }));
}

#[test]
fn download_refuses_non_newer_version() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let manifest = manifest_json("0.2.1", setup);
    let m = Manifest::parse(&manifest).unwrap().validate().unwrap();
    let avail = Available {
        manifest: m,
        setup_url: "http://127.0.0.1:9/d/GRX-Setup-0.2.1.exe".to_string(),
        current: Version::parse("0.2.1").unwrap(),
    };
    let world = setup_world("0.2.1", &k, setup);
    let dir = std::env::temp_dir().join(format!("grx-update-test-{}-{}", std::process::id(), "download_refuses_non_newer_version"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut progress = |_, _| {};
    assert!(matches!(download(&avail, &world, &dir, &mut progress), Err(UpdateError::BadManifest(_))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tag_mismatch_is_rejected() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let manifest = manifest_json("0.2.1", setup);
    let sig = sig::sign_manifest(&k, &manifest);
    let mut files = HashMap::new();
    files.insert("http://127.0.0.1:9/repos/o/r/releases/latest".to_string(), release_json("v9.9.9", &[("manifest.json", manifest.len() as u64), ("manifest.json.sig", sig.len() as u64), ("GRX-Setup-0.2.1.exe", setup.len() as u64)]));
    files.insert("http://127.0.0.1:9/d/manifest.json".to_string(), manifest);
    files.insert("http://127.0.0.1:9/d/manifest.json.sig".to_string(), sig.into_bytes());
    files.insert("http://127.0.0.1:9/d/GRX-Setup-0.2.1.exe".to_string(), setup.to_vec());
    let world = MockFetcher::new(files);
    let cfg = cfg("0.2.0", &k);
    let res = check(&cfg, &world);
    assert!(matches!(res, Err(UpdateError::TagMismatch { .. })));
}

#[test]
fn unexpected_github_responses() {
    let cases: Vec<&[u8]> = vec![
        b"pas du json",
        b"[]",
        b"{}",
        b"{\"tag_name\": \"v1.0\"}",
        b"{\"tag_name\": \"v1.0\", \"assets\": \"not_array\"}",
        b"{\"tag_name\": \"v1.0\", \"assets\": [{\"name\": \"a\"}]}",
        b"{\"tag_name\": \"v1.0\", \"assets\": [{\"name\": \"a\", \"browser_download_url\": \"http://x\"}, {\"name\": \"a\", \"browser_download_url\": \"http://y\"}]}",
    ];
    for b in cases {
        assert!(matches!(parse_release(b, true), Err(UpdateError::UnexpectedResponse(_))));
    }
    let insecure = b"{\"tag_name\": \"v1.0\", \"assets\": [{\"name\": \"a\", \"browser_download_url\": \"http://example.com/x\"}]}";
    assert!(matches!(parse_release(insecure, true), Err(UpdateError::InsecureUrl(_))));
}

#[test]
fn missing_signature_asset() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let manifest = manifest_json("0.2.1", setup);
    let mut files = HashMap::new();
    files.insert("http://127.0.0.1:9/repos/o/r/releases/latest".to_string(), release_json("v0.2.1", &[("manifest.json", manifest.len() as u64), ("GRX-Setup-0.2.1.exe", setup.len() as u64)]));
    files.insert("http://127.0.0.1:9/d/manifest.json".to_string(), manifest);
    files.insert("http://127.0.0.1:9/d/GRX-Setup-0.2.1.exe".to_string(), setup.to_vec());
    let world = MockFetcher::new(files);
    let cfg = cfg("0.2.0", &k);
    assert_eq!(check(&cfg, &world), Err(UpdateError::MissingAsset("manifest.json.sig".into())));
}

#[test]
fn min_version_requires_manual_install() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let mut hasher = Sha256::new();
    hasher.update(setup);
    let hash = hasher.finalize();
    let m = serde_json::json!({
        "version": "0.3.0",
        "published": "2026-11-13T12:00:00Z",
        "setup_name": "GRX-Setup-0.3.0.exe",
        "sha256": data_encoding::HEXLOWER.encode(&hash),
        "size": setup.len(),
        "notes": "notes",
        "min_version": "0.2.5"
    });
    let manifest = serde_json::to_vec_pretty(&m).unwrap();
    let sig = sig::sign_manifest(&k, &manifest);
    let mut files = HashMap::new();
    files.insert("http://127.0.0.1:9/repos/o/r/releases/latest".to_string(), release_json("v0.3.0", &[("manifest.json", manifest.len() as u64), ("manifest.json.sig", sig.len() as u64), ("GRX-Setup-0.3.0.exe", setup.len() as u64)]));
    files.insert("http://127.0.0.1:9/d/manifest.json".to_string(), manifest);
    files.insert("http://127.0.0.1:9/d/manifest.json.sig".to_string(), sig.into_bytes());
    files.insert("http://127.0.0.1:9/d/GRX-Setup-0.3.0.exe".to_string(), setup.to_vec());
    let world = MockFetcher::new(files);
    let cfg = cfg("0.2.0", &k);
    assert_eq!(check(&cfg, &world), Ok(CheckOutcome::ManualRequired { latest: Version::parse("0.3.0").unwrap(), min_version: Version::parse("0.2.5").unwrap() }));
}

#[test]
fn draft_release_is_refused() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let manifest = manifest_json("0.2.1", setup);
    let sig = sig::sign_manifest(&k, &manifest);
    let mut files = HashMap::new();
    let mut release_val = serde_json::from_slice::<serde_json::Value>(&release_json("v0.2.1", &[("manifest.json", manifest.len() as u64), ("manifest.json.sig", sig.len() as u64), ("GRX-Setup-0.2.1.exe", setup.len() as u64)])).unwrap();
    release_val["draft"] = serde_json::json!(true);
    files.insert("http://127.0.0.1:9/repos/o/r/releases/latest".to_string(), serde_json::to_vec_pretty(&release_val).unwrap());
    files.insert("http://127.0.0.1:9/d/manifest.json".to_string(), manifest);
    files.insert("http://127.0.0.1:9/d/manifest.json.sig".to_string(), sig.into_bytes());
    files.insert("http://127.0.0.1:9/d/GRX-Setup-0.2.1.exe".to_string(), setup.to_vec());
    let world = MockFetcher::new(files);
    let cfg = cfg("0.2.0", &k);
    assert!(matches!(check(&cfg, &world), Err(UpdateError::UnexpectedResponse(_))));
}

#[test]
fn not_configured_makes_no_request() {
    let k = key(1);
    let v = Version::parse("0.2.0").unwrap();
    let cfg = UpdateConfig::build(None, "CHANGE_ME/GamingRemoteExperience", k.verifying_key(), v).unwrap();
    let world = MockFetcher::new(HashMap::new());
    assert_eq!(check(&cfg, &world), Err(UpdateError::NotConfigured));
    assert_eq!(world.calls(), 0);
}

#[test]
fn too_large_api_response() {
    let k = key(1);
    let setup = b"fake-setup-data";
    let manifest = manifest_json("0.2.1", setup);
    let sig = sig::sign_manifest(&k, &manifest);
    let mut files = HashMap::new();
    let release = release_json("v0.2.1", &[("manifest.json", manifest.len() as u64), ("manifest.json.sig", sig.len() as u64), ("GRX-Setup-0.2.1.exe", setup.len() as u64)]);
    let mut large_release = release;
    large_release.resize(MAX_API_BYTES as usize + 1, b' ');
    files.insert("http://127.0.0.1:9/repos/o/r/releases/latest".to_string(), large_release);
    files.insert("http://127.0.0.1:9/d/manifest.json".to_string(), manifest);
    files.insert("http://127.0.0.1:9/d/manifest.json.sig".to_string(), sig.into_bytes());
    files.insert("http://127.0.0.1:9/d/GRX-Setup-0.2.1.exe".to_string(), setup.to_vec());
    let world = MockFetcher::new(files);
    let cfg = cfg("0.2.0", &k);
    assert_eq!(check(&cfg, &world), Err(UpdateError::TooLarge(MAX_API_BYTES)));
}

#[test]
fn url_policy() {
    assert!(check_url("https://api.github.com", false).is_ok());
    assert!(check_url("http://127.0.0.1:8080/x", true).is_ok());
    assert!(matches!(check_url("http://127.0.0.1:8080/x", false), Err(UpdateError::InsecureUrl(_))));
    assert!(check_url("http://localhost/x", true).is_ok());
    assert!(check_url("http://[::1]:5/x", true).is_ok());
    assert!(matches!(check_url("http://example.com/", true), Err(UpdateError::InsecureUrl(_))));
    assert!(matches!(check_url("http://127.0.0.1.evil.com/", true), Err(UpdateError::InsecureUrl(_))));
    assert!(matches!(check_url("http://user@127.0.0.1/", true), Err(UpdateError::InsecureUrl(_))));
    assert!(matches!(check_url("ftp://x", true), Err(UpdateError::InsecureUrl(_))));
    assert!(matches!(check_url("https://", true), Err(UpdateError::InsecureUrl(_))));
    assert!(matches!(check_url("http://127.0.0.1/x ", true), Err(UpdateError::InsecureUrl(_))));
}

#[test]
fn manifest_validation() {
    let setup = b"fake-setup-data";
    let mut hasher = Sha256::new();
    hasher.update(setup);
    let hash = hasher.finalize();
    let base = serde_json::json!({
        "version": "0.2.1",
        "published": "2026-11-13T12:00:00Z",
        "setup_name": "GRX-Setup-0.2.1.exe",
        "sha256": data_encoding::HEXLOWER.encode(&hash),
        "size": setup.len(),
        "notes": "notes"
    });
    let mut m = base.clone();
    m["setup_name"] = serde_json::json!("Wrong.exe");
    assert!(matches!(Manifest::parse(&serde_json::to_vec(&m).unwrap()).unwrap().validate(), Err(UpdateError::BadManifest(_))));

    m = base.clone();
    m["sha256"] = serde_json::json!("0".repeat(63));
    assert!(matches!(Manifest::parse(&serde_json::to_vec(&m).unwrap()).unwrap().validate(), Err(UpdateError::BadManifest(_))));

    m = base.clone();
    m["sha256"] = serde_json::json!("gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg");
    assert!(matches!(Manifest::parse(&serde_json::to_vec(&m).unwrap()).unwrap().validate(), Err(UpdateError::BadManifest(_))));

    m = base.clone();
    m["size"] = serde_json::json!(0);
    assert!(matches!(Manifest::parse(&serde_json::to_vec(&m).unwrap()).unwrap().validate(), Err(UpdateError::BadManifest(_))));

    m = base.clone();
    m["version"] = serde_json::json!("1.0");
    assert!(matches!(Manifest::parse(&serde_json::to_vec(&m).unwrap()).unwrap().validate(), Err(UpdateError::BadManifest(_))));

    m = base.clone();
    m["version"] = serde_json::json!("1.0.0+build");
    assert!(matches!(Manifest::parse(&serde_json::to_vec(&m).unwrap()).unwrap().validate(), Err(UpdateError::BadManifest(_))));

    m = base.clone();
    m["min_version"] = serde_json::json!("0.3.0");
    assert!(matches!(Manifest::parse(&serde_json::to_vec(&m).unwrap()).unwrap().validate(), Err(UpdateError::BadManifest(_))));

    let valid = Manifest::parse(&serde_json::to_vec(&base).unwrap()).unwrap().validate();
    assert!(valid.is_ok());
}

#[test]
fn notes_are_sanitized() {
    assert_eq!(sanitize_notes("a\u{1b}[31mb\u{202e}c\nd"), "a[31mbc\nd");
    assert!(sanitize_notes(&"x".repeat(crate::manifest::MAX_NOTES_CHARS + 100)).len() <= crate::manifest::MAX_NOTES_CHARS);
}

#[test]
fn install_mode_detection() {
    use std::path::Path;
    let base = std::env::temp_dir().join(format!("grx-test-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let pf = base.join("Program Files");
    let in_pf = pf.join("GamingRemoteExperience");
    let user = base.join("Users").join("x").join("Programs").join("GamingRemoteExperience");
    for d in [&in_pf, &user] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::File::create(d.join("unins000.exe")).unwrap();
    }
    let not_installed = base.join("target").join("release");
    std::fs::create_dir_all(&not_installed).unwrap();

    assert_eq!(detect(&in_pf, &[pf.clone()]), InstallMode::AllUsers);
    assert_eq!(detect(&user, &[pf.clone()]), InstallMode::CurrentUser);
    assert_eq!(detect(&not_installed, &[pf.clone()]), InstallMode::NotInstalled);

    assert!(is_within(Path::new(r"C:\Program Files\App"), Path::new(r"C:\Program Files")));
    assert!(is_within(Path::new("c:/program files/app"), Path::new(r"C:\Program Files\")));
    assert!(is_within(Path::new(r"C:\Program Files"), Path::new(r"C:\Program Files")));
    assert!(!is_within(Path::new(r"C:\Program Files Evil\App"), Path::new(r"C:\Program Files")));
    assert!(!is_within(Path::new(r"C:\Program Files"), Path::new("")));

    let args_cu = setup_args(InstallMode::CurrentUser, true);
    for f in ["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CURRENTUSER", "/RELAUNCH"] {
        assert!(args_cu.iter().any(|a| a == f), "{f}");
    }
    assert!(!args_cu.iter().any(|a| a == "/ALLUSERS"));
    let args_au = setup_args(InstallMode::AllUsers, false);
    assert!(args_au.iter().any(|a| a == "/ALLUSERS"));
    assert!(!args_au.iter().any(|a| a == "/RELAUNCH" || a == "/CURRENTUSER"));
    assert!(setup_args(InstallMode::NotInstalled, true).is_empty());

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn fuzz_parsers_never_panic() {
    let mut seed = 123456789u64;
    let xorshift = |s: &mut u64| {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    };
    let k = key(1);
    let setup = b"fake-setup-data";
    let valid_manifest = manifest_json("0.2.1", setup);
    let valid_sig = sig::sign_manifest(&k, &valid_manifest);
    let valid_release = release_json("v0.2.1", &[("manifest.json", valid_manifest.len() as u64), ("manifest.json.sig", valid_sig.len() as u64), ("GRX-Setup-0.2.1.exe", setup.len() as u64)]);
    let release_parsed = parse_release(&valid_release, true).unwrap();

    for _ in 0..3000 {
        seed = xorshift(&mut seed);
        let len = (seed % 512) as usize;
        let mut buf = vec![0u8; len];
        for b in buf.iter_mut() {
            seed = xorshift(&mut seed);
            *b = (seed & 0xFF) as u8;
        }

        let _ = parse_release(&buf, true);
        let _ = Manifest::parse(&buf).and_then(|m| m.validate());
        let _ = sig::verify_manifest(&k.verifying_key(), &valid_manifest, &buf);
        let _ = evaluate(&cfg("0.2.0", &k), &release_parsed, &valid_manifest, valid_sig.as_bytes());

        let mut m_buf = valid_manifest.clone();
        seed = xorshift(&mut seed);
        let mut_idx = (seed % m_buf.len() as u64) as usize;
        m_buf[mut_idx] ^= 0x01;
        let _ = Manifest::parse(&m_buf).and_then(|m| m.validate());
        let _ = sig::verify_manifest(&k.verifying_key(), &m_buf, valid_sig.as_bytes());
        let res = evaluate(&cfg("0.2.0", &k), &release_parsed, &m_buf, valid_sig.as_bytes());
        assert!(matches!(res, Err(UpdateError::BadSignature)));

        let mut r_buf = valid_release.clone();
        seed = xorshift(&mut seed);
        let mut_idx = (seed % r_buf.len() as u64) as usize;
        r_buf[mut_idx] ^= 0x01;
        let _ = parse_release(&r_buf, true);
        let _ = Manifest::parse(&valid_manifest).and_then(|m| m.validate());
        let _ = sig::verify_manifest(&k.verifying_key(), &valid_manifest, valid_sig.as_bytes());
        if let Ok(release_val) = parse_release(&r_buf, true) {
            let _ = evaluate(&cfg("0.2.0", &k), &release_val, &valid_manifest, valid_sig.as_bytes());
        }

        seed = xorshift(&mut seed);
        let trunc_len = (seed % m_buf.len() as u64) as usize + 1;
        let _ = Manifest::parse(&m_buf[..trunc_len]).and_then(|m| m.validate());
        let _ = sig::verify_manifest(&k.verifying_key(), &m_buf[..trunc_len], valid_sig.as_bytes());
        let _ = evaluate(&cfg("0.2.0", &k), &release_parsed, &m_buf[..trunc_len], valid_sig.as_bytes());
    }
}
