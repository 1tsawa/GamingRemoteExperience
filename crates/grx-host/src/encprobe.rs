//! Sonde des encodeurs par test réel (voir `grx_video::probe`) : cache disque, sonde de fond au
//! démarrage de `run`, sous-commande `grx-host encoders [--refresh] [--json]`.
//!
//! Cache : `encoder-probe.json` dans le data-dir, une entrée par adaptateur, invalidée si
//! l'adaptateur (vendeur/périphérique/sous-système/révision/nom), le pilote (version UMD), la
//! version de GRX ou la présence d'OpenH264 change. Une entrée dont un échec est « passager »
//! (délai dépassé, encodeur occupé) est re-sondée au démarrage suivant.
//!
//! Tant que la sonde de fond n'a pas fini, `grx_video::encoders_for_adapter` et
//! `Encoder::with_device` gardent l'énumération : la première connexion n'attend pas.

use std::path::{Path, PathBuf};

use grx_video::probe::{self, ProbeResult};
use grx_video::AdapterId;
use serde::{Deserialize, Serialize};

pub const CACHE_FILE: &str = "encoder-probe.json";

/// Clé d'invalidation d'une entrée du cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheKey {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub subsys_id: u32,
    pub revision: u32,
    pub driver_version: u64,
    pub grx_version: String,
    /// Version d'OpenH264 installée (`None` : absente).
    pub openh264: Option<String>,
}

impl CacheKey {
    pub fn new(a: &AdapterId, grx_version: &str, openh264: Option<String>) -> CacheKey {
        CacheKey {
            name: a.name.clone(),
            vendor_id: a.vendor_id,
            device_id: a.device_id,
            subsys_id: a.subsys_id,
            revision: a.revision,
            driver_version: a.driver_version,
            grx_version: grx_version.to_string(),
            openh264,
        }
    }
}

/// Un résultat sérialisé.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedResult {
    pub kind: String,
    pub name: String,
    pub codec: String,
    pub format: String,
    pub ok: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub init_ms: u32,
    #[serde(default)]
    pub frame_ms: u32,
    #[serde(default)]
    pub transient: bool,
}

impl CachedResult {
    pub fn from_result(r: &ProbeResult) -> CachedResult {
        CachedResult {
            kind: probe::kind_id(r.kind).into(),
            name: r.name.clone(),
            codec: probe::codec_id(r.codec).into(),
            format: probe::format_id(r.format).into(),
            ok: r.ok,
            reason: r.reason.clone(),
            init_ms: r.init_ms,
            frame_ms: r.frame_ms,
            transient: r.transient,
        }
    }

