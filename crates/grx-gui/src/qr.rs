//! Génération de QR codes (matrice de modules) et rendu en image pour egui.

use qrcode::{Color, EcLevel, QrCode};

pub struct QrMatrix {
    pub size: usize,
    dark: Vec<bool>,
}

/// Zone de silence autour du code, en modules (la norme en demande 4).
pub const QUIET_ZONE: usize = 4;

impl QrMatrix {
    pub fn encode(text: &str) -> Result<Self, String> {
        let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M)
            .map_err(|e| format!("QR code impossible : {e}"))?;
        let size = code.width();
        let dark = code.to_colors().into_iter().map(|c| c == Color::Dark).collect();
        Ok(Self { size, dark })
    }

    pub fn is_dark(&self, x: usize, y: usize) -> bool {
        x < self.size && y < self.size && self.dark[y * self.size + x]
    }

    /// Côté de l'image rendue, en pixels.
    pub fn pixel_side(&self, scale: usize) -> usize {
        (self.size + 2 * QUIET_ZONE) * scale
    }

    /// Niveaux de gris (0 = noir, 255 = blanc), zone de silence incluse.
    pub fn to_gray(&self, scale: usize) -> Vec<u8> {
        let side = self.pixel_side(scale);
        let mut px = vec![255u8; side * side];
        for y in 0..side {
            for x in 0..side {
                let (mx, my) = (x / scale, y / scale);
                if mx >= QUIET_ZONE && my >= QUIET_ZONE && self.is_dark(mx - QUIET_ZONE, my - QUIET_ZONE) {
                    px[y * side + x] = 0;
                }
            }
        }
        px
    }

    pub fn to_color_image(&self, scale: usize) -> eframe::egui::ColorImage {
        let side = self.pixel_side(scale);
        eframe::egui::ColorImage::from_gray([side, side], &self.to_gray(scale))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grx_core::{Identity, Invite, Token};

    fn decode(m: &QrMatrix, scale: usize) -> String {
        let side = m.pixel_side(scale);
        let gray = m.to_gray(scale);
        let mut img = rqrr::PreparedImage::prepare_from_greyscale(side, side, |x, y| gray[y * side + x]);
        let grids = img.detect_grids();
        assert_eq!(grids.len(), 1, "un QR code doit être détecté");
        let (_meta, content) = grids[0].decode().expect("décodage");
        content
    }

    #[test]
    fn invitation_link_roundtrips_through_the_rendered_qr() {
        let uri = Invite {
            addr: "monpc.duckdns.org:7777".into(),
            host_key: Identity::generate().public(),
            token: Some(Token::random()),
        }
        .to_uri();
        let m = QrMatrix::encode(&uri).unwrap();
        assert!(m.size >= 21 && (m.size - 17) % 4 == 0, "taille de QR valide : {}", m.size);
        for scale in [4, 6] {
            assert_eq!(decode(&m, scale), uri, "échelle {scale}");
        }
    }

    #[test]
    fn too_much_data_is_an_error_not_a_panic() {
        assert!(QrMatrix::encode(&"x".repeat(5000)).is_err());
    }

    #[test]
    fn quiet_zone_is_white() {
        let m = QrMatrix::encode("grx://a:1/b").unwrap();
        let g = m.to_gray(2);
        let side = m.pixel_side(2);
        assert!(g[..side * 2 * QUIET_ZONE].iter().all(|p| *p == 255));
        assert!(g.iter().any(|p| *p == 0));
    }

    /// Décode le QR d'une capture d'écran : `GRX_QR_PNG=capture.png cargo test -p grx-gui -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn decode_screenshot() {
        let path = std::env::var("GRX_QR_PNG").expect("GRX_QR_PNG");
        let img = image::open(&path).expect("image").to_luma8();
        let mut prep = rqrr::PreparedImage::prepare(img);
        let grids = prep.detect_grids();
        assert!(!grids.is_empty(), "aucun QR détecté");
        for g in grids {
            let (_m, content) = g.decode().expect("décodage");
            println!("QR décodé : {content}");
        }
    }
}
