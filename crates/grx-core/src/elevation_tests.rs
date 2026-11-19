use super::*;

#[test]
fn test_validate_run_args_accepts_empty() {
    assert!(validate_run_args(&[]).is_ok());
}

#[test]
fn test_validate_run_args_accepts_typical() {
    let args = vec![
        "--bind".to_string(), "0.0.0.0:7777".to_string(),
        "--codec".to_string(), "hevc".to_string(),
        "--fps".to_string(), "60".to_string(),
        "--no-upnp".to_string(),
        "--fill".to_string(),
        "--no-audio".to_string(),
        "--display".to_string(), "2".to_string(),
    ];
    assert!(validate_run_args(&args).is_ok());
}

#[test]
fn test_validate_run_args_accepts_ipv6_bind() {
    let args = vec!["--bind".to_string(), "[::]:7777".to_string()];
    assert!(validate_run_args(&args).is_ok());
}

#[test]
fn test_validate_run_args_refuses_data_dir() {
    assert!(validate_run_args(&["--data-dir".to_string(), "val".to_string()]).is_err());
}

#[test]
fn test_validate_run_args_refuses_from_task() {
    assert!(validate_run_args(&["--from-task".to_string()]).is_err());
}

#[test]
fn test_validate_run_args_refuses_simulate_secure() {
    assert!(validate_run_args(&["--simulate-secure".to_string()]).is_err());
}

#[test]
fn test_validate_run_args_refuses_equals_syntax() {
    assert!(validate_run_args(&["--fps=60".to_string()]).is_err());
}

#[test]
fn test_validate_run_args_refuses_positional_run() {
    assert!(validate_run_args(&["run".to_string()]).is_err());
}

#[test]
fn test_validate_run_args_refuses_missing_value() {
    assert!(validate_run_args(&["--fps".to_string()]).is_err());
}

#[test]
fn test_validate_run_args_refuses_empty_value() {
    assert!(validate_run_args(&["--bind".to_string(), "".to_string()]).is_err());
}

#[test]
fn test_validate_run_args_refuses_value_starting_with_dash() {
    assert!(validate_run_args(&["--fps".to_string(), "-1".to_string()]).is_err());
}

#[test]
fn test_validate_run_args_refuses_value_too_long() {
    let long_val = "a".repeat(65);
    assert!(validate_run_args(&["--bind".to_string(), long_val]).is_err());
}

#[test]
fn test_validate_run_args_refuses_value_with_control_char() {
    assert!(validate_run_args(&["--bind".to_string(), "a\nb".to_string()]).is_err());
}

#[test]
fn test_validate_run_args_refuses_too_many_args() {
    let args = vec!["--fill".to_string(); 65];
    assert!(validate_run_args(&args).is_err());
}

#[test]
fn test_task_options_parse_valid_json() {
    let json = r#"{"args": ["--bind", "0.0.0.0:9000"]}"#;
    let opts = TaskOptions::parse(json.as_bytes()).unwrap();
    assert_eq!(opts.args, vec!["--bind".to_string(), "0.0.0.0:9000".to_string()]);
}

