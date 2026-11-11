//! Links staged text for the same voltage rail across DIMMs.

use std::collections::HashMap;

use super::setting::{Setting, SettingGroup};

/// A verified PMIC register grid, supplied by the existing capability detection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryVoltageGrid {
    pub base_millivolts: i32,
    pub step_millivolts: i32,
    /// Highest code the rail accepts: 127 for a plain 7-bit PMIC setpoint, more where an extension
    /// (the ID 8A12 PMIC's VDDQ register 0x23) carries the rail further.
    pub max_code: i32,
}

/// An honest shared-editor projection; constructing it never changes any target.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryVoltageDisplay {
    pub text: String,
    pub is_mixed: bool,
    pub can_edit: bool,
    pub reason: Option<String>,
}

/// The currently supported intersection for one rail across detected DIMMs.
#[derive(Clone)]
pub struct MemoryVoltageGroup {
    pub rail: String,
    /// Snapshots of the member rows at the time the group was computed.
    pub members: Vec<Setting>,
    pub supported_values: Vec<f64>,
    pub can_sync: bool,
    pub reason: Option<String>,
}

impl MemoryVoltageGroup {
    pub fn minimum(&self) -> Option<f64> {
        self.supported_values.first().copied()
    }
    pub fn maximum(&self) -> Option<f64> {
        self.supported_values.last().copied()
    }
    pub fn step(&self) -> Option<f64> {
        if self.supported_values.len() < 2 { None } else { Some(self.supported_values[1] - self.supported_values[0]) }
    }
}

/// Staged text per setting id.
pub type Drafts = HashMap<String, String>;

const TOLERANCE: f64 = 0.0000001;

/// Links staged text for the same voltage rail across DIMMs. This helper never calls a setting's
/// read, write or restore_default delegate and never changes `current` or `default_value`. The
/// caller retains its existing explicit Apply workflow and hardware checks.
///
/// Settings are identified by id; every call takes the live row list so availability, ranges
/// and current values are never stale.
pub struct MemoryVoltageLink {
    ids: Vec<String>,
    grids: HashMap<String, MemoryVoltageGrid>,
    /// Sync is the default. Changing mode does not align or overwrite staged values.
    is_synced: bool,
}

impl MemoryVoltageLink {
    pub fn new(settings: &[Setting], grids: &HashMap<String, MemoryVoltageGrid>) -> Self {
        Self {
            ids: settings.iter().filter(|s| Self::try_get_rail(s).is_some()).map(|s| s.id.clone()).collect(),
            grids: grids.iter().map(|(k, v)| (k.to_lowercase(), *v)).collect(),
            is_synced: true,
        }
    }

    pub fn is_synced(&self) -> bool {
        self.is_synced
    }

    pub fn set_synced(&mut self, synced: bool) {
        self.is_synced = synced;
    }

    fn grid(&self, id: &str) -> Option<MemoryVoltageGrid> {
        self.grids.get(&id.to_lowercase()).copied()
    }

