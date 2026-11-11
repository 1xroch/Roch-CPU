//! Pure text and decision helpers of the main window, kept apart from the drawing code so they
//! can be unit-tested.

use super::model::{AmdInfo, IntelInfo, LiveReadings};
use crate::model::setting::{Setting, SettingGroup};

pub const APP_NAME: &str = "Roch CPU";
/// From Cargo.toml, so the window, the file version resource and `--help` never disagree.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `BclkController.ConfirmAboveMHz`: no ceiling on base clock, but a figure above this is worth
/// a second look. Hard-coded here so the window does not name the hardware module.
pub const BCLK_CONFIRM_ABOVE_MHZ: f64 = 105.0;

pub const YOUTUBE_URL: &str = "https://www.youtube.com/@MateoPcTech";
pub const AUTHOR_URL: &str = "https://x.com/1xroch";
pub const DISCORD_URL: &str = "https://discord.gg/KfzExpKQHB";
pub const PAWNIO_SITE: &str = "pawnio.eu";
pub const PAWNIO_URL: &str = "https://pawnio.eu";

pub const RESET_TIP: &str = "Reset changed available writable settings using their recorded startup values or per-control Auto/default actions; discard staged drafts. AMD power limits use CPU stock defaults and Curve Optimizer returns to zero. This does not reset factory BIOS settings.";
pub const RESET_PROMPT: &str = "Reset changed writable settings to their existing defaults? Voltage overrides may return to Auto.";
pub const MEMORY_HINT: &str = "PMIC targets; same rail only; staged until Apply.";
pub const SHARED_BOX_TIP: &str = "A shared staged target for this rail. Mixed means the DIMM setpoints or drafts differ; no value is chosen automatically.";

/// Section heading for a group, as shown (upper case, like `Theme.SectionTitle`).
pub fn section_title(group: SettingGroup) -> String {
    match group {
        SettingGroup::Clocks => "Clocks",
        SettingGroup::Voltages => "Voltages",
        SettingGroup::Power => "Power",
        SettingGroup::Pbo => "Precision Boost",
        SettingGroup::Memory => "Memory",
        SettingGroup::Board => "Board VRM rails",
    }
    .to_uppercase()
}

/// The row label: the setting name without the long-form qualifiers.
pub fn display_name(s: &Setting) -> String {
    let mut name = s
        .name
        .replace(" (Package Power Tracking)", "")
        .replace(" (Thermal Design Current)", "")
        .replace(" (Electrical Design Current)", "")
        .replace(" (Tctl max)", "")
        .replace(" (all cores)", "")
        .replace("DRAM ", "")
        .replace(" Voltage", "");
    if s.id == "core_v" {
        name = "CPU Core Voltage".into();
    }
    if s.group == SettingGroup::Memory && !name.contains("VDDQ") && !name.contains("VPP") {
        name += " VDD";
    }
    name
}

/// The right-hand column when there is no live reading or apply result to show.
pub fn idle_text(s: &Setting) -> &'static str {
    if s.read_only() { "read-only" } else { "" }
}

/// Hover text for a row: its allowed range first, then what the setting does.
pub fn row_tip(s: &Setting) -> Option<String> {
    let range = if s.read_only() {
        None
    } else {
        Some("Range: ".to_string() + &s.range_text().replace("Min:", "").replace(", Max:", " to "))
    };
    match (range, &s.note) {
        (None, note) => note.clone(),
        (Some(r), None) => Some(r),
        (Some(r), Some(n)) => Some(format!("{r}\n\n{n}")),
    }
}

pub fn theme_tip(dark: bool) -> &'static str {
    if dark { "Switch to light mode" } else { "Switch to dark mode" }
}

/// Without PawnIO the SMU power table cannot be read, so the AMD limit rows start as Auto.
pub fn pawnio_note() -> String {
    format!(
        "PPT, TDC, EDC and Thermal Limit show Auto because PawnIO is not installed. Install it from {PAWNIO_SITE}, then restart Roch CPU to see the values set in the BIOS."
    )
}

