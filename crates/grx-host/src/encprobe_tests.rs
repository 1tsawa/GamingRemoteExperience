use super::*;
use grx_video::{Codec, EncoderKind, PixelFormat, Vendor};

fn adapter(driver: u64) -> AdapterId {
    AdapterId {
        index: 0,
        luid: 42,
        name: "GPU test".into(),
        vendor_id: 0x10DE,
        device_id: 0x2782,
        subsys_id: 1,
        revision: 2,
        driver_version: driver,
    }
}

fn res(kind: EncoderKind, codec: Codec, format: PixelFormat, ok: bool, transient: bool) -> ProbeResult {
    ProbeResult {
        kind,
        name: "enc".into(),
        codec,
        format,
        ok,
        reason: if ok { String::new() } else { "refus".into() },
        init_ms: 12,
        frame_ms: 3,
        transient,
    }
}

#[test]
fn test_cached_result_roundtrip() {
    let r = res(EncoderKind::Mf(Vendor::Nvidia), Codec::Hevc, PixelFormat::Yuv420, true, false);
    let cached = CachedResult::from_result(&r);
    let back = cached.to_result().expect("roundtrip should succeed");
    assert_eq!(r, back);
}

#[test]
fn test_cached_result_unknown_id_returns_none() {
    let cached = CachedResult {
        kind: "inconnu".into(),
        name: "enc".into(),
        codec: "hevc".into(),
        format: "420".into(),
        ok: true,
        reason: String::new(),
        init_ms: 0,
        frame_ms: 0,
        transient: false,
    };
    assert!(cached.to_result().is_none());
}

#[test]
fn test_cache_lookup_variations() {
    let a = adapter(100);
    let key = CacheKey::new(&a, "0.2.2", None);
    let results = vec![res(EncoderKind::Mf(Vendor::Nvidia), Codec::Hevc, PixelFormat::Yuv420, true, false)];
    let mut cache = ProbeCache::default();
    cache.entries.push(CacheEntry {
        key: key.clone(),
        probed_at_ms: 1000,
        results: vec![CachedResult::from_result(&results[0])],
    });

    assert!(cache.lookup(&key).is_some());

    let mut key2 = key.clone();
    key2.driver_version = 200;
    assert!(cache.lookup(&key2).is_none());

    let mut key3 = key.clone();
    key3.grx_version = "0.3.0".into();
    assert!(cache.lookup(&key3).is_none());

    let mut key4 = key.clone();
    key4.openh264 = Some("2.0.0".into());
    assert!(cache.lookup(&key4).is_none());

    let mut key5 = key.clone();
    key5.device_id = 0x1234;
    assert!(cache.lookup(&key5).is_none());
}

#[test]
fn test_cache_lookup_transient_returns_none() {
    let a = adapter(100);
    let key = CacheKey::new(&a, "0.2.2", None);
    let transient_res = res(EncoderKind::Mf(Vendor::Nvidia), Codec::Hevc, PixelFormat::Yuv420, false, true);
    let mut cache = ProbeCache::default();
    cache.entries.push(CacheEntry {
        key,
        probed_at_ms: 1000,
        results: vec![CachedResult::from_result(&transient_res)],
    });
    assert!(cache.lookup(&CacheKey::new(&a, "0.2.2", None)).is_none());
}

#[test]
fn test_cache_upsert_replaces_same_adapter() {
    let a1 = adapter(100);
    let a2 = adapter(200);
    let key1 = CacheKey::new(&a1, "0.2.2", None);
    let key2 = CacheKey::new(&a2, "0.2.2", None);
    let res1 = vec![res(EncoderKind::Mf(Vendor::Nvidia), Codec::Hevc, PixelFormat::Yuv420, true, false)];
    let res2 = vec![res(EncoderKind::Nvenc, Codec::Av1, PixelFormat::Yuv444, true, false)];

    let mut cache = ProbeCache::default();
    cache.upsert(key1.clone(), &res1, 1000);
    assert_eq!(cache.entries.len(), 1);

    cache.upsert(key2.clone(), &res2, 2000);
    assert_eq!(cache.entries.len(), 1);
    assert_eq!(cache.entries[0].key, key2);
}

