//! Lit un fichier Annex-B (.h265/.h264), le décode en matériel et l'affiche en boucle.
//!
//! Usage : play_file <fichier> [--fps 60] [--seconds N] [--vsync] [--fullscreen] [--no-gpu-sync]
//!         [--width W --height H]
//!
//! Affiche toutes les 2 s : temps de décodage par image (moyenne / p99 / max) et latence
//! « soumission -> Present » du présentateur. F11 : plein écran. Fermer la fenêtre pour quitter.
use grx_render::*;
use std::time::{Duration, Instant};

struct Stats {
    v: Vec<f64>,
}
impl Stats {
    fn summary(&mut self) -> String {
        if self.v.is_empty() {
            return "n/a".into();
        }
        self.v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = self.v.len();
        format!(
            "avg {:.2} ms / p50 {:.2} / p99 {:.2} / max {:.2} (n={n})",
            self.v.iter().sum::<f64>() / n as f64,
            self.v[n / 2],
            self.v[(n * 99 / 100).min(n - 1)],
            self.v[n - 1]
        )
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    let (mut fps, mut seconds, mut vsync, mut fullscreen, mut gpu_sync) = (60.0f64, 0u64, false, false, true);
    let (mut w, mut h) = (1280u32, 720u32);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fps" => fps = args.next().unwrap().parse()?,
            "--seconds" => seconds = args.next().unwrap().parse()?,
            "--width" => w = args.next().unwrap().parse()?,
            "--height" => h = args.next().unwrap().parse()?,
            "--vsync" => vsync = true,
            "--fullscreen" => fullscreen = true,
            "--no-gpu-sync" => gpu_sync = false,
            _ => path = Some(a),
        }
    }
    let path = path.expect("usage: play_file <file.h265|file.h264> [options]");
    let data = std::fs::read(&path)?;
    let codec = sniff_codec(&data).expect("cannot detect codec");
    let aus = split_access_units(codec, &data);
    println!("{path}: {codec:?}, {} access units, {:.1} MB", aus.len(), data.len() as f64 / 1e6);

    let presenter = Presenter::new(PresenterConfig {
        title: format!("grx-render play_file ({codec:?})"),
        width: w,
        height: h,
        fullscreen,
        vsync,
        ..Default::default()
    })?;
    let handle = presenter.handle();
    let h2 = handle.clone();
    let worker = std::thread::spawn(move || -> anyhow::Result<()> {
        let mut dec = h2.create_decoder(codec)?;
        println!("decoder: {}", dec.backend_name());
        let period = Duration::from_secs_f64(1.0 / fps);
        let (mut call, mut gpu) = (Stats { v: vec![] }, Stats { v: vec![] });
        let (mut all_call, mut all_gpu) = (Stats { v: vec![] }, Stats { v: vec![] });
        let start = Instant::now();
        let mut report = Instant::now();
        let mut n: u64 = 0;
        let mut next = Instant::now();
        'outer: loop {
            for r in &aus {
                if h2.is_closed() || (seconds > 0 && start.elapsed().as_secs() >= seconds) {
                    break 'outer;
                }
                let t0 = Instant::now();
                let f = dec.decode(&data[r.clone()])?;
                let dt_call = t0.elapsed().as_secs_f64() * 1e3;
                if let Some(f) = f {
                    if gpu_sync {
                        dec.sync_gpu()?;
                    }
                    let dt_gpu = t0.elapsed().as_secs_f64() * 1e3;
                    for s in [&mut call, &mut all_call] {
                        s.v.push(dt_call);
                    }
                    for s in [&mut gpu, &mut all_gpu] {
                        s.v.push(dt_gpu);
                    }
                    h2.submit(f);
                    n += 1;
                }
                if report.elapsed() >= Duration::from_secs(2) {
                    report = Instant::now();
                    let pl = h2.take_latency_stats();
                    let st = h2.stats();
                    println!("decode call : {}", call.summary());
                    if gpu_sync {
                        println!("decode+GPU  : {}", gpu.summary());
                    }
                    println!(
                        "present lat : avg {:.2} ms, p99 {:.2}, max {:.2} | presented {} dropped {} | decoder {:?}",
                        pl.avg_us as f64 / 1e3,
                        pl.p99_us as f64 / 1e3,
                        pl.max_us as f64 / 1e3,
                        st.presented,
                        st.dropped,
                        dec.stats()
                    );
                    call.v.clear();
                    gpu.v.clear();
                }
                next += period;
                // Cadence : sommeil grossier puis attente active sur la dernière ms.
                loop {
                    let now = Instant::now();
                    if now >= next {
                        break;
                    }
                    if next - now > Duration::from_millis(2) {
                        std::thread::sleep(Duration::from_millis(1));
                    } else {
                        std::hint::spin_loop();
                    }
                }
            }
            // Fin du fichier : on repart d'un IDR (teste aussi reset()).
            dec.reset()?;
        }
        println!("=== total {n} frames: call {}", all_call.summary());
        if gpu_sync {
            println!("=== total {n} frames: decode+GPU {}", all_gpu.summary());
        }
        h2.close();
        Ok(())
    });

    let mut keys = 0u32;
    presenter.run(move |ev, _win| {
        match ev {
            PresenterEvent::Window(winit::event::WindowEvent::KeyboardInput { event, .. }) => {
                keys += 1;
                if keys <= 3 {
                    println!("raw key event: {:?} {:?}", event.physical_key, event.state);
                }
            }
            PresenterEvent::Window(winit::event::WindowEvent::MouseInput { button, state, .. }) => {
                println!("raw mouse: {button:?} {state:?}");
            }
            _ => {}
        }
        false
    })?;
    handle.close();
    match worker.join() {
        Ok(r) => r?,
        Err(_) => anyhow::bail!("worker panicked"),
    }
    Ok(())
}
