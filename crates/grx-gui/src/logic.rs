//! Logique pure de l'interface (aucune E/S, aucune dépendance egui) : validation des liens et
//! adresses, formatage, compte à rebours, état de la liste de clients, construction des lignes
//! de commande. Tout ce module est couvert par des tests unitaires.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Display;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone};
use grx_core::{Error as CoreError, Invite};
use serde::{Deserialize, Serialize};

/// Port UDP par défaut du host (celui de HTTP/3 : souvent ouvert sur les réseaux filtrés).
pub const DEFAULT_PORT: u16 = grx_core::oldport::DEFAULT_PORT;
pub const MAX_NAME_LEN: usize = 40;

// ---------------------------------------------------------------------------------------------
// Durée de validité des invitations
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
pub enum Ttl {
    Min5,
    #[default]
    Hour1,
    Hour24,
}

impl Ttl {
    pub const ALL: [Ttl; 3] = [Ttl::Min5, Ttl::Hour1, Ttl::Hour24];

    pub fn label(self) -> &'static str {
        match self {
            Ttl::Min5 => "5 minutes",
            Ttl::Hour1 => "1 heure",
            Ttl::Hour24 => "24 heures",
        }
    }

    pub fn ms(self) -> u64 {
        match self {
            Ttl::Min5 => 5 * 60_000,
            Ttl::Hour1 => 60 * 60_000,
            Ttl::Hour24 => grx_core::trust::MAX_INVITE_TTL_MS,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Adresses et liens
// ---------------------------------------------------------------------------------------------

/// Remplace le port de `bind` en gardant l'IP (`0.0.0.0` si illisible ; `[::]` reste entre crochets).
pub fn rebind_port(bind: &str, port: u16) -> String {
    let ip = bind.trim().rsplit_once(':').map(|(ip, _)| ip.trim()).filter(|ip| !ip.is_empty()).unwrap_or("0.0.0.0");
    format!("{ip}:{port}")
}

/// Vérifie une adresse `hôte:port` (IPv4, `[IPv6]:port` ou nom DNS) : partagée avec `grx-client`.
pub use grx_core::profiles::validate_addr;

/// Informations affichées à l'utilisateur pour un lien valide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkInfo {
    /// Lien nettoyé (celui qu'il faut transmettre au client).
    pub uri: String,
    pub addr: String,
    pub fingerprint: String,
    pub has_token: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkCheck {
    Empty,
    Invalid(String),
    Valid(LinkInfo),
}

/// Extrait le premier `grx://…` d'un texte collé (espaces, retours à la ligne, guillemets, sortie de `grx-host invite`).
pub fn extract_link(text: &str) -> Option<String> {
    let start = text.find("grx://")?;
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | ')' | ']'))
        .unwrap_or(rest.len());
    let link = rest[..end].trim_end_matches(['.', ',', ';']);
    Some(link.to_string())
}

pub fn validate_link(text: &str) -> LinkCheck {
    if text.trim().is_empty() {
        return LinkCheck::Empty;
    }
    let Some(uri) = extract_link(text) else {
        return LinkCheck::Invalid("Ce texte n'est pas un lien d'invitation : il doit commencer par grx://".into());
    };
    let invite = match Invite::parse(&uri) {
        Ok(i) => i,
        Err(CoreError::BadInvite(m)) => return LinkCheck::Invalid(link_error_fr(&m)),
        Err(e) => return LinkCheck::Invalid(format!("Lien invalide : {e}")),
    };
    if let Err(m) = validate_addr(&invite.addr) {
        return LinkCheck::Invalid(format!("Adresse du lien invalide : {m}"));
    }
    LinkCheck::Valid(LinkInfo {
        fingerprint: grx_core::fingerprint(&invite.host_key),
        has_token: invite.token.is_some(),
        addr: invite.addr.clone(),
        uri,
    })
}

