use super::*;

struct BitWriter {
    bits: Vec<u8>,
    buffer: u64,
    count: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self { bits: Vec::new(), buffer: 0, count: 0 }
    }

    fn put(&mut self, v: u64, n: u32) {
        for i in (0..n).rev() {
            self.buffer = (self.buffer << 1) | ((v >> i) & 1);
            self.count += 1;
            if self.count == 8 {
                self.bits.push(self.buffer as u8);
                self.buffer = 0;
                self.count = 0;
            }
        }
    }

    fn put_ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let zeros = 63 - x.leading_zeros();
        self.put(0, zeros);
        self.put(x, zeros + 1);
    }

    fn finish(mut self) -> Vec<u8> {
        self.put(1, 1);
        while self.count > 0 {
            self.buffer <<= 1;
            self.count += 1;
            if self.count == 8 {
                self.bits.push(self.buffer as u8);
                self.buffer = 0;
                self.count = 0;
            }
        }
        self.bits
    }
}

#[test]
fn test_h264_profile_100() {
    let mut w = BitWriter::new();
    w.put(100, 8); w.put(0, 8); w.put(40, 8);
    w.put_ue(0); w.put_ue(1); w.put_ue(0); w.put_ue(1);
    let sps = [vec![0x67], w.finish().to_vec()].concat();
    let data = [vec![0, 0, 0, 1], sps.to_vec()].concat();
    let info = stream_info(Codec::H264, &data).unwrap();
    assert_eq!(info.profile, 100);
    assert_eq!(info.chroma_format_idc, 1);
    assert_eq!(info.bit_depth, 8);
}

#[test]
fn test_h264_profile_244() {
    let mut w = BitWriter::new();
    w.put(244, 8); w.put(0, 8); w.put(40, 8);
    w.put_ue(0); w.put_ue(3); w.put(0, 1); w.put_ue(2);
    let sps = [vec![0x67], w.finish().to_vec()].concat();
    let data = [vec![0, 0, 0, 1], sps.to_vec()].concat();
    let info = stream_info(Codec::H264, &data).unwrap();
    assert_eq!(info.chroma_format_idc, 3);
    assert_eq!(info.bit_depth, 10);
}

#[test]
fn test_h264_profile_66() {
    let mut w = BitWriter::new();
    w.put(66, 8); w.put(0, 8); w.put(40, 8); w.put_ue(0);
    let sps = [vec![0x67], w.finish().to_vec()].concat();
    let data = [vec![0, 0, 0, 1], sps.to_vec()].concat();
    let info = stream_info(Codec::H264, &data).unwrap();
    assert_eq!(info.profile, 66);
    assert_eq!(info.chroma_format_idc, 1);
    assert_eq!(info.bit_depth, 8);
}

#[test]
fn test_hevc_profile_2() {
    let mut w = BitWriter::new();
    w.put(0, 4); w.put(0, 3); w.put(1, 1);
    w.put(0, 2); w.put(0, 1); w.put(2, 5);
    w.put(0, 32); w.put(0, 48); w.put(120, 8);
    w.put_ue(0); w.put_ue(1); w.put_ue(256); w.put_ue(256);
    w.put(0, 1); w.put_ue(2);
    let sps = [vec![0x42, 0x01], w.finish().to_vec()].concat();
    let data = [vec![0, 0, 0, 1], sps.to_vec()].concat();
    let info = stream_info(Codec::Hevc, &data).unwrap();
    assert_eq!(info.profile, 2);
    assert_eq!(info.chroma_format_idc, 1);
    assert_eq!(info.bit_depth, 10);
}

#[test]
fn test_hevc_profile_4() {
    let mut w = BitWriter::new();
    w.put(0, 4); w.put(0, 3); w.put(1, 1);
    w.put(0, 2); w.put(0, 1); w.put(4, 5);
    w.put(0, 32); w.put(0, 48); w.put(120, 8);
    w.put_ue(0); w.put_ue(3); w.put(0, 1);
    w.put_ue(256); w.put_ue(256);
    w.put(1, 1);
    w.put_ue(0); w.put_ue(0); w.put_ue(0); w.put_ue(0);
    w.put_ue(0);
    let sps = [vec![0x42, 0x01], w.finish().to_vec()].concat();
    let data = [vec![0, 0, 0, 1], sps.to_vec()].concat();
    let info = stream_info(Codec::Hevc, &data).unwrap();
    assert_eq!(info.profile, 4);
    assert_eq!(info.chroma_format_idc, 3);
    assert_eq!(info.bit_depth, 8);
}