/// What the box shows for a row: the core target says "Not read" when its gate refused it.
pub fn core_display_text(s: &Setting, core_read_failed: bool) -> String {
    if s.id == "core_v" && (!s.available || core_read_failed) { "Not read".into() } else { s.current_text() }
}

pub fn is_untouched_core_placeholder(s: &Setting, text: &str, core_read_failed: bool) -> bool {
    s.id == "core_v" && text.eq_ignore_ascii_case("Not read") && (!s.available || core_read_failed)
}

pub fn is_memory_target_edited(s: &Setting, text: &str) -> bool {
    if s.last_error.is_some() && s.available && !s.read_only() {
        return true;
    }
    match (s.try_parse(text), s.try_parse(&s.current_text())) {
        (Some(target), Some(current)) => (target - current).abs() >= 0.0000001,
        _ => true,
    }
}

/// Whether a value is above the usual daily-use range and needs a confirmation.
pub fn is_risky(id: &str, value: f64) -> bool {
    match id {
        // No confirmation for any voltage (CPU core, E-core L2, ring, SA, GT, their offsets, or
        // the DIMM rails): removed on request. Range checks, read-back and the rail/ADC
        // verification still run on every write.
        // DIMM VDD / VDDQ / VPP: no confirmation (removed on request). Overclocking kits run
        // above JEDEC every day, so the prompt fired on every Apply; each PMIC write is still
        // range-checked, read back and re-measured against the PMIC's ADC.
        "ppt" => value > 500.0,
        "tdc" | "edc" => value > 800.0,
        "co_all" => value > 15.0,
        // No ceiling on base clock, but it takes the memory controller and the ring with it,
        // so a figure this far out is worth a second look before it is walked to.
        "bclk" => value > BCLK_CONFIRM_ABOVE_MHZ,
        _ => false,
    }
}

pub fn danger_prompt(s: &Setting, value: f64) -> String {
    format!(
        "{} = {} {} is above the usual safe range for daily use and can damage hardware.\n\nApply it anyway?",
        s.name,
        s.format(value),
        s.unit
    )
}

/// The Intel live line under the header.
pub fn live_line(live: &LiveReadings) -> String {
    let opt = |v: Option<i32>| v.map(|r| r.to_string()).unwrap_or_else(|| "?".into());
    format!(
        "Live: P x{} E x{} Ring x{} | VID {} V | {}",
        opt(live.core_ratio),
        opt(live.e_core_ratio),
        opt(live.ring_ratio),
        live.core_vid.map(|v| format!("{v:.3}")).unwrap_or_else(|| "?".into()),
        match live.vcore_vrm {
            Some(rail) => format!("Vcore {rail:.3} V"),
            None => "Vcore unavailable".into(),
        }
    )
}

/// Live draw against an AMD limit row.
pub fn live_text(s: &Setting, now: Option<f64>) -> String {
    match now {
        Some(v) => format!("Live {} {}", s.format(v), s.unit),
        None => idle_text(s).into(),
    }
}

/// Everything the header warns about, from the model's capability flags.
pub struct WarningInputs {
    pub driver_loaded: bool,
    pub oc_locked: bool,
    pub voltage_access_restricted: bool,
    pub mailbox_available: bool,
    pub intel: Option<IntelInfo>,
    pub amd: Option<AmdInfo>,
    pub smu_available: bool,
    pub pbo_allowed: bool,
}

