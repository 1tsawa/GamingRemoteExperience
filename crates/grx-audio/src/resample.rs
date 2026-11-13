//! Reechantillonneur en flux, interpolation cubique (Hermite 4 points), rapport variable.
//!
//! Sert a deux choses cote client :
//! * convertir 48 kHz -> frequence du peripherique quand WASAPI ne le fait pas (`ratio = 48000/dev_rate`) ;
//! * corriger la derive d'horloge (le rapport est module de quelques centiemes de % autour de la valeur nominale).
//!
//! Le pas d'interpolation est constant par appel a `produce` ; la phase (`pos`) est conservee entre
//! les appels, donc pas de discontinuite quand le rapport change doucement.
//! Qualite : correcte pour de petits rapports (~1) ; pour 48 -> 44,1 kHz une interpolation cubique
//! sans filtre anti-repliement attenue legerement les aigus >18 kHz et laisse un peu d'aliasing.
//! (en pratique on laisse WASAPI convertir via `AUTOCONVERTPCM`, ce chemin n'est qu'un repli).

pub struct StreamResampler {
    ch: usize,
    /// Trames entrelacees ; la trame 0 est le point y[-1] (zero au depart).
    buf: Vec<f32>,
    /// Position fractionnaire de la prochaine sortie, en trames dans `buf`.
    pos: f64,
}

#[inline]
fn hermite(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let c0 = y1;
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + c0
}

impl StreamResampler {
    pub fn new(channels: usize) -> Self {
        Self { ch: channels, buf: vec![0.0; channels], pos: 1.0 }
    }

    /// Ajoute des trames entrelacees en entree.
    pub fn push(&mut self, input: &[f32]) {
        debug_assert_eq!(input.len() % self.ch, 0);
        self.buf.extend_from_slice(input);
    }

    fn frames(&self) -> usize {
        self.buf.len() / self.ch
    }

    /// Trames d'entree encore non consommees (approximation de la latence du resampler).
    pub fn buffered_frames(&self) -> f64 {
        (self.frames() as f64 - self.pos).max(0.0)
    }

    /// Combien de trames d'entree faut-il encore pousser pour pouvoir produire `n_out` sorties.
    pub fn input_needed(&self, ratio: f64, n_out: usize) -> usize {
        let last = self.pos + ratio * (n_out.saturating_sub(1)) as f64;
        let need_frames = last.floor() as usize + 3;
        need_frames.saturating_sub(self.frames())
    }

    /// Produit jusqu'a `max_out` trames (ajoutees a `out`, entrelacees) ; retourne le nombre produit.
    /// `ratio` = trames d'entree consommees par trame de sortie.
    pub fn produce(&mut self, ratio: f64, max_out: usize, out: &mut Vec<f32>) -> usize {
        let ch = self.ch;
        let frames = self.frames();
        let mut produced = 0;
        while produced < max_out {
            let i = self.pos.floor() as usize;
            if i + 2 >= frames {
                break;
            }
            let t = (self.pos - i as f64) as f32;
            for c in 0..ch {
                let at = |k: usize| self.buf[k * ch + c];
                out.push(hermite(at(i - 1), at(i), at(i + 1), at(i + 2), t));
            }
            self.pos += ratio;
            produced += 1;
        }
        // compacte : garde la trame i-1 et suivantes
        let drop = (self.pos.floor() as usize).saturating_sub(1).min(frames.saturating_sub(1));
        if drop > 0 {
            self.buf.drain(..drop * ch);
            self.pos -= drop as f64;
        }
        produced
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.buf.resize(self.ch, 0.0);
        self.pos = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / rate).sin() as f32 * 0.5).collect()
    }

    #[test]
    fn identity_at_ratio_one() {
        let mut r = StreamResampler::new(1);
        let inp = sine(1000.0, 48000.0, 960);
        let mut out = Vec::new();
        r.push(&inp[..480]);
        r.produce(1.0, 10_000, &mut out);
        r.push(&inp[480..]);
        r.produce(1.0, 10_000, &mut out);
        assert!(out.len() >= 950);
        for (a, b) in out.iter().zip(&inp) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn downsample_48k_to_44k1_keeps_tone() {
        let mut r = StreamResampler::new(1);
        let inp = sine(1000.0, 48000.0, 48000);
        let mut out = Vec::new();
        for chunk in inp.chunks(480) {
            r.push(chunk);
            r.produce(48000.0 / 44100.0, 10_000, &mut out);
        }
        assert!((out.len() as i64 - 44100).abs() < 5, "len {}", out.len());
        // compare avec le sinus ideal a 44.1k
        let ideal = sine(1000.0, 44100.0, out.len());
        let err: f64 = out.iter().zip(&ideal).skip(100).map(|(a, b)| ((a - b) as f64).powi(2)).sum::<f64>();
        let sig: f64 = ideal.iter().skip(100).map(|b| (*b as f64).powi(2)).sum::<f64>();
        let snr = 10.0 * (sig / err).log10();
        assert!(snr > 50.0, "snr {snr}");
    }

    #[test]
    fn slight_ratio_change_is_continuous() {
        let mut r = StreamResampler::new(2);
        let mono = sine(440.0, 48000.0, 48000);
        let inp: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
        let mut out = Vec::new();
        for (k, chunk) in inp.chunks(960).enumerate() {
            r.push(chunk);
            let ratio = if k % 2 == 0 { 1.003 } else { 0.997 };
            r.produce(ratio, 10_000, &mut out);
        }
        // pas de saut : difference entre echantillons successifs bornee
        let max_step = out.chunks(2).collect::<Vec<_>>().windows(2).map(|w| (w[1][0] - w[0][0]).abs()).fold(0f32, f32::max);
        assert!(max_step < 0.1, "saut {max_step}");
    }

    #[test]
    fn input_needed_is_consistent() {
        let mut r = StreamResampler::new(2);
        let need = r.input_needed(1.0, 480);
        r.push(&vec![0.1; need * 2]);
        let mut out = Vec::new();
        assert_eq!(r.produce(1.0, 480, &mut out), 480);
    }
}
