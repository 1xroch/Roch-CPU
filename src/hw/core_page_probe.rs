//! TODO: port pending (types below are the agreed public shape).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CorePageSnapshot {
    pub mode_register: u16,
    pub target_mv: u16,
    pub secondary_target_mv: u16,
}

impl CorePageSnapshot {
    pub fn mode(&self) -> i32 {
        ((self.mode_register >> 4) & 3) as i32
    }
}

#[derive(Clone, Debug)]
pub struct CorePageVerificationResult {
    pub succeeded: bool,
    pub original_page: Option<u8>,
    pub restored_page: Option<u8>,
    pub snapshot: Option<CorePageSnapshot>,
    pub status: String,
    pub page_selection_attempted: bool,
    pub page_restoration_attempted: bool,
    pub diagnostic_lines: Vec<String>,
}