#[test]
fn test_av1_profiles() {
    let build_av1 = |profile: u8, high_bitdepth: u8| -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(profile as u64, 3);
        w.put(0, 1); w.put(0, 1); w.put(0, 1); w.put(0, 1);
        w.put(0, 5); w.put(0, 12); w.put(8, 5); w.put(0, 1);
        w.put(15, 4); w.put(15, 4);
        w.put(255, 16); w.put(255, 16);
        w.put(0, 1); w.put(0, 3); w.put(0, 4);
        w.put(1, 1); w.put(0, 2);
        w.put(1, 1); w.put(1, 1); w.put(6, 3);
        w.put(0, 3); w.put(high_bitdepth as u64, 1);
        if profile == 0 { w.put(0, 1); }
        w.put(0, 1); w.put(2, 8); w.put(2, 8); w.put(2, 8);
        w.put(0, 1);
        if profile == 0 {
            w.put(0, 1); w.put(0, 1); w.put(0, 1);
        }
        w.put(0, 1); w.put(0, 1);
        w.finish()
    };

    let payload0 = build_av1(0, 1);
    let data0 = [vec![0x12, 0x00, 0x0A, payload0.len() as u8], payload0.to_vec()].concat();
    let info0 = stream_info(Codec::Av1, &data0).unwrap();
    assert_eq!(info0.profile, 0);
    assert_eq!(info0.chroma_format_idc, 1);
    assert_eq!(info0.bit_depth, 10);

    let payload1 = build_av1(1, 0);
    let data1 = [vec![0x12, 0x00, 0x0A, payload1.len() as u8], payload1.to_vec()].concat();
    let info1 = stream_info(Codec::Av1, &data1).unwrap();
    assert_eq!(info1.profile, 1);
    assert_eq!(info1.chroma_format_idc, 3);
    assert_eq!(info1.bit_depth, 8);
}

#[test]
fn test_check_stream() {
    let mut w = BitWriter::new();
    w.put(100, 8); w.put(0, 8); w.put(40, 8);
    w.put_ue(0); w.put_ue(1); w.put_ue(0); w.put_ue(1);
    let sps = [vec![0x67], w.finish().to_vec()].concat();
    let sps_nal = [vec![0, 0, 0, 1], sps.to_vec()].concat();

    let pps_nal = vec![0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80];
    let idr_nal = vec![0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33];
    let full_data = [sps_nal.clone(), pps_nal.clone(), idr_nal.clone()].concat();

    assert!(check_stream(Codec::H264, false, false, &full_data).is_ok());
    assert!(check_stream(Codec::H264, true, false, &full_data).unwrap_err().contains("4:4:4"));
    assert!(check_stream(Codec::H264, false, true, &full_data).unwrap_err().contains("profondeur"));

    let no_idr = [sps_nal.clone(), pps_nal.clone()].concat();
    assert!(check_stream(Codec::H264, false, false, &no_idr).unwrap_err().contains("IDR"));

    assert!(check_stream(Codec::H264, false, false, &[]).unwrap_err().contains("flux vide"));

    let no_sps = [pps_nal, idr_nal].concat();
    assert!(check_stream(Codec::H264, false, false, &no_sps).unwrap_err().contains("paramètres"));
}

#[test]
fn test_unescape() {
    assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3]), vec![0, 0, 1, 0, 0]);
}

#[test]
fn test_robustness() {
    let mut state: u32 = 12345;
    let mut lcg = || {
        state = state.wrapping_mul(1103515245).wrapping_add(12345);
        (state >> 16) & 0x7fff
    };

    for _ in 0..500 {
        let len = lcg() % 64;
        let mut data = Vec::with_capacity(len as usize);
        for _ in 0..len {
            data.push(lcg() as u8);
        }

        let _ = stream_info(Codec::H264, &data);
        let _ = stream_info(Codec::Hevc, &data);
        let _ = stream_info(Codec::Av1, &data);

        let _ = check_stream(Codec::H264, false, false, &data);
        let _ = check_stream(Codec::Hevc, false, false, &data);
        let _ = check_stream(Codec::Av1, false, false, &data);
    }
}
