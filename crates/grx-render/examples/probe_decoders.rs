//! Liste les MFT décodeurs (H.264/HEVC/AV1) et encodeurs matériels visibles par Media Foundation.
use windows::core::PWSTR;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
        let subs = [("H264", MFVideoFormat_H264), ("HEVC", MFVideoFormat_HEVC), ("AV1", MFVideoFormat_AV1)];
        for (cat, catname, is_enc) in
            [(MFT_CATEGORY_VIDEO_DECODER, "DEC", false), (MFT_CATEGORY_VIDEO_ENCODER, "ENC", true)]
        {
            for (label, sub) in subs {
                for (fl, flname) in [(MFT_ENUM_FLAG_HARDWARE, "HW"), (MFT_ENUM_FLAG_SYNCMFT, "SYNC"), (MFT_ENUM_FLAG_ASYNCMFT, "ASYNC")] {
                    let info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: sub };
                    let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
                    let mut count = 0u32;
                    let (inp, out) = if is_enc { (None, Some(&info as *const _)) } else { (Some(&info as *const _), None) };
                    let r = MFTEnumEx(cat, MFT_ENUM_FLAG(fl.0 | MFT_ENUM_FLAG_SORTANDFILTER.0), inp, out, &mut pp, &mut count);
                    println!("{catname} {label} {flname}: r={:?} count={count}", r.is_ok());
                    for i in 0..count as usize {
                        if let Some(a) = std::ptr::read(pp.add(i)) {
                            let mut p = PWSTR::null();
                            let mut len = 0u32;
                            let _ = a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len);
                            println!("   - {}", p.to_string().unwrap_or_default());
                        }
                    }
                }
            }
        }
    }
}
