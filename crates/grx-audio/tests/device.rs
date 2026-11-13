//! Tests necessitant un peripherique audio Windows reel. Ignores par defaut :
//! `cargo test -p grx-audio --test device --release -- --ignored --test-threads=1`
//! Rien d'audible n'est joue (silence numerique).
#![cfg(windows)]

use grx_audio::*;
use std::time::{Duration, Instant};

#[test]
#[ignore = "necessite un peripherique de rendu"]
fn capturer_delivers_continuous_10ms_frames_even_in_silence() {
    let cap = AudioCapturer::start(CaptureConfig::default()).expect("capture");
    let t0 = Instant::now();
    let mut n = 0u64;
    let mut last = None;
    while t0.elapsed() < Duration::from_secs(2) {
        if let Some(f) = cap.recv_timeout(Duration::from_millis(100)) {
            assert_eq!(f.pcm.len(), 960);
            if let Some(l) = last {
                assert_eq!(f.index, l + 1, "trame manquante");
            }
            last = Some(f.index);
            n += 1;
        }
    }
    // 2 s a 100 trames/s, tolerance pour le demarrage (30 ms de detection du silence)
    assert!((190..=205).contains(&n), "{n} trames en 2 s");
}

#[test]
#[ignore = "necessite un peripherique de rendu"]
fn player_plays_digital_silence_without_underrun() {
    let player = AudioPlayer::start(PlayerConfig::default()).expect("player");
    let mut enc = OpusEncoder::new(OpusConfig::default()).unwrap();
    let silence = vec![0f32; enc.frame_len()];
    let t0 = Instant::now();
    let period = Duration::from_millis(10);
    for seq in 0..150u32 {
        player.push_packet(seq, &enc.encode(&silence).unwrap());
        std::thread::sleep((t0 + period * (seq + 1)).saturating_duration_since(Instant::now()));
    }
    let s = player.stats();
    println!("{s:#?}");
    assert!(s.decoded_frames >= 120, "decodees: {}", s.decoded_frames);
    assert_eq!(s.decode_errors, 0);
    assert!(s.jitter.underruns <= 1, "underruns: {}", s.jitter.underruns);
}