    fn linked<'a>(&self, settings: &'a [Setting]) -> Vec<&'a Setting> {
        // Keep the original detection order of the linked ids.
        self.ids.iter().filter_map(|id| settings.iter().find(|s| &s.id == id)).collect()
    }

    /// Projects the existing same-rail drafts into one editor without selecting, staging or
    /// aligning a DIMM. Auto is only a display state; try_stage still requires Split for Auto/0.
    pub fn get_display(&self, group: &MemoryVoltageGroup, drafts: &Drafts) -> MemoryVoltageDisplay {
        let mixed = || MemoryVoltageDisplay {
            text: "Mixed".into(),
            is_mixed: true,
            can_edit: group.can_sync,
            reason: group.reason.clone(),
        };
        let mut common: Option<f64> = None;
        for member in &group.members {
            let text = drafts.get(&member.id).cloned().unwrap_or_else(|| member.current_text());
            let Some(value) = member.try_parse(&text) else { return mixed() };
            if let Some(previous) = common {
                if (previous - value).abs() >= TOLERANCE {
                    return mixed();
                }
            }
            common = Some(value);
        }
        let Some(target) = common else { return mixed() };
        MemoryVoltageDisplay {
            text: if target == 0.0 { "Auto".into() } else { group.members[0].format(target) },
            is_mixed: false,
            can_edit: group.can_sync,
            reason: group.reason.clone(),
        }
    }

    /// Recognizes DIMM rails only; CPU VDD2 and other voltage controls remain independent.
    pub fn try_get_rail(setting: &Setting) -> Option<String> {
        if setting.group != SettingGroup::Memory || setting.unit != "V" {
            return None;
        }
        let separator = setting.id.rfind('_')?;
        if separator <= 4 {
            return None;
        }
        let dimm = &setting.id[..separator];
        if !dimm.to_lowercase().starts_with("dimm") || !dimm[4..].chars().all(|c| c.is_alphanumeric()) {
            return None;
        }
        let suffix = setting.id[separator + 1..].to_uppercase();
        if !matches!(suffix.as_str(), "VDD" | "VDDQ" | "VPP") {
            return None;
        }
        Some(suffix)
    }

    pub fn get_group(&self, settings: &[Setting], source_id: &str) -> Option<MemoryVoltageGroup> {
        let linked = self.linked(settings);
        let source = linked.iter().find(|s| s.id == source_id)?;
        let rail = Self::try_get_rail(source)?;
        let members: Vec<Setting> = linked
            .iter()
            .filter(|s| Self::try_get_rail(s).as_deref() == Some(rail.as_str()))
            .map(|s| (*s).clone())
            .collect();
        let blocked = |reason: String| MemoryVoltageGroup {
            rail: rail.clone(),
            members: members.clone(),
            supported_values: Vec::new(),
            can_sync: false,
            reason: Some(reason),
        };
        if members.len() < 2 {
            return Some(blocked("At least two detected DIMMs are needed.".into()));
        }
        let mut seen = std::collections::HashSet::new();
        if !members.iter().all(|s| seen.insert(s.id.to_lowercase())) {
            return Some(blocked("Duplicate DIMM rail identifiers prevent safe linking.".into()));
        }
        for member in &members {
            if !member.available {
                return Some(blocked(format!("{} is unavailable; use Split DIMMs for independent edits.", member.name)));
            }
            if member.read_only() {
                return Some(blocked(format!("{} is read-only; use Split DIMMs for independent edits.", member.name)));
            }
            match self.grid(&member.id) {
                Some(g) if g.step_millivolts > 0 && g.base_millivolts > 0 => {}
                _ => {
                    return Some(blocked(format!(
                        "{} has no verified PMIC step; use Split DIMMs for independent edits.",
                        member.name
                    )));
                }
            }
            if !member.min.is_finite() || !member.max.is_finite() || member.min > member.max {
                return Some(blocked(format!("{} has no valid voltage range.", member.name)));
            }
        }

        // There are only 128 possible PMIC register codes. Enumerating the first DIMM's grid
        // avoids rounding a request differently on two PMICs with different calibrated steps.
        let first = self.grid(&members[0].id).unwrap();
        let mut common = Vec::new();
        for code in 0..=first.max_code.max(0) as i64 {
            let value = (first.base_millivolts as i64 + code * first.step_millivolts as i64) as f64 / 1000.0;
            if members.iter().all(|s| self.is_supported(s, value)) {
                common.push(value);
            }
        }
        if common.is_empty() {
            return Some(blocked("The DIMMs have no common supported target for this rail.".into()));
        }
        Some(MemoryVoltageGroup { rail, members, supported_values: common, can_sync: true, reason: None })
    }

    /// Returns text updates without applying them. In Sync, every corresponding DIMM must support
    /// the exact target; no clamping, rounding, partial group updates or cross-rail linking occurs.
    pub fn try_stage(&self, settings: &[Setting], source_id: &str, text: &str) -> Result<Drafts, Option<String>> {
        let Some(group) = self.get_group(settings, source_id) else {
            return Err(Some("This is not a detected DIMM voltage setting.".into()));
        };
        let source = group.members.iter().find(|s| s.id == source_id).unwrap();
        if !source.available || source.read_only() {
            return Err(Some(format!("{} is not writable.", source.name)));
        }

        if !self.is_synced {
            // Split retains the existing parser and Apply validation; text staging changes no hardware.
            return Ok(Drafts::from([(source.id.clone(), text.to_string())]));
        }
        if !group.can_sync {
            return Err(group.reason.clone());
        }
        let Some(value) = source.try_parse(text) else {
            return Err(Some(format!("Enter a valid {} target.", group.rail)));
        };
        if value == 0.0 {
            return Err(Some("Use Split DIMMs to restore individual startup values; those values may differ.".into()));
        }
        if !group.supported_values.iter().any(|v| (v - value).abs() < TOLERANCE) {
            // Reason first: the window shows this in a one-line footer that cuts off the tail.
            let (min, max) = (group.minimum().unwrap_or(0.0), group.maximum().unwrap_or(0.0));
            let step = group.step().map(|s| format!(" in {} mV steps", trim_3(s * 1000.0))).unwrap_or_default();
            let why = if value > max + TOLERANCE {
                format!("{value:.3} V is above the {max:.3} V maximum")
            } else if value < min - TOLERANCE {
                format!("{value:.3} V is below the {min:.3} V minimum")
            } else {
                format!("{value:.3} V is not a supported step")
            };
            return Err(Some(format!("{}: {why} ({min:.3}-{max:.3} V{step}).", group.rail)));
        }
        Ok(group.members.iter().map(|s| (s.id.clone(), s.format(value))).collect())
    }

    /// Validates dirty Sync groups before the caller's explicit Apply. Untouched mixed readings
    /// remain intact, so changing mode alone does not prevent applying an unrelated control.
    pub fn validate_for_apply(&self, settings: &[Setting], targets: &Drafts) -> Result<(), Option<String>> {
        if !self.is_synced {
            return Ok(());
        }
        let linked = self.linked(settings);
        let mut rails_seen: Vec<String> = Vec::new();
        for source in linked {
            let rail = Self::try_get_rail(source).unwrap_or_default();
            if rails_seen.contains(&rail) {
                continue;
            }
            rails_seen.push(rail);
            let group = self.get_group(settings, &source.id).unwrap();
            if !group.members.iter().any(|s| targets.get(&s.id).is_some_and(|t| is_edited(s, t))) {
                continue;
            }
            if !group.can_sync {
                return Err(group.reason.clone());
            }
            let mut common: Option<f64> = None;
            for member in &group.members {
                let text = targets.get(&member.id).cloned().unwrap_or_else(|| member.current_text());
                self.try_stage(settings, &member.id, &text)?;
                let value = member.try_parse(&text).unwrap_or(0.0);
                if let Some(previous) = common {
                    if (previous - value).abs() >= TOLERANCE {
                        return Err(Some(format!(
                            "{} targets differ across DIMMs. Edit one target to synchronize this rail, or use Split DIMMs.",
                            group.rail
                        )));
                    }
                }
                common = Some(value);
            }
        }
        Ok(())
    }

    fn is_supported(&self, setting: &Setting, value: f64) -> bool {
        if value < setting.min - TOLERANCE || value > setting.max + TOLERANCE {
            return false;
        }
        let Some(grid) = self.grid(&setting.id) else { return false };
        let code = (value * 1000.0 - grid.base_millivolts as f64) / grid.step_millivolts as f64;
        code >= -TOLERANCE && code <= grid.max_code as f64 + TOLERANCE && (code - code.round()).abs() < TOLERANCE
    }
}

