//! Capture N secondes du son systeme (WASAPI loopback), encode Opus, decode, ecrit un .wav 48 kHz stereo,
//! et mesure la latence du pipeline.
//!
//! Usage : cargo run --release --example loopback_test -- [--secs 3] [--out loopback.wav] [--tone] [--frame 10|20]
//!
//! `--tone` joue un bip TRES faible (440 Hz, amplitude 0,02 ~ -34 dBFS, 0,6 s) via `AudioPlayer`
//! (chaine complete encodeur -> jitter buffer -> decodeur -> WASAPI) pour qu'il y ait du signal a capturer
//! et pour mesurer la latence de bout en bout lecture -> loopback.

use grx_audio::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn rms(v: &[f32]) -> f32 {
    (v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32).sqrt()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let secs: f32 = get("--secs").and_then(|s| s.parse().ok()).unwrap_or(3.0);
    let out_path = get("--out").unwrap_or_else(|| "loopback.wav".into());
    let tone = args.iter().any(|a| a == "--tone");
    let frame = if get("--frame").as_deref() == Some("20") { FrameDuration::Ms20 } else { FrameDuration::Ms10 };

    let cap_cfg = CaptureConfig { frame, ..Default::default() };
    let capturer = AudioCapturer::start(cap_cfg).expect("capture");
    let st = capturer.stats();
    println!(
        "capture : '{}' {} Hz {} ca, evenementiel={}, trames {} ms",
        st.device_name, st.source_rate, st.source_channels, st.event_driven, frame.ms()
    );

    let opus_cfg = OpusConfig { frame, ..Default::default() };
    let mut enc = OpusEncoder::new(opus_cfg.clone()).unwrap();
    let mut dec = OpusDecoder::from_config(&opus_cfg).unwrap();
    println!("libopus {} ; {} kb/s {:?} FEC={}", ffi::version(), opus_cfg.bitrate_bps / 1000, opus_cfg.rate_mode, opus_cfg.fec);

    // --- bip faible via le lecteur (optionnel) ---
    let tone_start: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let mut player_stats: Option<PlayerStats> = None;
    let tone_thread = if tone {
        let player = AudioPlayer::start(PlayerConfig { frame, ..Default::default() }).expect("player");
        let ps = player.stats();
        println!("lecture : '{}' {} Hz {} ca, conversion WASAPI={}", ps.device_name, ps.device_rate, ps.device_channels, ps.wasapi_converts);
        let ts = tone_start.clone();
        let h = std::thread::spawn(move || {
            let mut tenc = OpusEncoder::new(OpusConfig { frame, ..Default::default() }).unwrap();
            let fs = tenc.frame_samples();
            let period = Duration::from_millis(frame.ms() as u64);
            std::thread::sleep(Duration::from_millis(300)); // laisse la capture demarrer
            let t0 = Instant::now();
            let nframes = 600 / frame.ms() as usize; // 0,6 s
            for k in 0..nframes {
                let mut pcm = Vec::with_capacity(fs * 2);
                for i in 0..fs {
                    let n = k * fs + i;
                    // enveloppe 20 ms en entree/sortie pour eviter les claquements
                    let env = ((n as f32 / 960.0).min(1.0)).min(((nframes * fs - n) as f32 / 960.0).min(1.0));
                    let s = 0.02 * env * (2.0 * std::f32::consts::PI * 440.0 * n as f32 / 48000.0).sin();
                    pcm.push(s);
                    pcm.push(s);
                }
                if k == 0 {
                    *ts.lock().unwrap() = Some(Instant::now());
                }
                player.push_packet(k as u32, &tenc.encode(&pcm).unwrap());
                let next = t0 + period * (k as u32 + 1);
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
            std::thread::sleep(Duration::from_millis(300));
            Some(player.stats())
        });
        Some(h)
    } else {
        None
    };

    // --- boucle capture -> encode -> decode ---
    let spec = hound::WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut wav = hound::WavWriter::create(&out_path, spec).expect("wav");
    let deadline = Instant::now() + Duration::from_secs_f32(secs);
    let (mut n, mut silent, mut synth, mut bytes) = (0u64, 0u64, 0u64, 0usize);
    let (mut enc_us, mut dec_us, mut lat_us) = (Vec::new(), Vec::new(), Vec::new());
    let mut first_signal: Option<Instant> = None;
    let mut peak = 0f32;
    let mut last_index: Option<u64> = None;
    let mut gaps = 0;
    while Instant::now() < deadline {
        let Some(f) = capturer.recv_timeout(Duration::from_millis(200)) else { continue };
        if let Some(l) = last_index {
            if f.index != l + 1 {
                gaps += 1;
            }
        }
        last_index = Some(f.index);
        n += 1;
        silent += f.silent as u64;
        synth += f.synthesized as u64;
        let t0 = Instant::now();
        let pkt = enc.encode(&f.pcm).unwrap();
        let t1 = Instant::now();
        let pcm = dec.decode(Some(&pkt)).unwrap();
        let t2 = Instant::now();
        bytes += pkt.len();
        if n > 30 {
            enc_us.push((t1 - t0).as_micros() as f64);
            dec_us.push((t2 - t1).as_micros() as f64);
        }
        if n > 30 {
            lat_us.push((t2 - f.captured_at).as_micros() as f64);
        }
        let r = rms(&pcm);
        peak = peak.max(pcm.iter().fold(0f32, |m, x| m.max(x.abs())));
        if first_signal.is_none() && r > 0.003 {
            first_signal = Some(f.captured_at);
        }
        for s in f32_to_i16(&pcm) {
            wav.write_sample(s).unwrap();
        }
    }
    wav.finalize().unwrap();
    if let Some(h) = tone_thread {
        player_stats = h.join().unwrap();
    }

    let stat = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg = v.iter().sum::<f64>() / v.len().max(1) as f64;
        (avg, v.get(v.len() / 2).copied().unwrap_or(0.0), v.last().copied().unwrap_or(0.0))
    };
    let (ea, em, ex) = stat(&mut enc_us);
    let (da, dm, dx) = stat(&mut dec_us);
    let (la, lm, lx) = stat(&mut lat_us);
    let cs = capturer.stats();
    println!("--- resultat ---");
    println!("trames : {n} ({silent} silencieuses dont {synth} synthetisees, {gaps} discontinuites d'index, {} jetees)", cs.dropped_frames);
    println!("debit Opus : {:.1} kb/s ; fichier : {out_path}", bytes as f64 * 8.0 / secs as f64 / 1000.0);
    println!("encodage  : moy {ea:.0} us, med {em:.0}, max {ex:.0}");
    println!("decodage  : moy {da:.0} us, med {dm:.0}, max {dx:.0}");
    println!("latence trame complete -> decodee (file + encodage + decodage) : moy {la:.0} us, med {lm:.0}, max {lx:.0}");
    println!("pic decode : {peak:.4} ({:.1} dBFS)", 20.0 * peak.max(1e-9).log10());
    if let (Some(a), Some(b)) = (*tone_start.lock().unwrap(), first_signal) {
        println!(
            "latence bout en bout (1er paquet du bip pousse -> 1ere trame capturee > -50 dBFS, decodee) : {:.1} ms",
            b.saturating_duration_since(a).as_secs_f64() * 1000.0
        );
    }
    if let Some(p) = player_stats {
        println!(
            "lecteur : decodees {} PLC {} FEC {} silence(device) {} | gigue {:.2} ms, cible {} trames | derive {:+.4} % | late {} lost {} underruns {} rebuffers {}",
            p.decoded_frames, p.plc_frames, p.fec_frames, p.silence_frames, p.jitter.jitter_ms, p.jitter.target_frames,
            p.drift_adjust * 100.0, p.jitter.late, p.jitter.lost, p.jitter.underruns, p.jitter.rebuffers
        );
    }
    capturer.stop();
}
