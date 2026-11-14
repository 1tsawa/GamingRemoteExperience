use super::*;

#[test]
fn warmup_reseeds_after_inflated_first_sample() {
    let mut estimator = RttEstimator::new();
    estimator.on_sample(0, 250_000);
    estimator.on_sample(500_000, 1_000);
    assert_eq!(estimator.srtt(), Some(1_000));
}

#[test]
fn reseed_only_during_warmup() {
    let mut estimator = RttEstimator::new();
    for i in 0..8 {
        estimator.on_sample(i * 500_000, 1_000);
    }
    estimator.on_sample(8 * 500_000, 100_000);
    estimator.on_sample(9 * 500_000, 1_000);
    let srtt = estimator.srtt().unwrap();
    assert!(srtt > 1_000, "Expected srtt > 1_000, got {}", srtt);
    assert!(srtt < 100_000, "Expected srtt < 100_000, got {}", srtt);
}

#[test]
fn moderate_drop_is_smoothed() {
    let mut estimator = RttEstimator::new();
    estimator.on_sample(0, 4_000);
    estimator.on_sample(500_000, 2_000);
    assert_eq!(estimator.srtt(), Some(3_750));
}