fn is_edited(setting: &Setting, text: &str) -> bool {
    if setting.last_error.is_some() && setting.available && !setting.read_only() {
        return true;
    }
    match (setting.try_parse(text), setting.try_parse(&setting.current_text())) {
        (Some(target), Some(current)) => (target - current).abs() >= TOLERANCE,
        _ => true,
    }
}

/// .NET "0.###": up to three decimals, trailing zeros removed.
pub fn trim_3(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn dimm_row(id: &str, current: f64) -> Setting {
        let mut s = Setting::new(id, format!("DRAM {id}"), SettingGroup::Memory);
        s.unit = "V".into();
        s.min = 1.0;
        s.max = 1.5;
        s.decimals = 3;
        s.write = Some(Arc::new(|_| Ok(())));
        s.current = Some(current);
        s
    }

    fn grids(ids: &[&str], base: i32, step: i32) -> HashMap<String, MemoryVoltageGrid> {
        ids.iter()
            .map(|id| (id.to_string(), MemoryVoltageGrid { base_millivolts: base, step_millivolts: step, max_code: 127 }))
            .collect()
    }

    #[test]
    fn recognises_only_dimm_rails() {
        let mut vdd2 = dimm_row("cpu_vdd2", 1.1);
        vdd2.group = SettingGroup::Board;
        assert_eq!(MemoryVoltageLink::try_get_rail(&dimm_row("dimma1_vddq", 1.1)).as_deref(), Some("VDDQ"));
        assert!(MemoryVoltageLink::try_get_rail(&vdd2).is_none());
        assert!(MemoryVoltageLink::try_get_rail(&dimm_row("dimm_vdd", 1.1)).is_none());
    }

    #[test]
    fn sync_stages_every_member_and_refuses_off_grid() {
        let rows = vec![dimm_row("dimma1_vdd", 1.1), dimm_row("dimmb1_vdd", 1.1)];
        let link = MemoryVoltageLink::new(&rows, &grids(&["dimma1_vdd", "dimmb1_vdd"], 800, 5));
        let staged = link.try_stage(&rows, "dimma1_vdd", "1.250").unwrap();
        assert_eq!(staged.len(), 2);
        assert_eq!(staged["dimmb1_vdd"], "1.250");
        let err = link.try_stage(&rows, "dimma1_vdd", "1.2512").unwrap_err().unwrap();
        assert!(err.contains("is not a supported step") && err.contains("in 5 mV steps)."), "{err}");
        let err = link.try_stage(&rows, "dimma1_vdd", "1.440").unwrap_err().unwrap();
        assert!(err.starts_with("VDD: 1.440 V is above the 1.435 V maximum"), "{err}");
        assert!(link.try_stage(&rows, "dimma1_vdd", "0").is_err());
    }

    #[test]
    fn split_stages_only_the_source() {
        let rows = vec![dimm_row("dimma1_vdd", 1.1), dimm_row("dimmb1_vdd", 1.1)];
        let mut link = MemoryVoltageLink::new(&rows, &grids(&["dimma1_vdd", "dimmb1_vdd"], 800, 5));
        link.set_synced(false);
        let staged = link.try_stage(&rows, "dimma1_vdd", "1.2").unwrap();
        assert_eq!(staged.len(), 1);
    }

    #[test]
    fn display_reports_mixed_and_common_values() {
        let rows = vec![dimm_row("dimma1_vdd", 1.1), dimm_row("dimmb1_vdd", 1.2)];
        let link = MemoryVoltageLink::new(&rows, &grids(&["dimma1_vdd", "dimmb1_vdd"], 800, 5));
        let group = link.get_group(&rows, "dimma1_vdd").unwrap();
        assert!(link.get_display(&group, &Drafts::new()).is_mixed);
        let drafts = Drafts::from([("dimmb1_vdd".to_string(), "1.100".to_string())]);
        assert_eq!(link.get_display(&group, &drafts).text, "1.100");
    }

    #[test]
    fn validate_rejects_differing_dirty_targets() {
        let rows = vec![dimm_row("dimma1_vdd", 1.1), dimm_row("dimmb1_vdd", 1.1)];
        let link = MemoryVoltageLink::new(&rows, &grids(&["dimma1_vdd", "dimmb1_vdd"], 800, 5));
        let targets = Drafts::from([
            ("dimma1_vdd".to_string(), "1.200".to_string()),
            ("dimmb1_vdd".to_string(), "1.250".to_string()),
        ]);
        assert!(link.validate_for_apply(&rows, &targets).is_err());
        let same = Drafts::from([
            ("dimma1_vdd".to_string(), "1.200".to_string()),
            ("dimmb1_vdd".to_string(), "1.200".to_string()),
        ]);
        assert!(link.validate_for_apply(&rows, &same).is_ok());
    }
}
