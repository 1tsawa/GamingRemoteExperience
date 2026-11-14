use super::*;
use std::collections::VecDeque;
use std::time::Duration;

#[derive(Default)]
struct FakeMft {
    events: VecDeque<Option<MftEvent>>,
    need_input: u32,
    inputs: u32,
    outputs: Vec<u32>,
    pending: VecDeque<u32>,
    fail_output: bool,
    polls: u32,
}

impl AsyncMft for FakeMft {
    fn next_event(&mut self) -> Result<Option<MftEvent>, String> {
        self.polls += 1;
        Ok(self.events.pop_front().flatten())
    }

    fn on_need_input(&mut self) {
        self.need_input += 1;
    }

    fn on_have_output(&mut self) -> Result<(), String> {
        if self.fail_output {
            return Err("boom".into());
        }
        self.outputs.push(self.pending.pop_front().unwrap_or(0));
        Ok(())
    }
}

fn submit(m: &mut FakeMft, frame: u32) {
    assert!(m.need_input > 0, "submit requires NeedInput");
    m.need_input -= 1;
    m.inputs += 1;
    m.pending.push_back(frame);
}

#[test]
fn test_basic_encode_cycle() {
    let mut m = FakeMft {
        events: vec![Some(MftEvent::NeedInput)].into(),
        ..Default::default()
    };
    pump(&mut m, Duration::from_secs(1), |m| m.need_input > 0).unwrap();
    submit(&mut m, 1);
    m.events.extend(vec![Some(MftEvent::Other), None, Some(MftEvent::HaveOutput)]);
    pump(&mut m, Duration::from_secs(1), |m| !m.outputs.is_empty()).unwrap();
    assert_eq!(m.outputs, vec![1]);
}

#[test]
fn test_many_frames_in_order() {
    let mut m = FakeMft { events: VecDeque::new(), ..Default::default() };
    for i in 0..50 {
        m.events.push_back(Some(MftEvent::NeedInput));
        pump(&mut m, Duration::from_secs(1), |m| m.need_input > 0).unwrap();
        submit(&mut m, i);
        m.events.push_back(Some(MftEvent::HaveOutput));
        pump(&mut m, Duration::from_secs(1), |m| m.outputs.len() == (i + 1) as usize).unwrap();
    }
    let expected: Vec<u32> = (0..50).collect();
    assert_eq!(m.outputs, expected);
}

#[test]
fn test_decoder_like_two_inputs_in_flight() {
    let mut m = FakeMft {
        events: vec![Some(MftEvent::NeedInput), Some(MftEvent::NeedInput)].into(),
        ..Default::default()
    };
    pump(&mut m, Duration::from_secs(1), |m| m.need_input >= 2).unwrap();
    submit(&mut m, 1);
    submit(&mut m, 2);
    m.events.extend(vec![Some(MftEvent::HaveOutput), Some(MftEvent::HaveOutput)]);
    pump(&mut m, Duration::from_secs(1), |m| m.outputs.len() == 2).unwrap();
    assert_eq!(m.outputs, vec![1, 2]);
}

#[test]
fn test_me_error() {
    let mut m = FakeMft {
        events: vec![Some(MftEvent::Error(0x8000_4005u32 as i32))].into(),
        ..Default::default()
    };
    let res = pump(&mut m, Duration::from_secs(1), |_| false);
    assert!(matches!(res, Err(PumpError::MftError(-2147467259))));
}

#[test]
fn test_on_have_output_failure() {
    let mut m = FakeMft {
        events: vec![Some(MftEvent::HaveOutput)].into(),
        fail_output: true,
        ..Default::default()
    };
    let res = pump(&mut m, Duration::from_secs(1), |_| false);
    assert!(matches!(res, Err(PumpError::Failed(s)) if s == "boom"));
}

#[test]
fn test_timeout() {
    let mut m = FakeMft {
        events: VecDeque::new(),
        ..Default::default()
    };
    let start = std::time::Instant::now();
    let res = pump(&mut m, Duration::from_millis(20), |_| false);
    let elapsed = start.elapsed();
    assert!(matches!(res, Err(PumpError::Timeout)));
    assert!(elapsed >= Duration::from_millis(20));
}

#[test]
fn test_condition_already_true() {
    let mut m = FakeMft {
        events: VecDeque::new(),
        need_input: 1,
        ..Default::default()
    };
    let res = pump(&mut m, Duration::from_secs(1), |m| m.need_input > 0);
    assert!(res.is_ok());
    assert_eq!(m.polls, 0);
}

#[test]
fn test_other_events_ignored() {
    let mut m = FakeMft {
        events: vec![
            Some(MftEvent::Other),
            Some(MftEvent::Other),
            Some(MftEvent::Other),
            Some(MftEvent::Other),
            Some(MftEvent::Other),
            Some(MftEvent::NeedInput)
        ].into(),
        ..Default::default()
    };
    let res = pump(&mut m, Duration::from_secs(1), |m| m.need_input == 1);
    assert!(res.is_ok());
    assert_eq!(m.need_input, 1);
}

#[test]
fn test_pump_error_display() {
    let err_mft = PumpError::MftError(-2147467259);
    let display_mft = format!("{}", err_mft);
    assert!(display_mft.contains("0x80004005"));

    let err_timeout = PumpError::Timeout;
    let display_timeout = format!("{}", err_timeout);
    assert!(!display_timeout.is_empty());
}