fn link_error_fr(m: &str) -> String {
    match m {
        "adresse manquante" => "Il manque l'adresse du host dans le lien (grx://hôte:port/…).".into(),
        "clé du host invalide" => "La clé du host est absente ou invalide : le lien est probablement tronqué (copie incomplète ?).".into(),
        "jeton invalide" => "Le jeton d'invitation est invalide ou tronqué : redemandez un lien complet au propriétaire du host.".into(),
        "segments en trop" => "Le lien contient des éléments en trop après le jeton.".into(),
        other => format!("Lien invalide : {other}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Noms de hosts
// ---------------------------------------------------------------------------------------------

/// Valide un nom de host mémorisé. `existing` = noms déjà pris (comparaison exacte).
pub fn validate_name(name: &str, existing: &[String]) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("Donnez un nom à ce host.".into());
    }
    if n.chars().count() > MAX_NAME_LEN {
        return Err(format!("Nom trop long ({MAX_NAME_LEN} caractères maximum)."));
    }
    if n.chars().any(char::is_control) {
        return Err("Le nom contient des caractères non autorisés.".into());
    }
    if n.starts_with('-') || n.starts_with("grx://") {
        return Err("Le nom ne peut pas commencer par « - » ni par « grx:// ».".into());
    }
    if existing.iter().any(|e| e == n) {
        return Err(format!("Un host nommé « {n} » existe déjà : choisissez un autre nom."));
    }
    Ok(n.to_string())
}

/// Nom proposé par défaut pour une adresse : l'hôte sans le port, rendu unique par un suffixe.
pub fn suggest_name(addr: &str, existing: &[String]) -> String {
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr).trim_matches(['[', ']']);
    let base = if host.is_empty() { "host".to_string() } else { host.chars().take(MAX_NAME_LEN - 4).collect() };
    if !existing.iter().any(|e| *e == base) {
        return base;
    }
    (2..).map(|i| format!("{base} {i}")).find(|c| !existing.contains(c)).unwrap()
}

// ---------------------------------------------------------------------------------------------
// Temps
// ---------------------------------------------------------------------------------------------

/// Secondes restantes avant `expires_ms` (0 si expirée).
pub fn remaining_secs(expires_ms: u64, now_ms: u64) -> u64 {
    expires_ms.saturating_sub(now_ms).div_ceil(1000)
}

pub fn format_countdown(secs: u64) -> String {
    match secs {
        0 => "expirée".to_string(),
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min {:02} s", s / 60, s % 60),
        s => format!("{} h {:02} min", s / 3600, (s % 3600) / 60),
    }
}

/// `jj/mm/aaaa hh:mm` dans le fuseau donné (`—` si la date est hors plage).
pub fn format_date<Tz: TimeZone>(ms: u64, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    i64::try_from(ms)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .map(|d| d.with_timezone(tz).format("%d/%m/%Y %H:%M").to_string())
        .unwrap_or_else(|| "—".to_string())
}

pub fn format_date_local(ms: u64) -> String {
    format_date(ms, &chrono::Local)
}

// ---------------------------------------------------------------------------------------------
// Liste des clients autorisés
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientRow {
    pub name: String,
    pub fingerprint: String,
    pub added_ms: u64,
    pub role: grx_core::Role,
}

impl ClientRow {
    pub fn role_label(&self) -> &'static str {
        match self.role {
            grx_core::Role::ViewOnly => "Lecture seule",
            grx_core::Role::Full => "Complet",
            grx_core::Role::Total => "Contrôle total",
        }
    }
}

/// Durée pendant laquelle un client nouvellement apparu reste mis en évidence.
pub const NEW_CLIENT_HIGHLIGHT: Duration = Duration::from_secs(12);

/// État de la liste affichée : suit les apparitions pour mettre en évidence un nouveau client
/// (le premier chargement ne marque rien).
#[derive(Default)]
pub struct ClientListState {
    pub rows: Vec<ClientRow>,
    loaded: bool,
    new_since: HashMap<String, Instant>,
}

impl ClientListState {
    /// Remplace la liste ; retourne les clients apparus depuis la mise à jour précédente.
    pub fn update(&mut self, mut rows: Vec<ClientRow>, now: Instant) -> Vec<ClientRow> {
        rows.sort_by(|a, b| b.added_ms.cmp(&a.added_ms).then_with(|| a.fingerprint.cmp(&b.fingerprint)));
        let previous: HashSet<&str> = self.rows.iter().map(|r| r.fingerprint.as_str()).collect();
        let appeared: Vec<ClientRow> = if self.loaded {
            rows.iter().filter(|r| !previous.contains(r.fingerprint.as_str())).cloned().collect()
        } else {
            Vec::new()
        };
        for r in &appeared {
            self.new_since.insert(r.fingerprint.clone(), now);
        }
        let present: HashSet<&str> = rows.iter().map(|r| r.fingerprint.as_str()).collect();
        self.new_since.retain(|fp, _| present.contains(fp.as_str()));
        self.rows = rows;
        self.loaded = true;
        appeared
    }

