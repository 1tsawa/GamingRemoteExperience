use super::*;
use crate::identity::Identity;

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("grx-trust-role-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn role_parse_and_as_str() {
    let inputs = [
        "view_only", "view-only", "VIEW_ONLY", "viewonly", "spectateur",
        "full", "complet", "total", "controle-total",
    ];
    for input in inputs {
        let with_spaces = format!("  {input}  ");
        assert_eq!(
            Role::parse(&with_spaces),
            Role::parse(input),
            "parse failed for: {input}"
        );
    }

    assert!(Role::parse("admin").is_none());
    assert!(Role::parse("").is_none());

    assert_eq!(Role::ViewOnly.as_str(), "view_only");
    assert_eq!(Role::Full.as_str(), "full");
    assert_eq!(Role::Total.as_str(), "total");

    for r in [Role::ViewOnly, Role::Full, Role::Total] {
        assert_eq!(Role::parse(r.as_str()), Some(r));
    }

    assert_eq!(Role::default(), Role::Full);

    assert!(!Role::ViewOnly.can_inject());
    assert!(Role::Full.can_inject());
    assert!(Role::Total.can_inject());
}

#[test]
fn new_client_defaults_to_full() {
    let dir = tmp_dir("new_client");
    let p = dir.join("trust.json");

    let mut store = TrustStore::load(&p).expect("load absent file");
    let k = Identity::generate().public();
    store.authorize(&k, "pc", 1).expect("authorize");

    assert_eq!(store.role_of(&k), Some(Role::Full));
    assert!(!store.is_view_only(&k));

    let unknown = Identity::generate().public();
    assert_eq!(store.role_of(&unknown), None);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn set_role_total_roundtrip_and_file_format() {
    let dir = tmp_dir("roundtrip");
    let p = dir.join("trust.json");
    let mut store = TrustStore::load(&p).expect("load");
    let k = Identity::generate().public();
    store.authorize(&k, "pc", 1).expect("authorize");

    let fp = fingerprint(&k);
    assert_eq!(store.set_role(&fp[..9], Role::Total).expect("set_role total"), 1);

    let mut store2 = TrustStore::load(&p).expect("reload");
    assert_eq!(store2.role_of(&k), Some(Role::Total));

    let json_str = std::fs::read_to_string(&p).expect("read json");
    let json: serde_json::Value = serde_json::from_str(&json_str).expect("parse json");
    let client = json["clients"][0].as_object().expect("client object");
    assert_eq!(client["role"], "total");
    assert_eq!(client["view_only"], false);

    store2.set_role(&fp[..9], Role::ViewOnly).expect("set_role viewonly");
    let json_str2 = std::fs::read_to_string(&p).expect("read json");
    let json2: serde_json::Value = serde_json::from_str(&json_str2).expect("parse json");
    let client2 = json2["clients"][0].as_object().expect("client object");
    assert_eq!(client2["role"], "view_only");
    assert_eq!(client2["view_only"], true);

    store2.set_view_only(&fp[..9], false).expect("set_view_only false");
    let store3 = TrustStore::load(&p).expect("reload");
    assert_eq!(store3.role_of(&k), Some(Role::Full));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn old_format_without_role_field() {
    let dir = tmp_dir("old_format");
    let p = dir.join("trust.json");
    let k1 = Identity::generate().public();
    let k2 = Identity::generate().public();

    let json = format!(
        r#"{{"clients":[{{"key":"{}","name":"a","added_ms":1,"view_only":true}},{{"key":"{}","name":"b","added_ms":2}}],"invites":[]}}"#,
        b32_encode(&k1),
        b32_encode(&k2)
    );
    std::fs::write(&p, json).expect("write json");

    let store = TrustStore::load(&p).expect("load");
    assert_eq!(store.role_of(&k1), Some(Role::ViewOnly));
    assert_eq!(store.role_of(&k2), Some(Role::Full));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn view_only_true_wins_over_role() {
    let dir = tmp_dir("wins_over");
    let p = dir.join("trust.json");
    let k = Identity::generate().public();

    let json = format!(
        r#"{{"clients":[{{"key":"{}","name":"x","added_ms":1,"view_only":true,"role":"total"}}],"invites":[]}}"#,
        b32_encode(&k)
    );
    std::fs::write(&p, json).expect("write json");

    let store = TrustStore::load(&p).expect("load");
    assert_eq!(store.role_of(&k), Some(Role::ViewOnly));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_role_is_least_privilege_and_file_still_loads() {
    let dir = tmp_dir("unknown_role");
    let p = dir.join("trust.json");
    let k1 = Identity::generate().public();
    let k2 = Identity::generate().public();

    let json = format!(
        r#"{{"clients":[{{"key":"{}","name":"a","added_ms":1,"role":"superadmin"}},{{"key":"{}","name":"b","added_ms":2,"role":"full"}}],"invites":[]}}"#,
        b32_encode(&k1),
        b32_encode(&k2)
    );
    std::fs::write(&p, json).expect("write json");

    let store = TrustStore::load(&p).expect("load should succeed");
    assert_eq!(store.role_of(&k1), Some(Role::ViewOnly));
    assert_eq!(store.role_of(&k2), Some(Role::Full));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn set_role_with_short_prefix_is_refused() {
    let dir = tmp_dir("short_prefix");
    let p = dir.join("trust.json");
    let mut store = TrustStore::load(&p).expect("load");
    let k = Identity::generate().public();
    store.authorize(&k, "pc", 1).expect("authorize");

    assert!(store.set_role("AB", Role::Total).is_err());
    assert!(store.set_role("", Role::Total).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn old_reader_sees_safe_role() {
    #[derive(serde::Deserialize)]
    struct OldClient {
        #[allow(dead_code)]
        key: String,
        view_only: bool,
    }
    #[derive(serde::Deserialize)]
    struct OldData {
        clients: Vec<OldClient>,
    }

    let dir = tmp_dir("old_reader");
    let p = dir.join("trust.json");
    let mut store = TrustStore::load(&p).expect("load");
    let k = Identity::generate().public();
    store.authorize(&k, "pc", 1).expect("authorize");

    let fp = fingerprint(&k);
    store.set_role(&fp[..9], Role::Total).expect("set total");
    let json_str = std::fs::read_to_string(&p).expect("read");
    let old_data: OldData = serde_json::from_str(&json_str).expect("parse old");
    assert!(!old_data.clients[0].view_only);

    store.set_role(&fp[..9], Role::ViewOnly).expect("set viewonly");
    let json_str2 = std::fs::read_to_string(&p).expect("read");
    let old_data2: OldData = serde_json::from_str(&json_str2).expect("parse old");
    assert!(old_data2.clients[0].view_only);

    let _ = std::fs::remove_dir_all(&dir);
}
