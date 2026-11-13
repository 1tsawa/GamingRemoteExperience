//! Tests de décodage matériel réels (GPU + extension HEVC requis) sur de petits flux de `testdata/`
//! (générés avec `ffmpeg -f lavfi -i testsrc2=size=WxH:rate=60 -t 1 -c:v hevc_nvenc -g 30 -bf 0 -forced-idr 1`).
//! Un test est ignoré (avec un message) si le décodeur n'est pas disponible sur la machine.
#![cfg(windows)]

use grx_render::*;
use grx_video::Codec;

fn load(name: &str) -> (Codec, Vec<u8>, Vec<std::ops::Range<usize>>) {
    let path = format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let codec = sniff_codec(&data).expect("codec");
    let aus = split_access_units(codec, &data);
    (codec, data, aus)
}

fn decoder(codec: Codec) -> Option<VideoDecoder> {
    match VideoDecoder::new(codec) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("SKIP: no {codec:?} decoder here: {e}");
            None
        }
    }
}

fn one_to_one(name: &str, w: u32, h: u32) {
    let (codec, data, aus) = load(name);
    let Some(mut dec) = decoder(codec) else { return };
    assert_eq!(aus.len(), 60);
    let mut n = 0;
    for (i, r) in aus.iter().enumerate() {
        let f = dec.decode(&data[r.clone()]).unwrap();
        let f = f.unwrap_or_else(|| panic!("{name}: no output for access unit {i} (not low latency?)"));
        assert_eq!((f.width, f.height), (w, h));
        n += 1;
    }
    assert_eq!(n, 60);
    assert!(!dec.needs_keyframe());
}

#[test]
fn hevc_one_in_one_out_720p() {
    one_to_one("1280x720.h265", 1280, 720);
}

#[test]
fn h264_one_in_one_out_720p() {
    one_to_one("1280x720.h264", 1280, 720);
}

#[test]
fn hevc_one_in_one_out_1440p() {
    one_to_one("2560x1440.h265", 2560, 1440);
}

#[test]
fn h264_one_in_one_out_1440p() {
    one_to_one("2560x1440.h264", 2560, 1440);
}

/// Perte de paquets : on saute des images, `reset()`, puis le décodeur n'accepte que depuis le prochain IDR.
fn loss_and_recover(name: &str) {
    let (codec, data, aus) = load(name);
    let Some(mut dec) = decoder(codec) else { return };
    for r in &aus[..10] {
        assert!(dec.decode(&data[r.clone()]).unwrap().is_some());
    }
    // Images 10..20 perdues ; le client détecte la perte et remet le décodeur à zéro.
    dec.reset().unwrap();
    assert!(dec.needs_keyframe());
    let mut first_out = None;
    for (i, r) in aus.iter().enumerate().skip(20) {
        let f = dec.decode(&data[r.clone()]).unwrap();
        if f.is_some() && first_out.is_none() {
            first_out = Some(i);
        }
        if i < 30 {
            assert!(f.is_none(), "P frame {i} must be ignored while waiting for the IDR");
        }
    }
    // GOP de 30 : le prochain IDR est l'image 30.
    assert_eq!(first_out, Some(30));
    assert!(!dec.needs_keyframe());
    assert_eq!(dec.stats().skipped_waiting_idr, 10);
}

#[test]
fn hevc_loss_then_reset_resumes_at_idr() {
    loss_and_recover("1280x720.h265");
}

#[test]
fn h264_loss_then_reset_resumes_at_idr() {
    loss_and_recover("1280x720.h264");
}

/// Changement de résolution en cours de flux (nouvel IDR avec nouveaux jeux de paramètres).
fn resolution_change(a: &str, b: &str) {
    let (codec, da, aa) = load(a);
    let (_, db, ab) = load(b);
    let Some(mut dec) = decoder(codec) else { return };
    let mut sizes = Vec::new();
    for r in &aa {
        sizes.push(dec.decode(&da[r.clone()]).unwrap().map(|f| (f.width, f.height)));
    }
    for r in &ab {
        sizes.push(dec.decode(&db[r.clone()]).unwrap().map(|f| (f.width, f.height)));
    }
    assert!(sizes.iter().all(|s| s.is_some()), "every packet must yield a frame: {sizes:?}");
    assert_eq!(sizes[0], Some((1280, 720)));
    assert_eq!(*sizes.last().unwrap(), Some((2560, 1440)));
}

#[test]
fn hevc_resolution_change() {
    resolution_change("1280x720.h265", "2560x1440.h265");
}

#[test]
fn h264_resolution_change() {
    resolution_change("1280x720.h264", "2560x1440.h264");
}

#[test]
fn flush_is_harmless() {
    let (codec, data, aus) = load("1280x720.h264");
    let Some(mut dec) = decoder(codec) else { return };
    dec.decode(&data[aus[0].clone()]).unwrap();
    assert!(dec.flush().unwrap().is_none());
    // Comme après reset() : un P est ignoré, l'IDR suivant (ici le premier paquet à nouveau) repart.
    assert!(dec.needs_keyframe());
    assert!(dec.decode(&data[aus[1].clone()]).unwrap().is_none());
    assert!(dec.decode(&data[aus[0].clone()]).unwrap().is_some());
}