pub fn warnings(w: &WarningInputs) -> Vec<&'static str> {
    let mut warns = Vec::new();
    if !w.driver_loaded {
        warns.push("kernel driver not loaded, nothing can be read or written (see Log)");
    }
    if w.oc_locked {
        warns.push("BIOS OC Lock set: ratio and voltage writes will be rejected");
    }
    if w.voltage_access_restricted {
        warns.push("Windows Hyper-V blocked voltage access (see Log)");
    } else {
        if w.intel.is_some() && !w.mailbox_available {
            warns.push("OC mailbox not responding: voltage rows disabled");
        }
        if w.intel.is_some_and(|i| i.hypervisor_present) {
            warns.push("Hypervisor active: read-back values may not reach hardware (see Log)");
        }
    }
    if w.intel.is_some_and(|i| !i.is_lga1700_family) {
        warns.push("not a known LGA1700 CPU");
    }
    if w.amd.is_some_and(|a| !a.is_supported) {
        warns.push("unknown Zen generation: SMU message numbers assumed");
    }
    if w.amd.is_some() && !w.smu_available {
        warns.push("SMU not responding: PBO rows disabled (see Log)");
    }
    if w.amd.is_some() && w.smu_available && !w.pbo_allowed {
        warns.push("SMU reports PBO unavailable: enable Precision Boost Overdrive in the BIOS or writes will be rejected");
    }
    warns
}

pub fn join_warnings(warns: &[&str]) -> String {
    warns.join("  \u{00B7}  ")
}

/// `HH:mm:ss` in local time.
pub fn clock() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// .NET `int.TryParse` with the default integer style: optional surrounding white space and sign.
pub fn parse_int(text: &str) -> Option<i32> {
    let t = text.trim_matches(|c: char| c.is_whitespace());
    let t = t.strip_prefix('+').filter(|r| !r.starts_with(['+', '-'])).unwrap_or(t);
    t.parse::<i32>().ok()
}

// ---------------------------------------------------------------- Curve Optimizer
/// Checks one Curve Optimizer box: the margin, or the text the row shows instead.
pub fn validate_margin(text: &str, range: i32) -> Result<i32, String> {
    let Some(margin) = parse_int(text.trim()) else { return Err("not a number".into()) };
    if margin < -range || margin > range {
        return Err(format!("outside {}..{range}", -range));
    }
    Ok(margin)
}

