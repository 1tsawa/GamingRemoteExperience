//! Décode un fichier Annex-B sans fenêtre et mesure le temps par image.
//! Usage : decode_bench <fichier.h265|.h264> [boucles]
use grx_render::*;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("fichier .h265/.h264");
    let loops: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let data = std::fs::read(&path)?;
    let codec = sniff_codec(&data).expect("codec inconnu");
    let aus = split_access_units(codec, &data);
    println!("{path}: {codec:?}, {} access units", aus.len());
    let mut dec = VideoDecoder::new(codec)?;
    println!("decoder: {}", dec.backend_name());
    let mut times = Vec::new();
    let mut none_count = 0;
    for l in 0..loops {
        if l > 0 {
            dec.reset()?;
        }
        for (i, r) in aus.iter().enumerate() {
            let t0 = Instant::now();
            let f = dec.decode(&data[r.clone()])?;
            let dt = t0.elapsed();
            match f {
                Some(f) => {
                    if l == 0 && i < 3 {
                        println!("frame {i}: {}x{} sub={} pts={} dt={:?}", f.width, f.height, f.subresource, f.pts, dt);
                        let mut d = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
                        unsafe { f.texture.GetDesc(&mut d) };
                        println!(
                            "  tex {}x{} fmt={:?} array={} bind={:#x} usage={:?}",
                            d.Width, d.Height, d.Format, d.ArraySize, d.BindFlags, d.Usage
                        );
                    }
                    times.push(dt.as_secs_f64() * 1000.0);
                }
                None => {
                    none_count += 1;
                    if l == 0 && i < 5 {
                        println!("frame {i}: None dt={dt:?}");
                    }
                }
            }
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = times.len();
    let avg = times.iter().sum::<f64>() / n as f64;
    println!(
        "decoded {n} frames ({none_count} without output): avg {avg:.2} ms, p50 {:.2}, p99 {:.2}, max {:.2}",
        times[n / 2],
        times[(n * 99 / 100).min(n - 1)],
        times[n - 1]
    );
    println!("{:?}", dec.stats());
    Ok(())
}
