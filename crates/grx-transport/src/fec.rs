//! FEC : Reed-Solomon systématique sur GF(256) (matrice de Cauchy) + CRC32.
//!
//! Un groupe = `k` fragments de données + `m` fragments de parité (k + m <= 256).
//! Propriété MDS : n'importe quels `k` fragments parmi les `k + m` reconstruisent les
//! données, donc jusqu'à `m` pertes par groupe sont récupérées sans aller-retour réseau.
//!
//! Coefficient de parité `j` pour la donnée `i` : `1 / (x_j ^ y_i)` avec `x_j = j` et
//! `y_i = m + i` ; tous les `x` et `y` sont distincts, donc toute sous-matrice carrée de
//! la matrice de Cauchy est inversible.

use std::sync::OnceLock;

// ───────────────────────────── GF(256), polynôme 0x11d ─────────────────────────────

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
    /// mul[a][b] = a*b
    mul: Box<[[u8; 256]; 256]>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for i in 0..255 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11d;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        let mut mul = Box::new([[0u8; 256]; 256]);
        for a in 1..256usize {
            for b in 1..256usize {
                mul[a][b] = exp[log[a] as usize + log[b] as usize];
            }
        }
        Tables { exp, log, mul }
    })
}

#[inline]
fn gf_inv(a: u8) -> u8 {
    debug_assert!(a != 0);
    let t = tables();
    t.exp[255 - t.log[a as usize] as usize]
}

#[inline]
fn coef(m: usize, j: usize, i: usize) -> u8 {
    gf_inv((j as u8) ^ ((m + i) as u8))
}

/// dst ^= c * src
#[inline]
fn mac(dst: &mut [u8], src: &[u8], c: u8, t: &Tables) {
    debug_assert_eq!(dst.len(), src.len());
    match c {
        0 => {}
        1 => {
            for (d, s) in dst.iter_mut().zip(src) {
                *d ^= *s;
            }
        }
        _ => {
            let row = &t.mul[c as usize];
            for (d, s) in dst.iter_mut().zip(src) {
                *d ^= row[*s as usize];
            }
        }
    }
}

/// Nombre maximal de fragments (données + parité) par groupe.
pub const MAX_SHARDS: usize = 256;

/// Calcule les `m` fragments de parité de `data` (k fragments de même longueur).
pub fn encode(data: &[&[u8]], m: usize) -> Vec<Vec<u8>> {
    let k = data.len();
    assert!(k >= 1 && k + m <= MAX_SHARDS);
    let len = data[0].len();
    let t = tables();
    let mut parity = vec![vec![0u8; len]; m];
    for (i, d) in data.iter().enumerate() {
        debug_assert_eq!(d.len(), len);
        for (j, p) in parity.iter_mut().enumerate() {
            mac(p, d, coef(m, j, i), t);
        }
    }
    parity
}

/// Reconstruit les fragments de DONNÉES manquants. `shards` = `k + m` entrées, toutes les
/// présentes ayant la même longueur. Retourne `false` si trop de fragments manquent.
/// En cas de succès, `shards[0..k]` sont tous `Some`.
pub fn reconstruct(k: usize, m: usize, shards: &mut [Option<Vec<u8>>]) -> bool {
    if shards.len() != k + m || k == 0 || k + m > MAX_SHARDS {
        return false;
    }
    let missing: Vec<usize> = (0..k).filter(|&i| shards[i].is_none()).collect();
    if missing.is_empty() {
        return true;
    }
    let pars: Vec<usize> = (0..m).filter(|&j| shards[k + j].is_some()).take(missing.len()).collect();
    if pars.len() < missing.len() {
        return false;
    }
    let len = match shards.iter().flatten().next() {
        Some(s) => s.len(),
        None => return false,
    };
    if shards.iter().flatten().any(|s| s.len() != len) {
        return false;
    }
    let t = tables();
    let n = missing.len();

    // rhs[r] = parité_r ^ somme_{i connu} coef * d_i
    let mut rhs: Vec<Vec<u8>> = Vec::with_capacity(n);
    for &j in &pars {
        let mut r = shards[k + j].as_ref().unwrap().clone();
        for i in 0..k {
            if let Some(d) = &shards[i] {
                mac(&mut r, d, coef(m, j, i), t);
            }
        }
        rhs.push(r);
    }

    // A[r][c] = coef(pars[r], missing[c]) ; on inverse A par Gauss-Jordan.
    let mut a = vec![vec![0u8; 2 * n]; n];
    for r in 0..n {
        for c in 0..n {
            a[r][c] = coef(m, pars[r], missing[c]);
        }
        a[r][n + r] = 1;
    }
    for col in 0..n {
        let Some(piv) = (col..n).find(|&r| a[r][col] != 0) else { return false };
        a.swap(col, piv);
        let inv = gf_inv(a[col][col]);
        for x in a[col].iter_mut() {
            *x = t.mul[inv as usize][*x as usize];
        }
        for r in 0..n {
            if r != col && a[r][col] != 0 {
                let f = a[r][col];
                for c in 0..2 * n {
                    let v = t.mul[f as usize][a[col][c] as usize];
                    a[r][c] ^= v;
                }
            }
        }
    }
    // x_c = somme_r Ainv[c][r] * rhs[r]
    for (c, &mi) in missing.iter().enumerate() {
        let mut out = vec![0u8; len];
        for r in 0..n {
            mac(&mut out, &rhs[r], a[c][n + r], t);
        }
        shards[mi] = Some(out);
    }
    true
}

