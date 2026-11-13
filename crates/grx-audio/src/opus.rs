//! Encodeur / decodeur Opus 48 kHz (sur libopus, voir `ffi.rs` et `build.rs`).

use crate::{AudioError, Result, SAMPLE_RATE};
use crate::ffi;

/// Duree d'une trame Opus. 10 ms = latence minimale (defaut), 20 ms = meilleur rendement a debit egal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameDuration {
    Ms10,
    Ms20,
}

impl FrameDuration {
    pub fn ms(self) -> u32 {
        match self {
            FrameDuration::Ms10 => 10,
            FrameDuration::Ms20 => 20,
        }
    }
    /// Echantillons par canal pour une trame.
    pub fn samples(self) -> usize {
        (SAMPLE_RATE / 1000 * self.ms()) as usize
    }
    fn opus_const(self) -> i32 {
        match self {
            FrameDuration::Ms10 => ffi::OPUS_FRAMESIZE_10_MS,
            FrameDuration::Ms20 => ffi::OPUS_FRAMESIZE_20_MS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateMode {
    /// Debit constant (taille de paquet fixe).
    Cbr,
    /// Debit variable libre.
    Vbr,
    /// VBR contraint : variable mais borne (bon compromis reseau).
    ConstrainedVbr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Application {
    /// Musique / jeu, qualite maximale (CELT, SILK/hybride a bas debit). Defaut.
    Audio,
    /// Voix (favorise SILK a bas debit ; FEC interne efficace).
    Voip,
    /// CELT seul, latence algorithmique minimale (pas de FEC).
    LowDelay,
}

#[derive(Clone, Debug)]
pub struct OpusConfig {
    pub frame: FrameDuration,
    /// 1 ou 2 (defaut 2).
    pub channels: usize,
    /// Debit cible en bit/s (defaut 128 000 pour du stereo jeu/musique).
    pub bitrate_bps: i32,
    pub rate_mode: RateMode,
    /// 0..=10 (defaut 5 : CPU negligeable).
    pub complexity: i32,
    /// FEC "in-band" (LBRR). Efficace uniquement quand Opus choisit SILK/hybride (bas debit,
    /// typiquement <= ~48 kb/s stereo ou voix) ; sans effet en mode CELT pur.
    pub fec: bool,
    /// Perte attendue en % (0..=100) : regle l'agressivite du FEC.
    pub expected_loss_percent: i32,
    pub application: Application,
    /// Transmission discontinue (silence -> paquets quasi vides). Desactive par defaut.
    pub dtx: bool,
}

impl Default for OpusConfig {
    fn default() -> Self {
        Self {
            frame: FrameDuration::Ms10,
            channels: 2,
            bitrate_bps: 128_000,
            rate_mode: RateMode::ConstrainedVbr,
            complexity: 5,
            fec: true,
            expected_loss_percent: 2,
            application: Application::Audio,
            dtx: false,
        }
    }
}

fn check(code: i32, what: &str) -> Result<()> {
    if code < 0 {
        Err(AudioError::Opus(format!("{what}: code {code}")))
    } else {
        Ok(())
    }
}

fn enc_ctl(st: *mut ffi::OpusEncoder, req: i32, val: i32) -> i32 {
    unsafe { ffi::opus_encoder_ctl(st, req, val) }
}

pub struct OpusEncoder {
    st: *mut ffi::OpusEncoder,
    cfg: OpusConfig,
    out: Vec<u8>,
}

// Le pointeur est possede exclusivement par la struct ; libopus n'a pas d'etat global.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(cfg: OpusConfig) -> Result<Self> {
        if cfg.channels != 1 && cfg.channels != 2 {
            return Err(AudioError::BadArg("channels doit etre 1 ou 2".into()));
        }
        let app = match cfg.application {
            Application::Audio => ffi::OPUS_APPLICATION_AUDIO,
            Application::Voip => ffi::OPUS_APPLICATION_VOIP,
            Application::LowDelay => ffi::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        };
        let mut err = 0i32;
        let st = unsafe { ffi::opus_encoder_create(SAMPLE_RATE as i32, cfg.channels as i32, app, &mut err) };
        if st.is_null() || err != 0 {
            return Err(AudioError::Opus(format!("opus_encoder_create: {err}")));
        }
        let mut e = Self { st, cfg, out: vec![0u8; 4000] };
        e.apply()?;
        Ok(e)
    }

    fn apply(&mut self) -> Result<()> {
        let st = self.st;
        let c = &self.cfg;
        check(enc_ctl(st, ffi::OPUS_SET_BITRATE_REQUEST, c.bitrate_bps), "bitrate")?;
        let (vbr, cons) = match c.rate_mode {
            RateMode::Cbr => (0, 0),
            RateMode::Vbr => (1, 0),
            RateMode::ConstrainedVbr => (1, 1),
        };
        check(enc_ctl(st, ffi::OPUS_SET_VBR_REQUEST, vbr), "vbr")?;
        check(enc_ctl(st, ffi::OPUS_SET_VBR_CONSTRAINT_REQUEST, cons), "vbr_constraint")?;
        check(enc_ctl(st, ffi::OPUS_SET_COMPLEXITY_REQUEST, c.complexity.clamp(0, 10)), "complexity")?;
        check(enc_ctl(st, ffi::OPUS_SET_INBAND_FEC_REQUEST, c.fec as i32), "fec")?;
        check(
            enc_ctl(st, ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST, c.expected_loss_percent.clamp(0, 100)),
            "packet_loss",
        )?;
        check(enc_ctl(st, ffi::OPUS_SET_DTX_REQUEST, c.dtx as i32), "dtx")?;
        check(enc_ctl(st, ffi::OPUS_SET_EXPERT_FRAME_DURATION_REQUEST, c.frame.opus_const()), "frame_duration")?;
        check(enc_ctl(st, ffi::OPUS_SET_SIGNAL_REQUEST, ffi::OPUS_AUTO), "signal")?;
        Ok(())
    }

    pub fn config(&self) -> &OpusConfig {
        &self.cfg
    }

    /// Echantillons par canal attendus par `encode`.
    pub fn frame_samples(&self) -> usize {
        self.cfg.frame.samples()
    }

    /// Longueur attendue du buffer PCM entrelace (`frame_samples * channels`).
    pub fn frame_len(&self) -> usize {
        self.frame_samples() * self.cfg.channels
    }

    /// Change le debit a chaud (adaptation au reseau).
    pub fn set_bitrate(&mut self, bps: i32) -> Result<()> {
        self.cfg.bitrate_bps = bps;
        check(enc_ctl(self.st, ffi::OPUS_SET_BITRATE_REQUEST, bps), "bitrate")
    }

    /// Change la perte attendue (%) a chaud.
    pub fn set_expected_loss(&mut self, percent: i32) -> Result<()> {
        self.cfg.expected_loss_percent = percent;
        check(enc_ctl(self.st, ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST, percent.clamp(0, 100)), "packet_loss")
    }

    /// Encode exactement une trame de PCM f32 entrelace. Retourne le paquet Opus.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>> {
        if pcm.len() != self.frame_len() {
            return Err(AudioError::BadArg(format!(
                "pcm: {} echantillons, {} attendus",
                pcm.len(),
                self.frame_len()
            )));
        }
        let n = unsafe {
            ffi::opus_encode_float(
                self.st,
                pcm.as_ptr(),
                self.frame_samples() as i32,
                self.out.as_mut_ptr(),
                self.out.len() as i32,
            )
        };
        check(n, "opus_encode_float")?;
        Ok(self.out[..n as usize].to_vec())
    }

    /// Variante i16.
    pub fn encode_i16(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        self.encode(&crate::i16_to_f32(pcm))
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_encoder_destroy(self.st) }
    }
}