#[test]
fn test_cache_upsert_keeps_different_adapter() {
    let a1 = adapter(100);
    let mut a2 = adapter(100);
    a2.device_id = 0x9999;
    let key1 = CacheKey::new(&a1, "0.2.2", None);
    let key2 = CacheKey::new(&a2, "0.2.2", None);
    let res1 = vec![res(EncoderKind::Mf(Vendor::Nvidia), Codec::Hevc, PixelFormat::Yuv420, true, false)];
    let res2 = vec![res(EncoderKind::Nvenc, Codec::Av1, PixelFormat::Yuv444, true, false)];

    let mut cache = ProbeCache::default();
    cache.upsert(key1.clone(), &res1, 1000);
    cache.upsert(key2.clone(), &res2, 2000);
    assert_eq!(cache.entries.len(), 2);
}

#[test]
fn test_cache_save_load_roundtrip() {
    let dir = std::env::temp_dir().join(format!("grx-encprobe-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("encoder-probe.json");

    let a = adapter(100);
    let key = CacheKey::new(&a, "0.2.2", None);
    let res = vec![res(EncoderKind::Mf(Vendor::Nvidia), Codec::Hevc, PixelFormat::Yuv420, true, false)];
    let mut cache = ProbeCache::default();
    cache.upsert(key, &res, 1000);

    cache.save(&path).unwrap();
    let loaded = ProbeCache::load(&path);
    assert_eq!(cache, loaded);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_cache_load_absent_or_invalid() {
    let dir = std::env::temp_dir().join(format!("grx-encprobe-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("encoder-probe.json");

    assert_eq!(ProbeCache::load(&path), ProbeCache::default());

    std::fs::write(&path, "pas du json").unwrap();
    assert_eq!(ProbeCache::load(&path), ProbeCache::default());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_rows_adapter_probe() {
    let a = adapter(100);
    let r1 = res(EncoderKind::Mf(Vendor::Nvidia), Codec::Hevc, PixelFormat::Yuv420, true, false);
    let r2 = res(EncoderKind::Nvenc, Codec::Av1, PixelFormat::Yuv444, false, false);
    let probe = AdapterProbe {
        adapter: a,
        results: vec![r1, r2],
        from_cache: true,
        error: None,
    };
    let rows = rows(&[probe]);
    assert_eq!(rows.len(), 2);

    assert_eq!(rows[0].vendor, "NVIDIA");
    assert_eq!(rows[0].kind, "mf-nvidia");
    assert_eq!(rows[0].codec, "hevc");
    assert_eq!(rows[0].format, "420");
    assert!(rows[0].ok);
    assert_eq!(rows[0].reason, "");

    assert_eq!(rows[1].vendor, "NVIDIA");
    assert_eq!(rows[1].kind, "nvenc");
    assert_eq!(rows[1].codec, "av1");
    assert_eq!(rows[1].format, "444");
    assert!(!rows[1].ok);
    assert_eq!(rows[1].reason, "refus");
}

#[test]
fn test_format_table_content() {
    let a = adapter(100);
    let r1 = res(EncoderKind::Mf(Vendor::Nvidia), Codec::Hevc, PixelFormat::Yuv420, true, false);
    let r2 = res(EncoderKind::Nvenc, Codec::Av1, PixelFormat::Yuv444, false, false);
    let probe = AdapterProbe {
        adapter: a.clone(),
        results: vec![r1, r2],
        from_cache: true,
        error: None,
    };
    let table = format_table(&[probe]);
    assert!(table.contains("OK"));
    assert!(table.contains("échec : refus"));
    assert!(table.contains("HEVC"));
    assert!(table.contains("4:4:4"));
    assert!(table.contains("cache"));

    let probe_err = AdapterProbe {
        adapter: a,
        results: vec![],
        from_cache: false,
        error: Some("x".into()),
    };
    let table_err = format_table(&[probe_err]);
    assert!(table_err.contains("sonde impossible : x"));
}