    pub fn is_new(&self, fingerprint: &str, now: Instant) -> bool {
        self.new_since.get(fingerprint).is_some_and(|t| now.duration_since(*t) < NEW_CLIENT_HIGHLIGHT)
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }
}

// ---------------------------------------------------------------------------------------------
// Journal du host
// ---------------------------------------------------------------------------------------------

/// Tampon circulaire de lignes de sortie (nettoyées des caractères de contrôle).
pub struct LogBuffer {
    cap: usize,
    lines: VecDeque<String>,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        Self { cap: cap.max(1), lines: VecDeque::new() }
    }

    pub fn push(&mut self, line: &str) {
        let clean: String = line.chars().filter(|c| !c.is_control() || *c == '\t').take(400).collect();
        let clean = clean.trim_end().to_string();
        if clean.is_empty() {
            return;
        }
        if self.lines.len() == self.cap {
            self.lines.pop_front();
        }
        self.lines.push_back(clean);
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    pub fn tail(&self, n: usize) -> Vec<String> {
        self.lines.iter().skip(self.lines.len().saturating_sub(n)).cloned().collect()
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }
}

// Options de lancement : voir `launch_opts.rs` (réexportées ici).
pub use crate::launch_opts::{ClientPlayOptions, CodecChoice, HostRunOptions};

/// Faut-il remplacer l'adresse publique saisie par une adresse détectée automatiquement ?
/// Oui si le champ est vide ou s'il contient déjà une adresse automatique ; jamais une saisie manuelle.
pub fn should_autofill(current: &str, current_is_auto: bool, candidate: &str) -> bool {
    (current.trim().is_empty() || current_is_auto) && current.trim() != candidate.trim()
}

/// Texte venant d'un tiers (nom annoncé par un client) rendu sûr pour l'affichage :
/// caractères de contrôle supprimés, longueur bornée.
pub fn sanitize_display(s: &str, max_chars: usize) -> String {
    let clean: String = s.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    if clean.chars().count() > max_chars {
        let cut: String = clean.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        clean.to_string()
    }
}