// ───────────────────────────── CRC32 (IEEE) ─────────────────────────────

fn crc_tables() -> &'static [[u32; 256]; 8] {
    static T: OnceLock<[[u32; 256]; 8]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [[0u32; 256]; 8];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            t[0][i as usize] = c;
        }
        for i in 0..256 {
            for s in 1..8 {
                let prev = t[s - 1][i];
                t[s][i] = (prev >> 8) ^ t[0][(prev & 0xff) as usize];
            }
        }
        t
    })
}

/// CRC32 (slicing-by-8).
pub fn crc32(data: &[u8]) -> u32 {
    let t = crc_tables();
    let mut c = !0u32;
    let mut chunks = data.chunks_exact(8);
    for ch in &mut chunks {
        let lo = u32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]) ^ c;
        let hi = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
        c = t[7][(lo & 0xff) as usize]
            ^ t[6][((lo >> 8) & 0xff) as usize]
            ^ t[5][((lo >> 16) & 0xff) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xff) as usize]
            ^ t[2][((hi >> 8) & 0xff) as usize]
            ^ t[1][((hi >> 16) & 0xff) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in chunks.remainder() {
        c = t[0][((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *seed >> 33
    }

    #[test]
    fn rs_recovers_any_m_losses() {
        let mut seed = 7;
        for &(k, m) in &[(1usize, 1usize), (4, 1), (10, 3), (48, 5), (100, 20), (200, 56)] {
            let len = 97;
            let data: Vec<Vec<u8>> =
                (0..k).map(|_| (0..len).map(|_| lcg(&mut seed) as u8).collect()).collect();
            let refs: Vec<&[u8]> = data.iter().map(|d| d.as_slice()).collect();
            let parity = encode(&refs, m);
            for trial in 0..20 {
                let mut shards: Vec<Option<Vec<u8>>> =
                    data.iter().cloned().chain(parity.iter().cloned()).map(Some).collect();
                // perd exactement m fragments au hasard (données ou parité)
                let mut lost = 0;
                while lost < m.min(k + m) {
                    let i = (lcg(&mut seed) as usize) % (k + m);
                    if shards[i].is_some() {
                        shards[i] = None;
                        lost += 1;
                    }
                }
                assert!(reconstruct(k, m, &mut shards), "k={k} m={m} trial={trial}");
                for i in 0..k {
                    assert_eq!(shards[i].as_ref().unwrap(), &data[i], "k={k} m={m} i={i}");
                }
            }
            // m+1 pertes de données (si possible) => échec
            if k > m {
                let mut shards: Vec<Option<Vec<u8>>> =
                    data.iter().cloned().chain(parity.iter().cloned()).map(Some).collect();
                for s in shards.iter_mut().take(m + 1) {
                    *s = None;
                }
                assert!(!reconstruct(k, m, &mut shards));
            }
        }
    }
}
