use super::*;
use crate::identity::Identity;

#[test]
fn old_file_without_addrs_reads_and_stays_compatible() {
    let dir = std::env::temp_dir().join(format!("grx-prof-addrs-old_compat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let k = crate::util::b32_encode(&Identity::generate().public());
    let json = format!(r#"{{"hosts":[{{"name":"a","addr":"1.2.3.4:9000","host_key":"{}"}}]}}"#, k);
    std::fs::write(dir.join("hosts.json"), json).unwrap();

    let mut s = ProfileStore::load(&dir).unwrap();
    assert_eq!(s.addresses("a"), Some(vec!["1.2.3.4:9000".to_string()]));

    s.set_addresses("a", &["192.168.1.20:443", "1.2.3.4:9000"]).unwrap();
    let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("hosts.json")).unwrap()).unwrap();
    assert_eq!(raw["hosts"][0]["addr"], "192.168.1.20:443");
    let addrs_arr = raw["hosts"][0]["addrs"].as_array().unwrap();
    assert_eq!(addrs_arr.len(), 2);
    assert_eq!(addrs_arr[0], "192.168.1.20:443");
    assert_eq!(addrs_arr[1], "1.2.3.4:9000");

    s.set_addresses("a", &["10.0.0.1:80"]).unwrap();
    let raw2: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("hosts.json")).unwrap()).unwrap();
    assert!(raw2["hosts"][0].get("addrs").is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn old_version_changed_addr_moves_first() {
    let dir = std::env::temp_dir().join(format!("grx-prof-addrs-old_move-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let k = crate::util::b32_encode(&Identity::generate().public());
    let json = format!(r#"{{"hosts":[{{"name":"h","addr":"9.9.9.9:443","addrs":["1.1.1.1:443","2.2.2.2:443"],"host_key":"{}"}}]}}"#, k);
    std::fs::write(dir.join("hosts.json"), json).unwrap();

    let s = ProfileStore::load(&dir).unwrap();
    assert_eq!(s.addresses("h"), Some(vec![
        "9.9.9.9:443".to_string(),
        "1.1.1.1:443".to_string(),
        "2.2.2.2:443".to_string()
    ]));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn validate_addr_cases() {
    assert_eq!(validate_addr(" 203.0.113.5:443 "), Ok("203.0.113.5:443".to_string()));
    assert_eq!(validate_addr("[2a01:e0a::1]:443"), Ok("[2a01:e0a::1]:443".to_string()));
    assert_eq!(validate_addr("monpc.duckdns.org:7777"), Ok("monpc.duckdns.org:7777".to_string()));
    assert_eq!(validate_addr("localhost:1"), Ok("localhost:1".to_string()));

    assert!(validate_addr("").is_err());
    assert!(validate_addr("monpc").is_err());
    assert!(validate_addr("monpc:0").is_err());
    assert!(validate_addr("monpc:70000").is_err());
    assert!(validate_addr("0.0.0.0:443").is_err());
    assert!(validate_addr("[::]:443").is_err());
    assert!(validate_addr("::1:443").is_err());
    assert!(validate_addr("grx://a:1").is_err());
    assert!(validate_addr("a b:1").is_err());

    assert_eq!(validate_addrs(&["a:1", "A:1", "b:2"]), Ok(vec!["a:1".to_string(), "b:2".to_string()]));
    assert!(validate_addrs(&Vec::<String>::new()).is_err());

    let addrs: Vec<String> = (0..9).map(|i| format!("h{i}:1")).collect();
    assert!(validate_addrs(&addrs).is_err());
}

#[test]
fn add_remove_order() {
    let dir = std::env::temp_dir().join(format!("grx-prof-addrs_add_rm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut s = ProfileStore::load(&dir).unwrap();
    let k = Identity::generate().public();
    let inv = Invite { addr: "10.0.0.2:443".into(), host_key: k, token: None };
    s.remember(&inv, Some("pc".into())).unwrap();

    assert_eq!(s.addresses("pc"), Some(vec!["10.0.0.2:443".to_string()]));
    s.add_address("pc", "1.2.3.4:443", false).unwrap();
    assert_eq!(s.addresses("pc"), Some(vec!["10.0.0.2:443".to_string(), "1.2.3.4:443".to_string()]));

    s.add_address("pc", "5.6.7.8:443", true).unwrap();
    assert_eq!(s.addresses("pc"), Some(vec!["5.6.7.8:443".to_string(), "10.0.0.2:443".to_string(), "1.2.3.4:443".to_string()]));

    s.add_address("pc", "1.2.3.4:443", true).unwrap();
    assert_eq!(s.addresses("pc"), Some(vec!["1.2.3.4:443".to_string(), "5.6.7.8:443".to_string(), "10.0.0.2:443".to_string()]));

    s.remove_address("pc", "5.6.7.8:443").unwrap();
    assert_eq!(s.addresses("pc"), Some(vec!["1.2.3.4:443".to_string(), "10.0.0.2:443".to_string()]));

    assert!(s.remove_address("pc", "9.9.9.9:443").is_err());
    s.remove_address("pc", "10.0.0.2:443").unwrap();
    assert!(s.remove_address("pc", "1.2.3.4:443").is_err());

    assert!(!s.add_address("absent", "1.1.1.1:1", false).unwrap());
    assert!(s.add_address("pc", "pas valide", false).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remember_same_key_keeps_other_addrs() {
    let dir = std::env::temp_dir().join(format!("grx-prof-addrs_rem_key-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut s = ProfileStore::load(&dir).unwrap();
    let k = Identity::generate().public();

    let inv_a = Invite { addr: "A:1".into(), host_key: k, token: None };
    s.remember(&inv_a, Some("pc".into())).unwrap();
    s.add_address("pc", "B:2", false).unwrap();

    let inv_c = Invite { addr: "C:3".into(), host_key: k, token: None };
    s.remember(&inv_c, Some("pc".into())).unwrap();
    assert_eq!(s.addresses("pc"), Some(vec!["C:3".to_string(), "A:1".to_string(), "B:2".to_string()]));

    let k2 = Identity::generate().public();
    let inv_d = Invite { addr: "D:4".into(), host_key: k2, token: None };
    s.remember(&inv_d, Some("pc".into())).unwrap();
    assert_eq!(s.addresses("pc"), Some(vec!["D:4".to_string()]));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_renames_and_sets_addrs() {
    let dir = std::env::temp_dir().join(format!("grx-prof-addrs_edit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut s = ProfileStore::load(&dir).unwrap();
    let k = Identity::generate().public();
    let inv = Invite { addr: "x:1".into(), host_key: k, token: None };
    s.remember(&inv, Some("pc".into())).unwrap();

    assert!(s.edit("pc", "salon", &["x:1", "y:2"]).unwrap());
    assert_eq!(s.addresses("salon"), Some(vec!["x:1".to_string(), "y:2".to_string()]));
    assert_eq!(s.addresses("pc"), None);

    let other = Invite { addr: "o:1".into(), host_key: Identity::generate().public(), token: None };
    s.remember(&other, Some("other".into())).unwrap();
    assert!(s.edit("salon", "other", &["z:1"]).is_err(), "nom déjà pris");
    assert!(s.edit("salon", "  ", &["z:1"]).is_err());
    assert!(!s.edit("absent", "z", &["x:1"]).unwrap());

    assert_eq!(s.hosts()[0].host_key, crate::util::b32_encode(&k));

    let (resolved_inv, known) = s.resolve("salon").unwrap();
    assert!(known);
    assert_eq!(resolved_inv.addr, "x:1");

    let _ = std::fs::remove_dir_all(&dir);
}
