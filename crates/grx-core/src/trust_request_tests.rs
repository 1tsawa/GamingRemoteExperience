use super::*;
use crate::identity::Identity;

#[test]
fn requests_open_lifecycle() {
    let mut t = TrustStore::in_memory();
    let t0: u64 = 1_700_000_000_000;

    // Fermé par défaut
    assert!(!t.requests_open(t0));
    assert_eq!(t.requests_open_until(), 0);

    // Ouverture jusqu'à T0 + 1000
    t.set_requests_open_until(t0 + 1000).unwrap();
    assert!(t.requests_open(t0));
    assert!(!t.requests_open(t0 + 1000));

    // REQUESTS_ALWAYS_OPEN
    t.set_requests_open_until(REQUESTS_ALWAYS_OPEN).unwrap();
    assert!(t.requests_open(u64::MAX - 1));

    // Fermeture explicite avec 0
    t.set_requests_open_until(0).unwrap();
    assert!(!t.requests_open(t0));
}

#[test]
fn note_request_and_access_requests() {
    let mut t = TrustStore::in_memory();
    let t0: u64 = 1_700_000_000_000;
    let key = Identity::generate().public();
    let fp = fingerprint(&key);

    t.note_request(&key, "Alice", "192.168.1.1:5000", t0).unwrap();
    let reqs = t.access_requests(t0);
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.name, "Alice");
    assert_eq!(r.addr, "192.168.1.1:5000");
    assert_eq!(r.fingerprint(), fp);

    // Relancer avec la même clé ne duplique pas
    t.note_request(&key, "Alice", "192.168.1.1:5000", t0 + 1).unwrap();
    assert_eq!(t.access_requests(t0 + 1).len(), 1);

    // Avec une autre adresse, met à jour l'adresse
    t.note_request(&key, "Alice", "10.0.0.1:6000", t0 + 2).unwrap();
    assert_eq!(t.access_requests(t0 + 2)[0].addr, "10.0.0.1:6000");
}

#[test]
fn request_expiration() {
    let mut t = TrustStore::in_memory();
    let t0: u64 = 1_700_000_000_000;
    let key = Identity::generate().public();

    t.note_request(&key, "Bob", "1.1.1.1:1234", t0).unwrap();
    assert_eq!(t.access_requests(t0).len(), 1);

    // La demande n'apparaît plus après expiration
    let expired_time = t0 + REQUEST_TTL_MS + 1;
    assert_eq!(t.access_requests(expired_time).len(), 0);
}

#[test]
fn approve_request_lifecycle() {
    let mut t = TrustStore::in_memory();
    let t0: u64 = 1_700_000_000_000;
    let key = Identity::generate().public();
    let fp = fingerprint(&key);

    t.note_request(&key, "Charlie", "2.2.2.2:8080", t0).unwrap();

    // Approbation réussie
    let name = t.approve_request(&fp, Role::ViewOnly, t0).unwrap();
    assert_eq!(name, Some("Charlie".to_string()));
    assert!(t.is_authorized(&key));
    assert_eq!(t.role_of(&key), Some(Role::ViewOnly));
    assert_eq!(t.access_requests(t0).len(), 0); // La demande disparaît

    // Deuxième approbation retourne None
    assert_eq!(t.approve_request(&fp, Role::Full, t0).unwrap(), None);

    // Approbation avec un préfixe retourne None (exige l'empreinte exacte)
    t.note_request(&key, "Charlie", "2.2.2.2:8080", t0).unwrap();
    assert_eq!(t.approve_request(&fp[..9], Role::Full, t0).unwrap(), None);
}

#[test]
fn deny_request_lifecycle() {
    let mut t = TrustStore::in_memory();
    let t0: u64 = 1_700_000_000_000;
    let key = Identity::generate().public();
    let fp = fingerprint(&key);

    t.note_request(&key, "DenyMe", "3.3.3.3:9090", t0).unwrap();
    assert!(t.deny_request(&fp, t0).unwrap());
    assert!(t.is_request_denied(&key, t0));

    // note_request ensuite ne la remet PAS en attente (toujours denied)
    t.note_request(&key, "DenyMe", "3.3.3.3:9090", t0 + 1).unwrap();
    assert!(t.is_request_denied(&key, t0 + 1));
    assert_eq!(t.access_requests(t0 + 1).len(), 1);
    assert!(t.access_requests(t0 + 1)[0].denied);

    // Le refus expire après DENIED_TTL_MS + 1
    let deny_expired = t0 + DENIED_TTL_MS + 1;
    assert!(!t.is_request_denied(&key, deny_expired));

    // forget_request l'efface
    t.note_request(&key, "ForgetMe", "4.4.4.4:7070", t0).unwrap();
    let fp2 = fingerprint(&key);
    assert!(t.forget_request(&fp2).unwrap());
    assert_eq!(t.access_requests(t0).len(), 0);
}

#[test]
fn access_requests_max_bound() {
    let mut t = TrustStore::in_memory();
    let t0: u64 = 1_700_000_000_000;
    let mut keys = Vec::new();

    // Noter MAX_ACCESS_REQUESTS + 5 clés distinctes (now croissant)
    for i in 0..(MAX_ACCESS_REQUESTS + 5) {
        let k = Identity::generate().public();
        keys.push(k);
        t.note_request(&k, &format!("Key{i}"), "1.1.1.1", t0 + i as u64).unwrap();
    }

    let reqs = t.access_requests(t0 + MAX_ACCESS_REQUESTS as u64 + 5);
    assert_eq!(reqs.len(), MAX_ACCESS_REQUESTS);

    // La clé la plus ancienne a disparu, la plus récente est présente
    let oldest_key = &keys[0];
    assert!(reqs.iter().any(|r| r.fingerprint() == fingerprint(&keys[5])));
    let newest_key = &keys[MAX_ACCESS_REQUESTS + 4];
    let oldest_fp = fingerprint(oldest_key);
    let newest_fp = fingerprint(newest_key);

    assert!(!reqs.iter().any(|r| r.fingerprint() == oldest_fp));
    assert!(reqs.iter().any(|r| r.fingerprint() == newest_fp));
}

#[test]
fn persistence_on_disk() {
    let dir = std::env::temp_dir().join(format!("grx-req-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("trust.json");

    // Une demande notée par un store est visible après load par un second
    let mut t1 = TrustStore::load(&p).unwrap();
    let key = Identity::generate().public();
    let fp = fingerprint(&key);
    t1.note_request(&key, "DiskTest", "5.5.5.5:1234", 1_700_000_000_000).unwrap();
    drop(t1);

    let t2 = TrustStore::load(&p).unwrap();
    let reqs = t2.access_requests(1_700_000_000_000);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].fingerprint(), fp);

    // Un trust.json ancien sans les champs requests/requests_open_until_ms se charge correctement
    let old_json = r#"{"clients":[],"invites":[]}"#;
    std::fs::write(&p, old_json).unwrap();
    let t3 = TrustStore::load(&p).unwrap();
    assert_eq!(t3.access_requests(0).len(), 0);
    assert!(!t3.requests_open(0));
    assert_eq!(t3.requests_open_until(), 0);

    // Nettoyage
    let _ = std::fs::remove_dir_all(&dir);
}
