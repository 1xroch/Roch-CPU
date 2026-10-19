//! Board / BIOS identification from the raw SMBIOS table (no WMI needed), and the
//! `GetSystemFirmwareTable` wrapper shared with the BCLK meter.

use super::native;

/// Board / BIOS identification from the raw SMBIOS table (no WMI needed).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SmbiosInfo {
    pub bios_vendor: String,
    pub bios_version: String,
    pub bios_date: String,
    pub system_manufacturer: String,
    pub system_product: String,
    pub board_manufacturer: String,
    pub board_product: String,
    pub board_version: String,
    /// AMD firmware version, e.g. "ComboAm5PI 1.2.0.3e Patch A". Empty on Intel.
    pub agesa: String,
}

/// `Encoding.ASCII.GetString`: bytes above 0x7F become '?'.
fn ascii(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| if b < 0x80 { b as char } else { '?' }).collect()
}

/// `Regex.Replace(agesa, @"^AGESA!?(V\d+)?\s*", "")` for a string known to start with "AGESA".
fn strip_agesa_prefix(s: &str) -> &str {
    let Some(mut rest) = s.strip_prefix("AGESA") else { return s };
    if let Some(r) = rest.strip_prefix('!') {
        rest = r;
    }
    if let Some(r) = rest.strip_prefix('V') {
        let digits = r.len() - r.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 {
            rest = &r[digits..];
        }
    }
    rest.trim_start()
}

impl SmbiosInfo {
    pub fn mainboard_model(&self) -> String {
        if self.board_product.trim().is_empty() {
            format!("{} {}", self.system_manufacturer, self.system_product).trim().to_string()
        } else {
            format!("{} {}", self.board_manufacturer, self.board_product).trim().to_string()
        }
    }

    pub fn read() -> SmbiosInfo {
        match FirmwareTables::get(FirmwareTables::RSMB, 0) {
            Some(raw) => SmbiosInfo::parse(&raw),
            None => SmbiosInfo::default(),
        }
    }

    /// Parses a `GetSystemFirmwareTable('RSMB')` buffer: an 8-byte RawSMBIOSData header
    /// (calling method, major, minor, DMI revision, u32 table length) followed by the structures.
    pub fn parse(raw: &[u8]) -> SmbiosInfo {
        let mut info = SmbiosInfo::default();
        if raw.len() < 8 {
            return info;
        }
        let length = i32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
        let end = (raw.len() as i64).min(8i32.wrapping_add(length) as i64).max(0) as usize;
        // An index past the buffer (a truncated table) reads as string 0, i.e. empty.
        let at = |i: usize| raw.get(i).copied().unwrap_or(0);
        let mut pos: usize = 8;
        while pos + 4 <= end {
            let typ = raw[pos];
            let len = raw[pos + 1] as usize;
            if len < 4 {
                break;
            }
            let mut p = pos + len;
            let mut strings: Vec<String> = Vec::new();
            while p < end {
                let s = p;
                while p < end && raw[p] != 0 {
                    p += 1;
                }
                // A structure with no strings still ends in two zeros; skipping only one left the walk
                // on the second, read as a zero-length header, and it stopped before type 40.
                if p == s {
                    p += 2;
                    break;
                }
                strings.push(ascii(&raw[s..p]));
                p += 1;
                if p < end && raw[p] == 0 {
                    p += 1;
                    break;
                }
            }
            let str_at = |idx: u8| -> String {
                let idx = idx as usize;
                if idx >= 1 && idx <= strings.len() { strings[idx - 1].trim().to_string() } else { String::new() }
            };
            // Some firmware (AMI on MSI boards, for one) carries a second, empty copy of a
            // structure later in the table; keep the first populated record of each type.
            let has_strings = !strings.is_empty();
            match typ {
                0 if has_strings && info.bios_version.is_empty() => {
                    info.bios_vendor = str_at(at(pos + 4));
                    info.bios_version = str_at(at(pos + 5));
                    info.bios_date = str_at(at(pos + 8));
                }
                1 if has_strings && info.system_product.is_empty() => {
                    info.system_manufacturer = str_at(at(pos + 4));
                    info.system_product = str_at(at(pos + 5));
                }
                2 if has_strings && info.board_product.is_empty() => {
                    info.board_manufacturer = str_at(at(pos + 4));
                    info.board_product = str_at(at(pos + 5));
                    info.board_version = str_at(at(pos + 6));
                }
                // Type 40, Additional Information: AMD firmware lists "AGESA!V9 ComboAm5PI 1.2.0.3e Patch A"
                // among its strings, the same entry Linux prints at boot.
                40 if info.agesa.is_empty() => {
                    if let Some(agesa) = strings.iter().find(|x| x.starts_with("AGESA")) {
                        info.agesa = strip_agesa_prefix(agesa).trim().to_string();
                    }
                }
                127 => return info,
                _ => {}
            }
            pos = p;
        }
        info
    }
}

