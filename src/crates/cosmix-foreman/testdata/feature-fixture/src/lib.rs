pub fn always_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_feature_passes() {
        assert!(always_true());
    }

    #[cfg(feature = "broken")]
    #[test]
    fn gated_test_fails() {
        panic!("fixture: this gated test always fails");
    }
}
