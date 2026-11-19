use super::*;

fn v(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn migrate_addr_old_port_to_443() {
    assert_eq!(migrate_addr("1.2.3.4:7777").as_deref(), Some("1.2.3.4:443"));
    assert_eq!(migrate_addr(" monpc.duckdns.org:7777 ").as_deref(), Some("monpc.duckdns.org:443"));
    assert_eq!(migrate_addr("[2a01:e0a::1]:7777").as_deref(), Some("[2a01:e0a::1]:443"));
    assert_eq!(migrate_addr("0.0.0.0:7777").as_deref(), Some("0.0.0.0:443"));
    assert_eq!(migrate_addr("[::]:7777").as_deref(), Some("[::]:443"));
}

#[test]
fn migrate_addr_leaves_other_addresses() {
    for a in ["1.2.3.4:443", "1.2.3.4:77777", "1.2.3.4:17777", ":7777", "7777", "2a01::7777", "a:7778", ""] {
        assert_eq!(migrate_addr(a), None, "{a}");
    }
}

#[test]
fn migrate_run_args_bind_and_external_port() {
    let mut a = v(&["--bind", "0.0.0.0:7777", "--fps", "60"]);
    assert!(migrate_run_args(&mut a));
    assert_eq!(a, v(&["--bind", "0.0.0.0:443", "--fps", "60"]));

    let mut a = v(&["--bind", "[::]:7777"]);
    assert!(migrate_run_args(&mut a));
    assert_eq!(a, v(&["--bind", "[::]:443"]));

    let mut a = v(&["--no-upnp", "--external-port", "7777", "--fill"]);
    assert!(migrate_run_args(&mut a));
    assert_eq!(a, v(&["--no-upnp", "--fill"]));

    let mut a = v(&["--bind", "192.168.1.20:7777", "--external-port", "7777", "--no-audio"]);
    assert!(migrate_run_args(&mut a));
    assert_eq!(a, v(&["--bind", "192.168.1.20:443", "--no-audio"]));
}

#[test]
fn migrate_run_args_leaves_the_rest() {
    for orig in [
        v(&["--external-port", "7790"]),
        v(&["--bind", "0.0.0.0:443", "--fps", "7777"]),
        v(&[]),
        v(&["--bind"]),
        v(&["--external-port"]),
    ] {
        let mut a = orig.clone();
        assert!(!migrate_run_args(&mut a), "{orig:?}");
        assert_eq!(a, orig);
    }
}