    /// `None` si un identifiant est inconnu (cache d'une autre version).
    pub fn to_result(&self) -> Option<ProbeResult> {
        Some(ProbeResult {
            kind: probe::kind_from_id(&self.kind)?,
            name: self.name.clone(),
            codec: probe::codec_from_id(&self.codec)?,
            format: probe::format_from_id(&self.format)?,
            ok: self.ok,
            reason: self.reason.clone(),
            init_ms: self.init_ms,
            frame_ms: self.frame_ms,
            transient: self.transient,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub key: CacheKey,
    pub probed_at_ms: u64,
    pub results: Vec<CachedResult>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeCache {
    #[serde(default)]
    pub entries: Vec<CacheEntry>,
}

impl ProbeCache {
    /// Lit le cache ; fichier absent ou illisible -> cache vide.
    pub fn load(path: &Path) -> ProbeCache {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        grx_core::util::write_atomic(path, &serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// Résultats valides pour `key` : entrée identique, tous les identifiants connus, aucun échec
    /// passager.
    pub fn lookup(&self, key: &CacheKey) -> Option<Vec<ProbeResult>> {
        let e = self.entries.iter().find(|e| &e.key == key)?;
        if e.results.iter().any(|r| r.transient) {
            return None;
        }
        e.results.iter().map(CachedResult::to_result).collect()
    }

    /// Remplace l'entrée du même adaptateur physique (quelle que soit la version du pilote).
    pub fn upsert(&mut self, key: CacheKey, results: &[ProbeResult], now_ms: u64) {
        self.entries.retain(|e| {
            !(e.key.vendor_id == key.vendor_id
                && e.key.device_id == key.device_id
                && e.key.subsys_id == key.subsys_id
                && e.key.revision == key.revision
                && e.key.name == key.name)
        });
        self.entries.push(CacheEntry { key, probed_at_ms: now_ms, results: results.iter().map(CachedResult::from_result).collect() });
    }
}

/// Résultat de la sonde d'un adaptateur.
pub struct AdapterProbe {
    pub adapter: AdapterId,
    pub results: Vec<ProbeResult>,
    pub from_cache: bool,
    pub error: Option<String>,
}

pub fn cache_path(dir: &Path) -> PathBuf {
    dir.join(CACHE_FILE)
}

fn current_key(a: &AdapterId) -> CacheKey {
    CacheKey::new(a, env!("CARGO_PKG_VERSION"), grx_video::openh264::availability().ok())
}

/// Sonde (ou relit du cache) chaque adaptateur matériel, installe les résultats, met le cache à
/// jour.
pub fn ensure(dir: &Path, refresh: bool) -> Vec<AdapterProbe> {
    let path = cache_path(dir);
    let mut cache = ProbeCache::load(&path);
    let mut out = Vec::new();
    let mut dirty = false;
    let adapters = grx_video::hardware_adapters();
    for a in adapters.iter().filter(|a| !adapters.iter().take_while(|b| b.luid != a.luid).any(|b| b.same_hardware(a))) {
        let a = a.clone();
        let twins: Vec<i64> = adapters.iter().filter(|b| b.luid != a.luid && b.same_hardware(&a)).map(|b| b.luid).collect();
        let key = current_key(&a);
        if !refresh {
            if let Some(r) = cache.lookup(&key) {
                for l in std::iter::once(a.luid).chain(twins.iter().copied()) {
                    probe::install(l, r.clone());
                }
                out.push(AdapterProbe { adapter: a, results: r, from_cache: true, error: None });
                continue;
            }
        }
        match grx_video::probe_adapter(a.index) {
            Ok((_, r)) => {
                for l in std::iter::once(a.luid).chain(twins.iter().copied()) {
                    probe::install(l, r.clone());
                }
                cache.upsert(key, &r, grx_core::util::now_ms());
                dirty = true;
                out.push(AdapterProbe { adapter: a, results: r, from_cache: false, error: None });
            }
            Err(e) => out.push(AdapterProbe { adapter: a, results: Vec::new(), from_cache: false, error: Some(e.to_string()) }),
        }
    }
    if dirty {
        if let Err(e) = cache.save(&path) {
            eprintln!("Sonde des encodeurs : cache non enregistré ({e})");
        }
    }
    out
}

/// Sonde de fond au démarrage du host (ne bloque pas les connexions).
pub fn start_background(dir: PathBuf) {
    let _ = std::thread::Builder::new().name("grx-encprobe".into()).spawn(move || {
        let t = std::time::Instant::now();
        for p in ensure(&dir, false) {
            let ok: Vec<String> = p
                .results
                .iter()
                .filter(|r| r.ok)
                .map(|r| format!("{} {} {}", probe::kind_id(r.kind), probe::codec_label(r.codec), r.format.label()))
                .collect();
            match &p.error {
                Some(e) => println!("Sonde des encodeurs ({}) : impossible ({e})", p.adapter.name),
                None => println!(
                    "Sonde des encodeurs ({}{}) : {} combinaison(s) validée(s) sur {} [{}] en {} ms",
                    p.adapter.name,
                    if p.from_cache { ", cache" } else { "" },
                    ok.len(),
                    p.results.len(),
                    ok.join(", "),
                    t.elapsed().as_millis()
                ),
            }
        }
    });
}

/// Une ligne du tableau `grx-host encoders`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub adapter: String,
    pub vendor: String,
    pub kind: String,
    pub name: String,
    pub codec: String,
    pub format: String,
    pub ok: bool,
    pub reason: String,
    pub init_ms: u32,
    pub frame_ms: u32,
}

pub fn rows(probes: &[AdapterProbe]) -> Vec<Row> {
    probes
        .iter()
        .flat_map(|p| {
            p.results.iter().map(move |r| Row {
                adapter: p.adapter.name.clone(),
                vendor: r.vendor().label().into(),
                kind: probe::kind_id(r.kind).into(),
                name: r.name.clone(),
                codec: probe::codec_id(r.codec).into(),
                format: probe::format_id(r.format).into(),
                ok: r.ok,
                reason: r.reason.clone(),
                init_ms: r.init_ms,
                frame_ms: r.frame_ms,
            })
        })
        .collect()
}

/// Tableau texte (largeurs fixes).
pub fn format_table(probes: &[AdapterProbe]) -> String {
    let mut s = String::new();
    for p in probes {
        s.push_str(&format!(
            "Adaptateur {} : {} (pilote {}){}\n",
            p.adapter.index,
            p.adapter.name,
            p.adapter.driver_label(),
            if p.from_cache { " — résultats du cache" } else { " — sondé maintenant" }
        ));
        if let Some(e) = &p.error {
            s.push_str(&format!("  sonde impossible : {e}\n"));
            continue;
        }
        if p.results.is_empty() {
            s.push_str("  aucun encodeur candidat\n");
            continue;
        }
        s.push_str(&format!("  {:<9} {:<10} {:<6} {:<7} {:<6} {:>7} {:>7}  {}\n", "Vendeur", "Voie", "Codec", "Chroma", "Bits", "Init", "Image", "Résultat"));
        for r in &p.results {
            let (chroma, bits) = (if r.format.is_444() { "4:4:4" } else { "4:2:0" }, if r.format.is_10bit() { "10" } else { "8" });
            let verdict = if r.ok { "OK".to_string() } else { format!("échec : {}", r.reason) };
            s.push_str(&format!(
                "  {:<9} {:<10} {:<6} {:<7} {:<6} {:>5}ms {:>5}ms  {}\n",
                r.vendor().label(),
                probe::kind_id(r.kind),
                probe::codec_label(r.codec),
                chroma,
                bits,
                r.init_ms,
                r.frame_ms,
                verdict
            ));
        }
    }
    s
}

/// `grx-host encoders [--refresh] [--json]`.
pub fn cmd_encoders(dir: &Path, refresh: bool, json: bool) -> anyhow::Result<()> {
    let probes = ensure(dir, refresh);
    if json {
        println!("{}", serde_json::to_string(&serde_json::json!({ "encoders": rows(&probes) }))?);
    } else {
        if probes.is_empty() {
            println!("Aucun adaptateur graphique matériel.");
        }
        print!("{}", format_table(&probes));
        println!("Cache : {}", cache_path(dir).display());
    }
    Ok(())
}

#[cfg(test)]
#[path = "encprobe_tests.rs"]
mod tests;