/// Dernières lignes d'un journal, condensées pour un message d'erreur.
pub fn summarize_tail(lines: &[String], n: usize) -> String {
    lines.iter().skip(lines.len().saturating_sub(n)).cloned().collect::<Vec<_>>().join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Utc};
    use grx_core::{Identity, Token};

    fn sample_invite(addr: &str, token: bool) -> Invite {
        Invite { addr: addr.into(), host_key: Identity::generate().public(), token: token.then(Token::random) }
    }

    #[test]
    fn addr_accepts_valid_and_rejects_invalid() {
        for ok in ["203.0.113.5:7777", " monpc.duckdns.org:7777 ", "[::1]:7777", "localhost:1", "a_b-c.d:65535"] {
            assert!(validate_addr(ok).is_ok(), "{ok}");
        }
        assert_eq!(validate_addr(" h.example:80 ").unwrap(), "h.example:80");
        for bad in [
            "", "   ", "monpc", "monpc:", ":7777", "monpc:0", "monpc:70000", "monpc:abc", "grx://a:1", "a:1/b",
            "a b:1", "0.0.0.0:7777", "[::]:7777", "::1:7777", "[zz]:7777", "ho$t:7777", "a..b:7777",
        ] {
            assert!(validate_addr(bad).is_err(), "{bad:?} doit être refusé");
        }
    }

    #[test]
    fn link_valid_shows_addr_and_fingerprint() {
        let inv = sample_invite("203.0.113.5:7777", true);
        match validate_link(&inv.to_uri()) {
            LinkCheck::Valid(i) => {
                assert_eq!(i.addr, "203.0.113.5:7777");
                assert_eq!(i.fingerprint, grx_core::fingerprint(&inv.host_key));
                assert!(i.has_token);
                assert_eq!(i.uri, inv.to_uri());
            }
            other => panic!("{other:?}"),
        }
        match validate_link(&sample_invite("h:1", false).to_uri()) {
            LinkCheck::Valid(i) => assert!(!i.has_token),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn link_is_extracted_from_noisy_paste() {
        let uri = sample_invite("h.example:7777", true).to_uri();
        for text in [
            format!("  {uri}\r\n"),
            format!("\"{uri}\""),
            format!("<{uri}>"),
            format!("Invitation (valable 10 min, usage unique) :\n\n  {uri}\n\nEmpreinte à vérifier : AAAA"),
            format!("Voici le lien : {uri}."),
        ] {
            match validate_link(&text) {
                LinkCheck::Valid(i) => assert_eq!(i.uri, uri, "{text:?}"),
                other => panic!("{text:?} -> {other:?}"),
            }
        }
    }

    #[test]
    fn link_errors_are_specific_and_in_french() {
        assert_eq!(validate_link(""), LinkCheck::Empty);
        assert_eq!(validate_link("  \n "), LinkCheck::Empty);
        let msg = |t: &str| match validate_link(t) {
            LinkCheck::Invalid(m) => m,
            other => panic!("{t:?} -> {other:?}"),
        };
        assert!(msg("http://example.com").contains("grx://"));
        assert!(msg("grx://").contains("adresse"));
        assert!(msg("grx://h:1/abc").contains("clé du host"));
        let uri = sample_invite("h:1", true).to_uri();
        let truncated = &uri[..uri.len() - 3]; // jeton tronqué -> longueur incorrecte
        assert!(msg(truncated).contains("jeton"));
        assert!(msg(&format!("{uri}/x")).contains("en trop"));
        let bad_addr = sample_invite("0.0.0.0:7777", true).to_uri();
        assert!(msg(&bad_addr).contains("Adresse du lien invalide"));
    }

    #[test]
    fn names_are_validated() {
        let existing = vec!["salon".to_string()];
        assert_eq!(validate_name("  pc bureau ", &existing).unwrap(), "pc bureau");
        for bad in ["", "   ", "salon", "-x", "grx://a", &"x".repeat(41), "a\nb"] {
            assert!(validate_name(bad, &existing).is_err(), "{bad:?}");
        }
        assert!(validate_name(&"é".repeat(40), &existing).is_ok());
    }

    #[test]
    fn suggested_name_is_unique() {
        let mut ex = vec![];
        assert_eq!(suggest_name("monpc.duckdns.org:7777", &ex), "monpc.duckdns.org");
        ex.push("monpc.duckdns.org".to_string());
        assert_eq!(suggest_name("monpc.duckdns.org:7777", &ex), "monpc.duckdns.org 2");
        ex.push("monpc.duckdns.org 2".to_string());
        assert_eq!(suggest_name("monpc.duckdns.org:7777", &ex), "monpc.duckdns.org 3");
        assert_eq!(suggest_name("[::1]:7777", &[]), "::1");
        assert_eq!(suggest_name(":7777", &[]), "host");
    }

    #[test]
    fn countdown_and_formatting() {
        assert_eq!(remaining_secs(10_000, 4_001), 6);
        assert_eq!(remaining_secs(10_000, 10_000), 0);
        assert_eq!(remaining_secs(10_000, 99_999), 0);
        assert_eq!(format_countdown(0), "expirée");
        assert_eq!(format_countdown(7), "7 s");
        assert_eq!(format_countdown(59), "59 s");
        assert_eq!(format_countdown(60), "1 min 00 s");
        assert_eq!(format_countdown(252), "4 min 12 s");
        assert_eq!(format_countdown(3600), "1 h 00 min");
        assert_eq!(format_countdown(86_400), "24 h 00 min");
        assert_eq!(Ttl::Min5.ms(), 300_000);
        assert_eq!(Ttl::Hour1.ms(), 3_600_000);
        assert_eq!(Ttl::Hour24.ms(), grx_core::trust::MAX_INVITE_TTL_MS);
    }

    #[test]
    fn dates_are_formatted_in_the_given_zone() {
        // 2024-01-02 03:04:05 UTC
        let ms = 1_704_164_645_000;
        assert_eq!(format_date(ms, &Utc), "02/01/2024 03:04");
        assert_eq!(format_date(ms, &FixedOffset::east_opt(3600).unwrap()), "02/01/2024 04:04");
        assert_eq!(format_date(u64::MAX, &Utc), "—");
    }

    #[test]
    fn rebind_port_keeps_ip() {
        assert_eq!(rebind_port("0.0.0.0:7777", 443), "0.0.0.0:443");
        assert_eq!(rebind_port("[::]:7777", 443), "[::]:443");
        assert_eq!(rebind_port("192.168.1.20:9000", 7777), "192.168.1.20:7777");
        assert_eq!(rebind_port("", 443), "0.0.0.0:443");
        assert_eq!(rebind_port("n'importe quoi", 443), "0.0.0.0:443");
    }

    fn row(name: &str, fp: &str, added: u64, vo: bool) -> ClientRow {
        let role = if vo { grx_core::Role::ViewOnly } else { grx_core::Role::Full };
        ClientRow { name: name.into(), fingerprint: fp.into(), added_ms: added, role }
    }

    #[test]
    fn client_list_sorts_flags_new_and_forgets_removed() {
        let t0 = Instant::now();
        let mut st = ClientListState::default();
        assert!(!st.is_loaded());
        // Premier chargement : rien n'est « nouveau ».
        let appeared = st.update(vec![row("a", "AAAA", 10, false), row("b", "BBBB", 20, true)], t0);
        assert!(appeared.is_empty() && st.is_loaded());
        assert_eq!(st.rows[0].name, "b", "le plus récent d'abord");
        assert!(!st.is_new("AAAA", t0));
        // Un client apparaît tout seul.
        let appeared = st.update(
            vec![row("a", "AAAA", 10, false), row("b", "BBBB", 20, true), row("c", "CCCC", 30, false)],
            t0,
        );
        assert_eq!(appeared.len(), 1);
        assert_eq!(appeared[0].name, "c");
        assert!(st.is_new("CCCC", t0 + Duration::from_secs(5)));
        assert!(!st.is_new("CCCC", t0 + NEW_CLIENT_HIGHLIGHT + Duration::from_secs(1)));
        // Changement de rôle : personne de nouveau.
        let appeared = st.update(
            vec![row("a", "AAAA", 10, true), row("b", "BBBB", 20, true), row("c", "CCCC", 30, false)],
            t0,
        );
        assert!(appeared.is_empty());
        // Révocation : la ligne et son marquage disparaissent.
        st.update(vec![row("a", "AAAA", 10, true)], t0);
        assert_eq!(st.rows.len(), 1);
        assert!(!st.is_new("CCCC", t0));
        // Ordre stable à date égale.
        st.update(vec![row("z", "ZZZZ", 5, false), row("y", "YYYY", 5, false)], t0);
        assert_eq!(st.rows[0].fingerprint, "YYYY");
    }

    #[test]
    fn role_labels() {
        assert_eq!(row("a", "A", 0, true).role_label(), "Lecture seule");
        assert_eq!(row("a", "A", 0, false).role_label(), "Complet");
    }

    #[test]
    fn log_buffer_is_bounded_and_clean() {
        let mut l = LogBuffer::new(3);
        for i in 0..5 {
            l.push(&format!("ligne {i}\r"));
        }
        l.push("   ");
        l.push("\u{1b}[31mrouge\u{1b}[0m");
        assert_eq!(l.snapshot().len(), 3);
        assert_eq!(l.snapshot()[0], "ligne 3");
        assert!(l.snapshot().iter().all(|s| !s.contains('\u{1b}')));
        assert_eq!(l.tail(1), vec!["[31mrouge[0m".to_string()]);
        l.push(&"x".repeat(1000));
        assert_eq!(l.tail(1)[0].len(), 400);
        l.clear();
        assert!(l.snapshot().is_empty());
    }

    #[test]
    fn display_text_from_remote_is_sanitized() {
        assert_eq!(sanitize_display("  pc\u{7}\n\u{1b}[0m salon ", 50), "pc[0m salon");
        assert_eq!(sanitize_display(&"é".repeat(100), 10), format!("{}…", "é".repeat(9)));
        assert_eq!(sanitize_display("court", 10), "court");
    }

    #[test]
    fn autofill_never_overwrites_manual_input() {
        assert!(should_autofill("", false, "198.51.100.7:7777"));
        assert!(should_autofill("  ", false, "198.51.100.7:7777"));
        assert!(should_autofill("198.51.100.7:7777", true, "198.51.100.8:7778"), "ancienne adresse auto");
        assert!(!should_autofill("monpc.duckdns.org:7777", false, "198.51.100.7:7777"), "saisie manuelle");
        assert!(!should_autofill("198.51.100.7:7777", true, "198.51.100.7:7777"), "inchangée");
    }

    #[test]
    fn tail_summary() {
        let l: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(summarize_tail(&l, 2), "b | c");
        assert_eq!(summarize_tail(&l, 10), "a | b | c");
        assert_eq!(summarize_tail(&[], 2), "");
    }
}
