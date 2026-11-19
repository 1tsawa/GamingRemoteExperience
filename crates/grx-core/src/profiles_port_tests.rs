//! Migration unique de l'ancien port par défaut (7777 → 443) dans `hosts.json`.

use super::*;
use crate::identity::Identity;

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("grx-prof-port-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn raw(dir: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(dir.join("hosts.json")).unwrap()).unwrap()
}

#[test]
fn old_port_migrated_once_and_written() {
    let dir = tmp_dir("once");
    let k = crate::util::b32_encode(&Identity::generate().public());
    let json = format!(
        r#"{{"hosts":[
            {{"name":"a","addr":"1.2.3.4:7777","host_key":"{k}"}},
            {{"name":"b","addr":"monpc.duckdns.org:7777","addrs":["monpc.duckdns.org:7777","[2a01:e0a::1]:7777","192.168.1.20:443","monpc.duckdns.org:443","10.0.0.1:9000"],"host_key":"{k}"}}
        ]}}"#
    );
    std::fs::write(dir.join("hosts.json"), json).unwrap();

    let s = ProfileStore::load(&dir).unwrap();
    assert_eq!(s.addresses("a"), Some(vec!["1.2.3.4:443".to_string()]));
    // Ordre conservé, doublon (monpc.duckdns.org:443) retiré, autres ports intacts.
    let b = ["monpc.duckdns.org:443", "[2a01:e0a::1]:443", "192.168.1.20:443", "10.0.0.1:9000"];
    assert_eq!(s.addresses("b"), Some(b.iter().map(|x| x.to_string()).collect()));
    // Écrit sur disque avec le marqueur ; `addr` = première adresse.
    let r = raw(&dir);
    assert_eq!(r["migrated_443"], true);
    assert_eq!(r["hosts"][0]["addr"], "1.2.3.4:443");
    assert_eq!(r["hosts"][1]["addr"], "monpc.duckdns.org:443");
    assert_eq!(r["hosts"][1]["addrs"].as_array().unwrap().len(), 4);
    assert_eq!(r["hosts"][0]["name"], "a", "les noms ne changent pas");

    // Une seule fois : une adresse en 7777 ajoutée ensuite à la main est gardée.
    let mut s = ProfileStore::load(&dir).unwrap();
    s.add_address("a", "5.6.7.8:7777", false).unwrap();
    let s = ProfileStore::load(&dir).unwrap();
    assert_eq!(s.addresses("a"), Some(vec!["1.2.3.4:443".to_string(), "5.6.7.8:7777".to_string()]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn single_address_collapses_after_dedup() {
    let dir = tmp_dir("dedup");
    let k = crate::util::b32_encode(&Identity::generate().public());
    let json = format!(r#"{{"hosts":[{{"name":"h","addr":"pc.lan:7777","addrs":["pc.lan:7777","pc.lan:443"],"host_key":"{k}"}}]}}"#);
    std::fs::write(dir.join("hosts.json"), json).unwrap();
    let s = ProfileStore::load(&dir).unwrap();
    assert_eq!(s.addresses("h"), Some(vec!["pc.lan:443".to_string()]));
    assert!(raw(&dir)["hosts"][0].get("addrs").is_none(), "une seule adresse : `addrs` n'est plus écrit");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_file_no_write_and_new_file_is_marked() {
    let dir = tmp_dir("new");
    let mut s = ProfileStore::load(&dir).unwrap();
    assert!(!dir.join("hosts.json").exists(), "pas de fichier créé au chargement");
    let inv = Invite { addr: "1.2.3.4:7777".into(), host_key: Identity::generate().public(), token: None };
    s.remember(&inv, Some("pc".into())).unwrap();
    assert_eq!(raw(&dir)["migrated_443"], true);
    // Lien d'invitation en 7777 : mémorisé tel quel, pas réécrit au rechargement.
    assert_eq!(ProfileStore::load(&dir).unwrap().addresses("pc"), Some(vec!["1.2.3.4:7777".to_string()]));
    let _ = std::fs::remove_dir_all(&dir);
}
