//! Tampon C opaque aligné (structures d API natives remplies par offsets mesurés).

use std::ffi::c_void;

/// Tampon aligné sur 8 octets pour une structure C opaque.
pub(crate) struct Blob(Vec<u64>);

impl Blob {
    pub fn new(size: usize) -> Blob {
        Blob(vec![0u64; size.div_ceil(8)])
    }
    pub fn ptr(&mut self) -> *mut c_void {
        self.0.as_mut_ptr() as *mut c_void
    }
    pub fn bytes(&mut self) -> &mut [u8] {
        let n = self.0.len() * 8;
        // SAFETY : vue octet d'un Vec<u64> possédé.
        unsafe { std::slice::from_raw_parts_mut(self.0.as_mut_ptr() as *mut u8, n) }
    }
    pub fn put_i32(&mut self, off: usize, v: i32) {
        self.bytes()[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    pub fn put_f32(&mut self, off: usize, v: f32) {
        self.bytes()[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    pub fn put_u8(&mut self, off: usize, v: u8) {
        self.bytes()[off] = v;
    }
    pub fn put_u64(&mut self, off: usize, v: u64) {
        self.bytes()[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    pub fn get_i32(&mut self, off: usize) -> i32 {
        i32::from_le_bytes(self.bytes()[off..off + 4].try_into().unwrap())
    }
    pub fn get_ptr(&mut self, off: usize) -> *mut c_void {
        u64::from_le_bytes(self.bytes()[off..off + 8].try_into().unwrap()) as usize as *mut c_void
    }

    pub fn put_ptr(&mut self, off: usize, v: *const c_void) {
        self.put_u64(off, v as usize as u64);
    }
    pub fn put_guid(&mut self, off: usize, g: &windows::core::GUID) {
        // SAFETY : GUID est repr(C), 16 octets.
        let b: [u8; 16] = unsafe { std::mem::transmute_copy(g) };
        self.bytes()[off..off + 16].copy_from_slice(&b);
    }
    pub fn get_u32(&mut self, off: usize) -> u32 {
        self.get_i32(off) as u32
    }
    /// Met à `v` le champ de bits de largeur `width` commençant au bit `bit` du mot u32 en `word`.
    pub fn put_bits(&mut self, word: usize, bit: u32, width: u32, v: u32) {
        let mask = ((1u64 << width) - 1) as u32;
        let cur = self.get_u32(word) & !(mask << bit);
        self.put_i32(word, (cur | ((v & mask) << bit)) as i32);
    }
    pub fn copy_from(&mut self, off: usize, src: &[u8]) {
        self.bytes()[off..off + src.len()].copy_from_slice(src);
    }
}
