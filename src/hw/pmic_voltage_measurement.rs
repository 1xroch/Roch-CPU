//! A historical PMIC ADC observation (see [`PmicVoltageMeasurement`]).

use chrono::{DateTime, Utc};

/// A historical PMIC ADC observation, captured only during existing calibration/verification.
/// It is never a voltage target and reading this record performs no hardware access.
/// A failed latest attempt retains the previous successful observation for diagnostic context.
#[derive(Clone, Debug, PartialEq)]
pub struct PmicVoltageMeasurement {
    pub last_successful_volts: Option<f64>,
    pub sampled_at_utc: Option<DateTime<Utc>>,
    pub last_attempt_at_utc: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub source: String,
}

impl PmicVoltageMeasurement {
    pub fn new(
        last_successful_volts: Option<f64>,
        sampled_at_utc: Option<DateTime<Utc>>,
        last_attempt_at_utc: Option<DateTime<Utc>>,
        last_error: Option<String>,
        source: impl Into<String>,
    ) -> PmicVoltageMeasurement {
        PmicVoltageMeasurement { last_successful_volts, sampled_at_utc, last_attempt_at_utc, last_error, source: source.into() }
    }

    pub fn latest_attempt_succeeded(&self) -> bool {
        self.last_error.is_none() && self.last_successful_volts.is_some() && self.sampled_at_utc.is_some()
    }

    pub fn not_read(source: impl Into<String>) -> PmicVoltageMeasurement {
        PmicVoltageMeasurement::new(None, None, None, Some("No PMIC ADC sample has been recorded.".to_string()), source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_read_is_not_a_successful_attempt() {
        let m = PmicVoltageMeasurement::not_read("PMIC ADC SWA");
        assert!(!m.latest_attempt_succeeded());
        assert_eq!(m.source, "PMIC ADC SWA");
        assert_eq!(m.last_error.as_deref(), Some("No PMIC ADC sample has been recorded."));
        let now = Utc::now();
        assert!(PmicVoltageMeasurement::new(Some(1.1), Some(now), Some(now), None, "x").latest_attempt_succeeded());
        assert!(!PmicVoltageMeasurement::new(Some(1.1), None, Some(now), None, "x").latest_attempt_succeeded());
    }
}