pub fn co_fill_status(unread: usize, has_readback: bool) -> String {
    if unread == 0 {
        "Values read from the SMU.".into()
    } else if has_readback {
        format!("{unread} core(s) did not answer; those fields show the last value applied here.")
    } else {
        "No read-back on this SMU; fields show the last value applied here (0 if none).".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn writable(id: &str, name: &str, group: SettingGroup) -> Setting {
        let mut s = Setting::new(id, name, group);
        s.write = Some(Arc::new(|_| Ok(())));
        s
    }

    #[test]
    fn display_names_drop_qualifiers() {
        let s = writable("ppt", "PPT (Package Power Tracking)", SettingGroup::Power);
        assert_eq!(display_name(&s), "PPT");
        let s = writable("thermal", "Thermal Limit (Tctl max)", SettingGroup::Power);
        assert_eq!(display_name(&s), "Thermal Limit");
        let s = writable("core_v", "CPU Core Voltage (override)", SettingGroup::Voltages);
        assert_eq!(display_name(&s), "CPU Core Voltage");
        let s = writable("sa_v", "SA Voltage", SettingGroup::Voltages);
        assert_eq!(display_name(&s), "SA");
        let s = writable("dimma1_vdd", "DRAM DIMMA1 Voltage", SettingGroup::Memory);
        assert_eq!(display_name(&s), "DIMMA1 VDD");
        let s = writable("dimma1_vddq", "DRAM DIMMA1 VDDQ Voltage", SettingGroup::Memory);
        assert_eq!(display_name(&s), "DIMMA1 VDDQ");
        let s = writable("dimma1_vpp", "DRAM DIMMA1 VPP Voltage", SettingGroup::Memory);
        assert_eq!(display_name(&s), "DIMMA1 VPP");
        let s = writable("co_all", "Curve Optimizer (all cores)", SettingGroup::Pbo);
        assert_eq!(display_name(&s), "Curve Optimizer");
    }

    #[test]
    fn section_titles_are_upper_case() {
        assert_eq!(section_title(SettingGroup::Board), "BOARD VRM RAILS");
        assert_eq!(section_title(SettingGroup::Pbo), "PRECISION BOOST");
    }

    #[test]
    fn row_tip_puts_the_range_first() {
        let mut s = writable("pl1", "PL1", SettingGroup::Power);
        s.unit = "W".into();
        s.min = 10.0;
        s.max = 4095.0;
        assert_eq!(row_tip(&s).unwrap(), "Range: 10 W to 4095 W");
        s.note = Some("Note.".into());
        assert_eq!(row_tip(&s).unwrap(), "Range: 10 W to 4095 W\n\nNote.");
        let mut ro = Setting::new("cpu_aux", "CPU AUX", SettingGroup::Board);
        assert_eq!(row_tip(&ro), None);
        ro.note = Some("Read only.".into());
        assert_eq!(row_tip(&ro).as_deref(), Some("Read only."));
        let mut unitless = writable("cpu_ratio", "CPU Ratio", SettingGroup::Clocks);
        unitless.min = 8.0;
        unitless.max = 120.0;
        assert_eq!(row_tip(&unitless).unwrap(), "Range: 8.0 to 120.0");
        assert_eq!(idle_text(&ro), "read-only");
        assert_eq!(idle_text(&unitless), "");
    }

    #[test]
    fn core_placeholder_rules() {
        let mut core = writable("core_v", "CPU Core Voltage", SettingGroup::Voltages);
        core.decimals = 3;
        core.current = Some(1.28);
        assert_eq!(core_display_text(&core, false), "1.280");
        assert_eq!(core_display_text(&core, true), "Not read");
        core.available = false;
        assert_eq!(core_display_text(&core, false), "Not read");
        assert!(is_untouched_core_placeholder(&core, "not read", false));
        core.available = true;
        assert!(!is_untouched_core_placeholder(&core, "Not read", false));
        let sa = writable("sa_v", "SA", SettingGroup::Voltages);
        assert!(!is_untouched_core_placeholder(&sa, "Not read", true));
        assert_eq!(core_display_text(&sa, true), "Auto");
    }

    #[test]
    fn memory_edit_detection() {
        let mut s = writable("dimma1_vdd", "DIMMA1", SettingGroup::Memory);
        s.decimals = 3;
        s.current = Some(1.1);
        assert!(!is_memory_target_edited(&s, "1.100"));
        assert!(is_memory_target_edited(&s, "1.105"));
        assert!(is_memory_target_edited(&s, "Mixed"));
        s.last_error = Some("failed".into());
        assert!(is_memory_target_edited(&s, "1.100"));
    }

    #[test]
    fn risky_thresholds() {
        assert!(!is_risky("core_v", 1.60) && !is_risky("sa_v", 1.50) && !is_risky("sa_off", 300.0));
        assert!(!is_risky("dimma1_vdd", 1.45) && !is_risky("dimmb2_vddq", 1.45) && !is_risky("dimma1_vpp", 2.0));
        assert!(is_risky("ppt", 501.0) && is_risky("edc", 801.0) && !is_risky("tdc", 800.0));
        assert!(is_risky("co_all", 16.0) && !is_risky("co_all", -30.0));
        assert!(is_risky("bclk", 105.01) && !is_risky("bclk", 105.0));
        assert!(!is_risky("pl1", 10000.0));
    }

    #[test]
    fn danger_prompt_text() {
        let mut s = writable("core_v", "CPU Core Voltage", SettingGroup::Voltages);
        s.unit = "V".into();
        s.decimals = 3;
        assert_eq!(
            danger_prompt(&s, 1.5),
            "CPU Core Voltage = 1.500 V is above the usual safe range for daily use and can damage hardware.\n\nApply it anyway?"
        );
    }

    #[test]
    fn live_line_formats_missing_values() {
        let live = LiveReadings { core_ratio: Some(54), ring_ratio: Some(50), core_vid: Some(1.27), ..Default::default() };
        assert_eq!(live_line(&live), "Live: P x54 E x? Ring x50 | VID 1.270 V | Vcore unavailable");
        let live = LiveReadings { vcore_vrm: Some(1.2764), ..Default::default() };
        assert_eq!(live_line(&live), "Live: P x? E x? Ring x? | VID ? V | Vcore 1.276 V");
    }

    #[test]
    fn live_text_for_limit_rows() {
        let mut s = writable("ppt", "PPT", SettingGroup::Power);
        s.unit = "W".into();
        assert_eq!(live_text(&s, Some(39.4)), "Live 39 W");
        assert_eq!(live_text(&s, None), "");
    }

    #[test]
    fn warning_list() {
        let base = WarningInputs {
            driver_loaded: true,
            oc_locked: false,
            voltage_access_restricted: false,
            mailbox_available: true,
            intel: Some(IntelInfo { hypervisor_present: false, is_lga1700_family: true, e_core_count: 8 }),
            amd: None,
            smu_available: false,
            pbo_allowed: true,
        };
        assert!(warnings(&base).is_empty());
        let w = warnings(&WarningInputs {
            driver_loaded: false,
            oc_locked: true,
            mailbox_available: false,
            intel: Some(IntelInfo { hypervisor_present: true, is_lga1700_family: false, e_core_count: 0 }),
            ..base
        });
        assert_eq!(
            join_warnings(&w),
            "kernel driver not loaded, nothing can be read or written (see Log)  \u{00B7}  BIOS OC Lock set: ratio and voltage writes will be rejected  \u{00B7}  OC mailbox not responding: voltage rows disabled  \u{00B7}  Hypervisor active: read-back values may not reach hardware (see Log)  \u{00B7}  not a known LGA1700 CPU"
        );
        let restricted = WarningInputs {
            driver_loaded: true,
            oc_locked: false,
            voltage_access_restricted: true,
            mailbox_available: false,
            intel: Some(IntelInfo { hypervisor_present: true, is_lga1700_family: true, e_core_count: 0 }),
            amd: None,
            smu_available: false,
            pbo_allowed: true,
        };
        assert_eq!(warnings(&restricted), vec!["Windows Hyper-V blocked voltage access (see Log)"]);
        let amd = WarningInputs {
            driver_loaded: true,
            oc_locked: false,
            voltage_access_restricted: false,
            mailbox_available: false,
            intel: None,
            amd: Some(AmdInfo { is_supported: false, ..Default::default() }),
            smu_available: true,
            pbo_allowed: false,
        };
        assert_eq!(
            warnings(&amd),
            vec![
                "unknown Zen generation: SMU message numbers assumed",
                "SMU reports PBO unavailable: enable Precision Boost Overdrive in the BIOS or writes will be rejected"
            ]
        );
        let amd_no_smu = WarningInputs { smu_available: false, amd: Some(AmdInfo { is_supported: true, ..Default::default() }), ..amd };
        assert_eq!(warnings(&amd_no_smu), vec!["SMU not responding: PBO rows disabled (see Log)"]);
    }

    #[test]
    fn integer_parsing_like_dotnet() {
        assert_eq!(parse_int(" 42 "), Some(42));
        assert_eq!(parse_int("-15"), Some(-15));
        assert_eq!(parse_int("+7"), Some(7));
        assert_eq!(parse_int("N/A"), None);
        assert_eq!(parse_int("1.5"), None);
        assert_eq!(parse_int("+-1"), None);
        assert_eq!(parse_int(""), None);
    }

    #[test]
    fn curve_optimizer_margins() {
        assert_eq!(validate_margin(" -20 ", 30), Ok(-20));
        assert_eq!(validate_margin("abc", 30), Err("not a number".into()));
        assert_eq!(validate_margin("-31", 30), Err("outside -30..30".into()));
        assert_eq!(validate_margin("51", 50), Err("outside -50..50".into()));
        assert_eq!(co_fill_status(0, true), "Values read from the SMU.");
        assert_eq!(co_fill_status(2, true), "2 core(s) did not answer; those fields show the last value applied here.");
        assert_eq!(co_fill_status(8, false), "No read-back on this SMU; fields show the last value applied here (0 if none).");
    }

    #[test]
    fn pawnio_note_mentions_the_site_once() {
        let note = pawnio_note();
        assert_eq!(note.matches(PAWNIO_SITE).count(), 1);
        assert!(note.starts_with("PPT, TDC, EDC and Thermal Limit show Auto"));
    }
}