pub struct OpusDecoder {
    st: *mut ffi::OpusDecoder,
    channels: usize,
    frame_samples: usize,
}

unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    /// `frame` : duree d'une trame du flux (sert a dimensionner le PLC et le FEC).
    pub fn new(channels: usize, frame: FrameDuration) -> Result<Self> {
        if channels != 1 && channels != 2 {
            return Err(AudioError::BadArg("channels doit etre 1 ou 2".into()));
        }
        let mut err = 0i32;
        let st = unsafe { ffi::opus_decoder_create(SAMPLE_RATE as i32, channels as i32, &mut err) };
        if st.is_null() || err != 0 {
            return Err(AudioError::Opus(format!("opus_decoder_create: {err}")));
        }
        Ok(Self { st, channels, frame_samples: frame.samples() })
    }

    pub fn from_config(cfg: &OpusConfig) -> Result<Self> {
        Self::new(cfg.channels, cfg.frame)
    }

    /// `Some(paquet)` : decode normalement. `None` : PLC (masquage d'une trame perdue,
    /// extrapole a partir de l'etat du decodeur). Retourne du PCM f32 entrelace.
    pub fn decode(&mut self, packet: Option<&[u8]>) -> Result<Vec<f32>> {
        match packet {
            Some(p) if !p.is_empty() => {
                // 120 ms = taille maximale d'un paquet Opus.
                let mut out = vec![0f32; 5760 * self.channels];
                let n = unsafe {
                    ffi::opus_decode_float(self.st, p.as_ptr(), p.len() as i32, out.as_mut_ptr(), 5760, 0)
                };
                check(n, "opus_decode_float")?;
                out.truncate(n as usize * self.channels);
                Ok(out)
            }
            _ => self.plc(),
        }
    }

    fn plc(&mut self) -> Result<Vec<f32>> {
        let mut out = vec![0f32; self.frame_samples * self.channels];
        let n = unsafe {
            ffi::opus_decode_float(self.st, std::ptr::null(), 0, out.as_mut_ptr(), self.frame_samples as i32, 0)
        };
        check(n, "opus_decode_float(plc)")?;
        out.truncate(n as usize * self.channels);
        Ok(out)
    }

    /// Recupere la trame PRECEDENTE (perdue) a partir du FEC embarque dans le paquet
    /// `next_packet` (celui qui la suit). A appeler avant `decode(Some(next_packet))`.
    /// Si le paquet ne contient pas de FEC (mode CELT), libopus fait du PLC.
    pub fn decode_fec(&mut self, next_packet: &[u8]) -> Result<Vec<f32>> {
        let mut out = vec![0f32; self.frame_samples * self.channels];
        let n = unsafe {
            ffi::opus_decode_float(
                self.st,
                next_packet.as_ptr(),
                next_packet.len() as i32,
                out.as_mut_ptr(),
                self.frame_samples as i32,
                1,
            )
        };
        check(n, "opus_decode_float(fec)")?;
        out.truncate(n as usize * self.channels);
        Ok(out)
    }

    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }
    pub fn channels(&self) -> usize {
        self.channels
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_decoder_destroy(self.st) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, amp: f32, frames: usize, offset: usize, ch: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * ch);
        for i in 0..frames {
            let s = amp * (2.0 * std::f32::consts::PI * freq * (offset + i) as f32 / 48000.0).sin();
            for _ in 0..ch {
                v.push(s);
            }
        }
        v
    }

    /// SNR apres recherche du meilleur alignement (le codec a un delai de lookahead).
    fn best_snr_db(orig: &[f32], dec: &[f32], ch: usize) -> (f32, usize) {
        let mut best = (f32::MIN, 0);
        let start = 9600; // saute l'etablissement (200 ms)
        let n = orig.len() / ch - start - 1000;
        for delay in 0..600usize {
            let (mut sig, mut noise) = (0f64, 0f64);
            for i in start..start + n {
                let o = orig[i * ch] as f64;
                let d = dec[(i + delay) * ch] as f64;
                sig += o * o;
                noise += (o - d) * (o - d);
            }
            let snr = 10.0 * ((sig / noise.max(1e-20)).log10()) as f32;
            if snr > best.0 {
                best = (snr, delay);
            }
        }
        best
    }

    fn roundtrip(cfg: OpusConfig, secs: f32) -> (Vec<f32>, Vec<f32>, usize) {
        let ch = cfg.channels;
        let mut enc = OpusEncoder::new(cfg.clone()).unwrap();
        let mut dec = OpusDecoder::from_config(&cfg).unwrap();
        let fs = enc.frame_samples();
        let nframes = (secs * 48000.0) as usize / fs;
        let mut orig = Vec::new();
        let mut out = Vec::new();
        let mut bytes = 0;
        for f in 0..nframes {
            let pcm = sine(1000.0, 0.5, fs, f * fs, ch);
            let pkt = enc.encode(&pcm).unwrap();
            bytes += pkt.len();
            out.extend(dec.decode(Some(&pkt)).unwrap());
            orig.extend(pcm);
        }
        (orig, out, bytes)
    }

    #[test]
    fn snr_sine_10ms_cbr() {
        let cfg = OpusConfig { rate_mode: RateMode::Cbr, ..Default::default() };
        let (o, d, bytes) = roundtrip(cfg, 1.0);
        let (snr, delay) = best_snr_db(&o, &d, 2);
        println!("10ms CBR: SNR {snr:.1} dB, delai {delay} ech, {bytes} octets/s");
        assert!(snr > 20.0, "SNR trop faible: {snr}");
        // CBR 128k, 10 ms => 160 octets/paquet
        assert_eq!(bytes, 160 * 100);
    }

    #[test]
    fn snr_sine_20ms_vbr() {
        let cfg = OpusConfig { frame: FrameDuration::Ms20, rate_mode: RateMode::Vbr, ..Default::default() };
        let (o, d, _) = roundtrip(cfg, 1.0);
        let (snr, delay) = best_snr_db(&o, &d, 2);
        println!("20ms VBR: SNR {snr:.1} dB, delai {delay} ech");
        assert!(snr > 20.0, "SNR trop faible: {snr}");
    }

    #[test]
    fn i16_roundtrip() {
        let cfg = OpusConfig::default();
        let mut enc = OpusEncoder::new(cfg.clone()).unwrap();
        let mut dec = OpusDecoder::from_config(&cfg).unwrap();
        let pcm = crate::f32_to_i16(&sine(440.0, 0.3, enc.frame_samples(), 0, 2));
        let pkt = enc.encode_i16(&pcm).unwrap();
        assert_eq!(dec.decode(Some(&pkt)).unwrap().len(), enc.frame_len());
    }

    #[test]
    fn rejects_bad_frame_length() {
        let mut enc = OpusEncoder::new(OpusConfig::default()).unwrap();
        assert!(enc.encode(&[0.0; 10]).is_err());
    }

    #[test]
    fn plc_continues_signal() {
        let cfg = OpusConfig { rate_mode: RateMode::Cbr, ..Default::default() };
        let mut enc = OpusEncoder::new(cfg.clone()).unwrap();
        let mut dec = OpusDecoder::from_config(&cfg).unwrap();
        let fs = enc.frame_samples();
        let mut concealed = Vec::new();
        let mut ref_rms = 0f32;
        for f in 0..60 {
            let pcm = sine(500.0, 0.5, fs, f * fs, 2);
            let pkt = enc.encode(&pcm).unwrap();
            if f == 40 || f == 41 {
                // perte de 2 trames consecutives
                concealed.extend(dec.decode(None).unwrap());
            } else {
                let out = dec.decode(Some(&pkt)).unwrap();
                if f == 39 {
                    ref_rms = (out.iter().map(|x| x * x).sum::<f32>() / out.len() as f32).sqrt();
                }
            }
        }
        assert_eq!(concealed.len(), 2 * fs * 2);
        let rms = (concealed.iter().map(|x| x * x).sum::<f32>() / concealed.len() as f32).sqrt();
        println!("PLC rms {rms:.3} (ref {ref_rms:.3})");
        // le PLC doit prolonger un signal non nul, sans exploser
        assert!(rms > 0.2 * ref_rms && rms < 1.5 * ref_rms, "rms PLC {rms} vs ref {ref_rms}");
        assert!(concealed.iter().all(|x| x.is_finite() && x.abs() < 1.5));
    }

    #[test]
    fn fec_recovers_lost_frame_better_than_plc() {
        // Voix a bas debit : SILK/hybride avec LBRR (FEC in-band).
        let cfg = OpusConfig {
            frame: FrameDuration::Ms20,
            channels: 1,
            bitrate_bps: 24_000,
            application: Application::Voip,
            fec: true,
            expected_loss_percent: 20,
            rate_mode: RateMode::Cbr,
            ..Default::default()
        };
        let mut enc = OpusEncoder::new(cfg.clone()).unwrap();
        let fs = enc.frame_samples();
        // signal "voise" : 12 harmoniques a 150 Hz, enveloppe lente, un peu de bruit deterministe
        let mut pkts = Vec::new();
        for f in 0..120 {
            let pcm: Vec<f32> = (0..fs)
                .map(|i| {
                    let n = f * fs + i;
                    let t = n as f32 / 48000.0;
                    let env = 0.7 + 0.3 * (2.0 * std::f32::consts::PI * 2.0 * t).sin();
                    let mut v = 0.0;
                    for h in 1..=12 {
                        v += (2.0 * std::f32::consts::PI * 150.0 * h as f32 * t).sin() / h as f32;
                    }
                    v * 0.2 * env + (((n as u32).wrapping_mul(2654435761) >> 16) as f32 / 65536.0 - 0.5) * 0.05
                })
                .collect();
            pkts.push(enc.encode(&pcm).unwrap());
        }
        let with_fec = pkts.iter().skip(5).filter(|p| crate::ffi::packet_has_fec(p)).count();
        assert!(with_fec > 50, "le FEC devrait etre present dans la plupart des paquets ({with_fec})");

        // trames perdues : celles dont le paquet SUIVANT porte du FEC
        let lost_set: Vec<usize> = (20..100).filter(|&f| f % 7 == 0 && crate::ffi::packet_has_fec(&pkts[f + 1])).collect();
        assert!(lost_set.len() >= 5);
        let mut d_ref = OpusDecoder::from_config(&cfg).unwrap();
        let mut d_fec = OpusDecoder::from_config(&cfg).unwrap();
        let mut d_plc = OpusDecoder::from_config(&cfg).unwrap();
        let (mut e_fec, mut e_plc, mut sig) = (0f64, 0f64, 0f64);
        for f in 0..110 {
            let r = d_ref.decode(Some(&pkts[f])).unwrap();
            if lost_set.contains(&f) {
                let a = d_fec.decode_fec(&pkts[f + 1]).unwrap();
                let b = d_plc.decode(None).unwrap();
                assert_eq!(a.len(), fs);
                for i in 0..fs {
                    e_fec += ((a[i] - r[i]) as f64).powi(2);
                    e_plc += ((b[i] - r[i]) as f64).powi(2);
                    sig += (r[i] as f64).powi(2);
                }
            } else {
                d_fec.decode(Some(&pkts[f])).unwrap();
                d_plc.decode(Some(&pkts[f])).unwrap();
            }
        }
        println!(
            "{} pertes : erreur relative FEC {:.3} vs PLC {:.3} (SNR {:.1} dB vs {:.1} dB)",
            lost_set.len(),
            e_fec / sig,
            e_plc / sig,
            -10.0 * (e_fec / sig).log10(),
            -10.0 * (e_plc / sig).log10()
        );
        assert!(e_fec < 0.7 * e_plc, "le FEC doit reconstruire nettement mieux que le PLC");
    }
}
