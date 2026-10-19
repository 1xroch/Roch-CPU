//! Formats immutable PMIC ADC snapshots only. No delegate, transport, target, default or draft
//! value supplies a measurement, and no fresh hardware sampling takes place here.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use super::memory_voltage_link::MemoryVoltageGroup;
use super::setting::Setting;
use crate::hw::pmic_voltage_measurement::PmicVoltageMeasurement;

/// A presentation of cached ADC observations, independent of staged voltage targets.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryVoltageMeasurementText {
    pub text: String,
    pub tooltip: String,
    pub has_complete_samples: bool,
    pub is_mixed: bool,
}

pub fn for_setting(
    setting: &Setting,
    measurement: Option<&PmicVoltageMeasurement>,
    now_utc: Option<DateTime<Utc>>,
) -> MemoryVoltageMeasurementText {
    let now = now_utc.unwrap_or_else(Utc::now);
    let detail = detail(setting, measurement, now);
    match measurement.filter(|m| valid(Some(m))) {
        Some(m) => MemoryVoltageMeasurementText {
            text: format!(
                "Last measured {} V ({})",
                volts(m.last_successful_volts.unwrap()),
                age(m.sampled_at_utc.unwrap(), now)
            ),
            tooltip: detail,
            has_complete_samples: true,
            is_mixed: false,
        },
        None => MemoryVoltageMeasurementText {
            text: "Last measured: Not read".into(),
            tooltip: detail,
            has_complete_samples: false,
            is_mixed: false,
        },
    }
}

pub fn for_group(
    group: &MemoryVoltageGroup,
    measurements: &HashMap<String, PmicVoltageMeasurement>,
    now_utc: Option<DateTime<Utc>>,
) -> MemoryVoltageMeasurementText {
    let now = now_utc.unwrap_or_else(Utc::now);
    let mut valid_samples: Vec<&PmicVoltageMeasurement> = Vec::new();
    let mut details = Vec::new();
    for member in &group.members {
        let sample = measurements.get(&member.id);
        details.push(detail(member, sample, now));
        if let Some(s) = sample.filter(|s| valid(Some(s))) {
            valid_samples.push(s);
        }
    }
    let tooltip = details.join("\r\n");
    if group.members.is_empty() || valid_samples.len() != group.members.len() {
        return MemoryVoltageMeasurementText {
            text: format!("Last measured: Not read ({}/{} DIMMs)", valid_samples.len(), group.members.len()),
            tooltip,
            has_complete_samples: false,
            is_mixed: false,
        };
    }

    let values = valid_samples.iter().map(|m| m.last_successful_volts.unwrap());
    let minimum = values.clone().fold(f64::INFINITY, f64::min);
    let maximum = values.fold(f64::NEG_INFINITY, f64::max);
    // ADC observations retain their actual values; the programmed PMIC target grid and
    // allowed editing range do not change the physical sensor observation.
    let mixed = (maximum - minimum).abs() >= 0.0000001;
    let value = if mixed { format!("{}–{}", volts(minimum), volts(maximum)) } else { volts(minimum) };
    let oldest = valid_samples.iter().map(|m| m.sampled_at_utc.unwrap()).min().unwrap();
    let same_time = valid_samples.iter().all(|m| m.sampled_at_utc == valid_samples[0].sampled_at_utc);
    let age_text = format!("{}{}", if same_time { "" } else { "oldest " }, age(oldest, now));
    MemoryVoltageMeasurementText {
        text: format!("Last measured {value} V ({age_text})"),
        tooltip,
        has_complete_samples: true,
        is_mixed: mixed,
    }
}

fn valid(measurement: Option<&PmicVoltageMeasurement>) -> bool {
    measurement.is_some_and(|m| {
        m.latest_attempt_succeeded() && m.last_successful_volts.is_some_and(|v| v.is_finite() && v >= 0.0)
    })
}

fn detail(setting: &Setting, measurement: Option<&PmicVoltageMeasurement>, now: DateTime<Utc>) -> String {
    let Some(m) = measurement else {
        return format!("{}: Not read; no cached PMIC ADC sample.", setting.name);
    };
    let source = if m.source.trim().is_empty() { "PMIC ADC" } else { m.source.as_str() };
    if valid(Some(m)) {
        let sampled = m.sampled_at_utc.unwrap();
        return format!(
            "{}: Last measured {} V; {source}; sampled {} ({}).",
            setting.name,
            volts(m.last_successful_volts.unwrap()),
            time(sampled),
            age(sampled, now)
        );
    }

    let reason = m
        .last_error
        .clone()
        .unwrap_or_else(|| "No valid nonnegative PMIC ADC sample and timestamp are available.".into());
    let mut d = format!("{}: Not read; {source}; {reason}", setting.name);
    if let Some(attempted) = m.last_attempt_at_utc {
        d += &format!(" Latest attempt {}.", time(attempted));
    }
    if let (Some(old), Some(sampled)) = (m.last_successful_volts, m.sampled_at_utc) {
        if old.is_finite() && old >= 0.0 {
            d += &format!(
                " Previous sample {} V at {} ({}); latest attempt did not provide a usable reading.",
                volts(old),
                time(sampled),
                age(sampled, now)
            );
        }
    }
    d
}

fn volts(value: f64) -> String {
    format!("{value:.3}")
}

fn time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn age(sampled: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - sampled).num_milliseconds() as f64 / 1000.0;
    if seconds < 0.0 {
        return "sample time ahead".into();
    }
    if seconds < 60.0 {
        return format!("{}s", seconds.floor());
    }
    if seconds < 3600.0 {
        return format!("{}m", (seconds / 60.0).floor());
    }
    if seconds < 86400.0 {
        return format!("{}h", (seconds / 3600.0).floor());
    }
    format!("{}d", (seconds / 86400.0).floor())
}
