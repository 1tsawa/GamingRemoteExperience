//! Diagnostic manuel de la redirection de port (n'est pas lancé par les tests).
//!
//! ```text
//! cargo run -p grx-portmap --example probe -- info            # passerelle, IP externe (classée)
//! cargo run -p grx-portmap --example probe -- list            # redirections UPnP de la box
//! cargo run -p grx-portmap --example probe -- test 47777 120  # ouvre/vérifie/supprime une redirection « grx-test »
//! cargo run -p grx-portmap --example probe -- rm 47777        # supprime une redirection « grx* » de CETTE machine
//! cargo run -p grx-portmap --example probe -- get 7777      # GetSpecificPortMappingEntry UDP
//! ```
//! `--mask` masque les deux derniers octets de l'IP externe.

use std::net::{IpAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use grx_portmap::{global_ipv6, netif, PortMapper};

fn mask(ip: IpAddr, on: bool) -> String {
    match (ip, on) {
        (IpAddr::V4(v), true) => format!("{}.{}.x.x", v.octets()[0], v.octets()[1]),
        _ => ip.to_string(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "--mask").collect();
    let masked = std::env::args().any(|a| a == "--mask");
    let pm = PortMapper::default();
    match args.first().map(String::as_str).unwrap_or("info") {
        "info" => {
            println!("route par défaut : {:?}", netif::default_route_v4());
            for g in global_ipv6() {
                println!("IPv6 globale : {} (temporaire: {}, {})", g.addr, g.temporary, g.interface);
            }
            let t = Instant::now();
            match pm.discover_upnp() {
                Ok(g) => println!("IGD : {} (local {}) en {:?}", g.addr(), g.local_ip, t.elapsed()),
                Err(e) => println!("IGD : {e} ({:?})", t.elapsed()),
            }
            match pm.external_ip() {
                Ok(e) => println!("IP externe : {} classe {:?} via {}", mask(IpAddr::V4(e.ip), masked), e.class, e.method),
                Err(e) => println!("IP externe : {e}"),
            }
            let mut only = PortMapper { try_upnp: false, ..PortMapper::default() };
            only.discovery_timeout = Duration::from_secs(2);
            match only.external_ip() {
                Ok(e) => println!("NAT-PMP seul : IP externe {} via {}", mask(IpAddr::V4(e.ip), masked), e.method),
                Err(e) => println!("NAT-PMP seul : {e}"),
            }
        }
        "list" => {
            let g = pm.discover_upnp().expect("IGD");
            let all = g.list(256);
            println!("{} redirection(s) sur la box", all.len());
            for e in all {
                println!("  {} {} -> {}:{} « {} » bail {} s actif {}", e.protocol, e.external_port, e.internal_client, e.internal_port, e.description, e.lease_secs, e.enabled);
            }
        }
        "get" => {
            let port: u16 = args[1].parse().unwrap();
            let g = pm.discover_upnp().expect("IGD");
            match g.specific(port) {
                Ok(Some(e)) => println!("UDP {port} : « {} » -> {}:{} bail restant {} s", e.description, e.internal_client, e.internal_port, e.lease_secs),
                Ok(None) => println!("UDP {port} : aucune redirection"),
                Err(e) => println!("erreur : {e}"),
            }
        }
        "rm" => {
            let port: u16 = args[1].parse().unwrap();
            let g = pm.discover_upnp().expect("IGD");
            match g.specific(port) {
                Ok(Some(e)) if e.description.starts_with("grx") && e.internal_client == g.local_ip.to_string() => {
                    println!("suppression : {:?}", g.remove(port))
                }
                Ok(Some(e)) => println!("entrée {port} non créée par grx sur cette machine ({e:?}) : non touchée"),
                Ok(None) => println!("aucune entrée UDP {port}"),
                Err(e) => println!("erreur : {e}"),
            }
        }
        "test" => {
            let port: u16 = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(47777);
            let lease: u64 = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(120);
            // `test <port> <bail> pmp` : PCP/NAT-PMP seuls (sans UPnP).
            let pm = if args.get(3).map(String::as_str) == Some("pmp") { PortMapper { try_upnp: false, ..pm } } else { pm };
            let t = Instant::now();
            let mut m = match pm.open(SocketAddrV4::new([0, 0, 0, 0].into(), port), port, Duration::from_secs(lease), "grx-test") {
                Ok(m) => m,
                Err(e) => {
                    println!("ouverture impossible : {e}");
                    return;
                }
            };
            println!(
                "ouverte en {:?} : {} externe {}:{} -> {} bail accordé {:?} avertissements {:?}",
                t.elapsed(),
                m.method,
                mask(m.external.ip(), masked),
                m.external.port(),
                m.local,
                m.lease,
                m.warnings
            );
            if m.method != grx_portmap::Method::Upnp {
                if let Ok(g) = PortMapper::default().discover_upnp() {
                    println!("(vu par UPnP) : {:?}", g.specific(m.external.port()).map(|e| e.map(|e| (e.description, e.lease_secs))));
                }
            }
            if let Some(g) = m.upnp_gateway() {
                println!("GetSpecificPortMappingEntry : {:?}", g.specific(m.external.port()));
                let found = g.list(256).into_iter().any(|e| e.external_port == m.external.port() && e.description == "grx-test");
                println!("présente dans la liste : {found}");
            }
            std::thread::sleep(Duration::from_secs(3));
            println!("renouvellement : {:?} ; bail {:?}", m.renew(), m.lease);
            if let Some(g) = m.upnp_gateway() {
                println!("après renouvellement : {:?}", g.specific(m.external.port()).map(|e| e.map(|e| e.lease_secs)));
            }
            let g = m.upnp_gateway().cloned();
            let ext = m.external.port();
            println!("fermeture : {:?}", m.close());
            if m.method != grx_portmap::Method::Upnp {
                if let Ok(g) = PortMapper::default().discover_upnp() {
                    println!("(vu par UPnP) après fermeture : {:?}", g.specific(ext).map(|e| e.map(|e| e.description)));
                }
            }
            if let Some(g) = g {
                println!("après fermeture, GetSpecificPortMappingEntry : {:?}", g.specific(ext));
                let found = g.list(256).into_iter().any(|e| e.external_port == ext && e.description == "grx-test");
                println!("toujours dans la liste : {found}");
            }
        }
        other => eprintln!("commande inconnue {other}"),
    }
}