/// Wrapper for GetSystemFirmwareTable.
pub struct FirmwareTables;

impl FirmwareTables {
    pub const ACPI: u32 = 0x4143_5049; // 'ACPI'
    pub const RSMB: u32 = 0x5253_4D42; // 'RSMB'

    /// Little-endian four-character code, as `BitConverter.ToUInt32(Encoding.ASCII.GetBytes(fourCc), 0)`.
    pub fn signature(four_cc: &str) -> u32 {
        let b = four_cc.as_bytes();
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    pub fn get(provider: u32, id: u32) -> Option<Vec<u8>> {
        native::get_system_firmware_table(provider, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One structure: formatted area (type, length, handle, then `fields`), then its strings.
    fn structure(typ: u8, fields: &[u8], strings: &[&str]) -> Vec<u8> {
        let mut v = vec![typ, (4 + fields.len()) as u8, 0, 0];
        v.extend_from_slice(fields);
        if strings.is_empty() {
            v.extend_from_slice(&[0, 0]);
        } else {
            for s in strings {
                v.extend_from_slice(s.as_bytes());
                v.push(0);
            }
            v.push(0);
        }
        v
    }

    fn table(structures: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = structures.concat();
        let mut raw = vec![0, 3, 6, 0];
        raw.extend_from_slice(&(body.len() as u32).to_le_bytes());
        raw.extend(body);
        raw
    }

    #[test]
    fn parses_bios_system_board_and_agesa() {
        let raw = table(&[
            // Type 0: vendor=1, version=2, start segment (2 bytes), date=3
            structure(0, &[1, 2, 0, 0, 3, 0], &["American Megatrends International, LLC.", " 1.A0 ", "05/12/2025"]),
            // A structure without strings (ends in two zeros) must not stop the walk.
            structure(32, &[0, 0, 0, 0, 0, 0, 0], &[]),
            structure(1, &[1, 2, 0, 0], &["Micro-Star International Co., Ltd.", "MS-7E06"]),
            structure(2, &[1, 2, 3, 0], &["Micro-Star International Co., Ltd.", "MAG X870 TOMAHAWK WIFI (MS-7E51)", "1.0"]),
            // Second, empty copy of type 2: ignored.
            structure(2, &[0, 0, 0, 0], &[]),
            structure(40, &[1, 0], &["Something else", "AGESA!V9 ComboAm5PI 1.2.0.3e Patch A"]),
            structure(127, &[], &[]),
            // Past the end-of-table marker: never read.
            structure(1, &[1, 2, 0, 0], &["Other", "Other"]),
        ]);
        let info = SmbiosInfo::parse(&raw);
        assert_eq!(info.bios_vendor, "American Megatrends International, LLC.");
        assert_eq!(info.bios_version, "1.A0");
        assert_eq!(info.bios_date, "05/12/2025");
        assert_eq!(info.system_manufacturer, "Micro-Star International Co., Ltd.");
        assert_eq!(info.system_product, "MS-7E06");
        assert_eq!(info.board_manufacturer, "Micro-Star International Co., Ltd.");
        assert_eq!(info.board_product, "MAG X870 TOMAHAWK WIFI (MS-7E51)");
        assert_eq!(info.board_version, "1.0");
        assert_eq!(info.agesa, "ComboAm5PI 1.2.0.3e Patch A");
        assert_eq!(info.mainboard_model(), "Micro-Star International Co., Ltd. MAG X870 TOMAHAWK WIFI (MS-7E51)");
    }

    #[test]
    fn mainboard_model_falls_back_to_system_and_short_tables_are_empty() {
        let raw = table(&[structure(1, &[1, 2, 0, 0], &["ASUSTeK COMPUTER INC.", "System Product Name"])]);
        let info = SmbiosInfo::parse(&raw);
        assert_eq!(info.mainboard_model(), "ASUSTeK COMPUTER INC. System Product Name");
        assert_eq!(SmbiosInfo::parse(&[0, 3, 6]), SmbiosInfo::default());
        // A header whose length runs past the buffer stops at the buffer end.
        let mut long = table(&[structure(2, &[1, 2, 0, 0], &["ASUSTeK", "ROG"])]);
        long[4..8].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(SmbiosInfo::parse(&long).board_product, "ROG");
    }

    #[test]
    fn agesa_prefix_variants() {
        assert_eq!(strip_agesa_prefix("AGESA!V9 ComboAm5PI 1.2.0.3e"), "ComboAm5PI 1.2.0.3e");
        assert_eq!(strip_agesa_prefix("AGESA V12 X"), "V12 X");
        assert_eq!(strip_agesa_prefix("AGESAV9 X"), "X");
        assert_eq!(strip_agesa_prefix("AGESA!VX"), "VX");
        assert_eq!(FirmwareTables::signature("FACP"), 0x5043_4146);
    }
}
