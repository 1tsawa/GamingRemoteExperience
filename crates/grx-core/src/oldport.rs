//! Migration de l'ancien port par défaut (UDP 7777, versions ≤ 0.2.3) vers UDP 443.
//!
//! Utilisée au chargement des fichiers mémorisés : `hosts.json` (client, voir
//! [`crate::profiles`]), `host-options.json` (host élevé, voir [`crate::elevation`]) et les
//! réglages de grx-gui. Logique pure, sans dépendance à la plateforme.

/// Ancien port par défaut, remplacé automatiquement au chargement des réglages.
pub const OLD_PORT_MIGRATED: u16 = 7777;
/// Port par défaut actuel (UDP 443, celui de HTTP/3).
pub const DEFAULT_PORT: u16 = 443;

/// `hôte:7777` → `hôte:443` (IPv4, nom DNS ou `[IPv6]`), espaces autour ignorés.
/// `None` si l'adresse n'utilise pas l'ancien port (ou n'est pas de la forme `hôte:port`).
pub fn migrate_addr(addr: &str) -> Option<String> {
    let (host, port) = addr.trim().rsplit_once(':')?;
    if port != OLD_PORT_MIGRATED.to_string() || host.is_empty() {
        return None;
    }
    // IPv6 sans crochets (`2a01::7777`) : le dernier groupe n'est pas un port.
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        return None;
    }
    Some(format!("{host}:{DEFAULT_PORT}"))
}

/// Arguments de `grx-host run` (host élevé) : `--bind <ip>:7777` → `--bind <ip>:443`, et
/// `--external-port 7777` retiré (0 = même port que l'écoute). `true` si modifiés.
pub fn migrate_run_args(args: &mut Vec<String>) -> bool {
    let old = OLD_PORT_MIGRATED.to_string();
    let mut out = Vec::with_capacity(args.len());
    let mut changed = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--bind" => {
                out.push(a.clone());
                if let Some(v) = it.next() {
                    match migrate_addr(v) {
                        Some(m) => {
                            out.push(m);
                            changed = true;
                        }
                        None => out.push(v.clone()),
                    }
                }
            }
            "--external-port" => match it.next() {
                Some(v) if v.trim() == old => changed = true,
                Some(v) => {
                    out.push(a.clone());
                    out.push(v.clone());
                }
                None => out.push(a.clone()),
            },
            _ => out.push(a.clone()),
        }
    }
    if changed {
        *args = out;
    }
    changed
}

#[cfg(test)]
#[path = "oldport_tests.rs"]
mod tests;