#[test]
fn test_task_options_parse_migrates_old_port() {
    let json = r#"{"args": ["--bind", "[::]:7777", "--external-port", "7777", "--fps", "60"]}"#;
    let opts = TaskOptions::parse(json.as_bytes()).unwrap();
    assert_eq!(opts.args, vec!["--bind".to_string(), "[::]:443".into(), "--fps".into(), "60".into()]);
    // Port externe choisi (≠ 7777) : conservé.
    let opts = TaskOptions::parse(br#"{"args": ["--external-port", "7790"]}"#).unwrap();
    assert_eq!(opts.args, vec!["--external-port".to_string(), "7790".into()]);
}

#[test]
fn test_task_options_parse_invalid_json() {
    assert!(TaskOptions::parse(b"not json").is_err());
}

#[test]
fn test_task_options_parse_forbidden_args() {
    let json = r#"{"args": ["--data-dir", "x"]}"#;
    assert!(TaskOptions::parse(json.as_bytes()).is_err());
}

#[test]
fn test_task_options_parse_buffer_too_large() {
    let buf = vec![0u8; MAX_OPTIONS_BYTES + 1];
    assert!(TaskOptions::parse(&buf).is_err());
}

#[test]
fn test_xml_escape() {
    assert_eq!(
        xml_escape("a&b<c>d\"e'f"),
        "a&amp;b&lt;c&gt;d&quot;e&apos;f"
    );
}

#[test]
fn test_task_xml_contains_expected_tags() {
    let spec = TaskSpec {
        exe: r"C:\Program Files\GamingRemoteExperience\grx-host.exe".into(),
        user_sid: "S-1-5-21-1-2-3-1001".into(),
        at_logon: true,
    };
    let xml = task_xml(&spec).unwrap();
    assert!(xml.contains("<LogonTrigger>"));
    assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
    assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
    assert!(xml.contains("<Arguments>run --from-task</Arguments>"));
    assert!(xml.contains("<WorkingDirectory>C:\\Program Files\\GamingRemoteExperience</WorkingDirectory>"));
    assert!(xml.contains("<Priority>4</Priority>"));
}

#[test]
fn test_task_xml_no_logon_trigger_when_false() {
    let spec = TaskSpec {
        exe: r"C:\Program Files\GamingRemoteExperience\grx-host.exe".into(),
        user_sid: "S-1-5-21-1-2-3-1001".into(),
        at_logon: false,
    };
    let xml = task_xml(&spec).unwrap();
    assert!(!xml.contains("LogonTrigger"));
}

#[test]
fn test_parse_task_xml_roundtrip() {
    let spec = TaskSpec {
        exe: r"C:\Program Files\GamingRemoteExperience\grx-host.exe".into(),
        user_sid: "S-1-5-21-1-2-3-1001".into(),
        at_logon: true,
    };
    let xml = task_xml(&spec).unwrap();
    let info = parse_task_xml(&xml);
    assert_eq!(info.command, Some(spec.exe));
    assert_eq!(info.arguments.as_deref(), Some(TASK_ARGS));
    assert!(info.at_logon);
    assert!(info.highest);
    assert_eq!(info.user_id, Some(spec.user_sid));
}

#[test]
fn test_task_xml_ampersand_escaped_and_roundtrips() {
    let spec = TaskSpec {
        exe: r"C:\A&B\grx-host.exe".into(),
        user_sid: "S-1-5-21-1-2-3-1001".into(),
        at_logon: false,
    };
    let xml = task_xml(&spec).unwrap();
    assert!(xml.contains("&amp;"));
    let info = parse_task_xml(&xml);
    assert_eq!(info.command, Some(spec.exe));
}

#[test]
fn test_task_xml_refuses_relative_exe() {
    let spec = TaskSpec {
        exe: "grx-host.exe".into(),
        user_sid: "S-1-5-21-1-2-3-1001".into(),
        at_logon: false,
    };
    assert!(task_xml(&spec).is_err());
}

#[test]
fn test_task_xml_refuses_invalid_sid() {
    let spec = TaskSpec {
        exe: r"C:\Program Files\GamingRemoteExperience\grx-host.exe".into(),
        user_sid: "S-1-5-21-abc".into(),
        at_logon: false,
    };
    assert!(task_xml(&spec).is_err());

    let spec2 = TaskSpec {
        exe: r"C:\Program Files\GamingRemoteExperience\grx-host.exe".into(),
        user_sid: "".into(),
        at_logon: false,
    };
    assert!(task_xml(&spec2).is_err());
}

#[test]
fn test_valid_sid_true_cases() {
    assert!(valid_sid("S-1-5-18"));
    assert!(valid_sid("S-1-5-21-1-2-3-1001"));
}

#[test]
fn test_valid_sid_false_cases() {
    assert!(!valid_sid("S-1-"));
    assert!(!valid_sid("S-1-5--1"));
    assert!(!valid_sid("X-1-5"));
    assert!(!valid_sid("S-1-5-21-1;2"));
}

#[test]
fn test_decode_tool_output_utf8() {
    assert_eq!(decode_tool_output(b"Hello"), "Hello");
}

#[test]
fn test_decode_tool_output_utf8_with_bom() {
    let input = [0xEF, 0xBB, 0xBF, b'H', b'e', b'l', b'l', b'o'];
    assert_eq!(decode_tool_output(&input), "Hello");
}

#[test]
fn test_decode_tool_output_utf16le_with_bom() {
    let mut input: Vec<u8> = vec![0xFF, 0xFE];
    for c in "Tâche".encode_utf16() {
        input.extend_from_slice(&c.to_le_bytes());
    }
    assert_eq!(decode_tool_output(&input), "Tâche");
}

#[test]
fn test_decode_tool_output_utf16le_without_bom() {
    let input: Vec<u8> = "Tâche".encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    assert_eq!(decode_tool_output(&input), "Tâche");
}

#[test]
fn test_is_within_ci_true_cases() {
    assert!(is_within_ci(r"C:\Program Files\GRX\grx-host.exe", r"C:\Program Files"));
    assert!(is_within_ci(r"C:\Program Files\GRX\grx-host.exe", r"c:\program files"));
    assert!(is_within_ci(r"\\?\C:\Program Files\GRX\x.exe", r"C:\Program Files"));
}

#[test]
fn test_is_within_ci_false_cases() {
    assert!(!is_within_ci(r"C:\Program Files", r"C:\Program Files"));
    assert!(!is_within_ci(r"C:\Program Files (x86)\a.exe", r"C:\Program Files"));
    assert!(!is_within_ci(r"C:\Program Files\..\Users\x.exe", r"C:\Program Files"));
    assert!(!is_within_ci(r"C:\Program Files\GRX\x.exe", ""));
}

#[test]
fn test_stop_event_name() {
    assert_eq!(stop_event_name(42), r"Local\GamingRemoteExperience-Host-Stop-42");
}
