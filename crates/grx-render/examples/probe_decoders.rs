use windows::core::PWSTR;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
        for (label, sub) in [("H264", MFVideoFormat_H264), ("HEVC", MFVideoFormat_HEVC)] {
            for (fl, flname) in [(MFT_ENUM_FLAG_HARDWARE, "HW"), (MFT_ENUM_FLAG_SYNCMFT, "SYNC"), (MFT_ENUM_FLAG_ASYNCMFT, "ASYNC"), (MFT_ENUM_FLAG_ALL, "ALL")] {
                let info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: sub };
                let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
                let mut count = 0u32;
                let r = MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG(fl.0 | MFT_ENUM_FLAG_SORTANDFILTER.0), Some(&info), None, &mut pp, &mut count);
                println!("{label} {flname}: r={:?} count={count}", r);
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
