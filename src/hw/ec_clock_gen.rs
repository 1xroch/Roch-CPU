//! The external base-clock generator, reached over [`EcMailbox`].

use anyhow::{Result, bail};

use super::ec_mailbox::EcMailbox;
use super::super_io::SuperIo;

/// Written as a word with the value 1 before each burst. The vendor tool does this before
/// every read group and again before every write group, so it is treated as required rather
/// than incidental; it is most likely the register-window enable.
const R_CLKGEN_UNLOCK: u8 = 0xFD;

/// The external base-clock generator, reached over [`EcMailbox`].
///
/// The BCLK on a Raptor Lake board does not come from the CPU: it comes from a small synthesiser
/// on the board, an I2C device at the CK505-style address 0xD2 (7-bit 0x69), on the bus the EC
/// owns. That is why base clock could be measured but never set until the mailbox was found.
pub struct EcClockGen {
    mb: EcMailbox,
}

impl EcClockGen {
    /// The clock generator's I2C address in the 8-bit form the mailbox wants.
    pub const CLOCK_GEN_ADDRESS: u8 = 0xD2;

    /// The clock generator's frequency block: eight bytes at 0xE0.
    pub const BLOCK_BASE: u8 = 0xE0;
    pub const BLOCK_LENGTH: usize = 8;

    pub fn new(mailbox: EcMailbox) -> EcClockGen {
        EcClockGen { mb: mailbox }
    }

    pub fn is_supported(sio: Option<&SuperIo>) -> bool {
        EcMailbox::is_supported(sio)
    }

    fn unlock(&self) -> Result<bool> {
        self.mb.write_word(Self::CLOCK_GEN_ADDRESS, R_CLKGEN_UNLOCK, 1)
    }

    // ---------------------------------------------------------------- frequency block
    /// Reads the eight-byte frequency block. Each register is read as a 16-bit word covering
    /// itself and the byte after it, so consecutive reads overlap by one byte; the overlap is
    /// used as a consistency check, because a mailbox that returned stale data would break it.
    pub fn read_block(&self) -> Result<Option<Vec<u8>>> {
        if !self.unlock()? {
            return Ok(None);
        }
        let mut bytes = vec![0u8; Self::BLOCK_LENGTH + 1];
        for i in 0..Self::BLOCK_LENGTH {
            let Some(w) = self.mb.read_word(Self::CLOCK_GEN_ADDRESS, Self::BLOCK_BASE + i as u8)? else {
                return Ok(None);
            };
            let lo = w as u8;
            let hi = (w >> 8) as u8;
            if i > 0 && bytes[i] != lo {
                return Ok(None); // overlap disagreed - do not trust the read
            }
            bytes[i] = lo;
            bytes[i + 1] = hi;
        }
        bytes.truncate(Self::BLOCK_LENGTH);
        Ok(Some(bytes))
    }

    /// Writes the eight-byte frequency block the way the vendor tool does: a 16-bit write at each
    /// of the eight register indices in ascending order, each carrying its own byte and the next
    /// one. The trailing byte past the block is written as zero, which is what the capture shows.
    ///
    /// **The order is load-bearing.** The bytes go into shadow registers and the part
    /// latches the divider from them when the last one arrives, so writing 0xE0 upwards makes the
    /// change take effect all at once - there is no moment where a half-old, half-new divider is
    /// driving the clock. Writing the fraction before the integer looks safer if you assume each
    /// byte takes effect as it lands, but it leaves the integer written and never latched, and the
    /// clock quietly runs a whole count off. Measured, twice. Do not reorder this loop.
    pub fn write_block(&self, block: &[u8]) -> Result<bool> {
        if block.len() != Self::BLOCK_LENGTH {
            bail!("need {} bytes", Self::BLOCK_LENGTH);
        }
        if !self.unlock()? {
            return Ok(false);
        }
        for i in 0..Self::BLOCK_LENGTH {
            let next = if i + 1 < Self::BLOCK_LENGTH { block[i + 1] } else { 0 };
            if !self.mb.write_word(Self::CLOCK_GEN_ADDRESS, Self::BLOCK_BASE + i as u8, block[i] as u16 | ((next as u16) << 8))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    // ---------------------------------------------------------------- block layout
    /// The block holds one fractional-N divider, and the base clock is a fixed oscillator divided
    /// by it:
    /// `  f = Fvco / (block[0] + tuning / 2^28)      Fvco is about 10002 MHz`
    /// Byte 0 is the integer part and bytes 4..7 are a 28-bit fraction, little-endian; the top
    /// four bits of that word are ignored by the part, and bytes 1..3 read as zero. Both fields
    /// therefore run *backwards* against frequency - a smaller divider is a faster clock.
    ///
    /// Measured across eleven points spanning 100.0 to 103.1 MHz, that relation holds to a
    /// thousandth of a MHz. It is worth saying how far the capture alone would have got this
    /// wrong: read on its own, the vendor tool's recorded ramp steps the fraction down, which
    /// looks like an ordinary overclock ramp of a frequency field until it is anchored against a
    /// measurement. It is actually a divider, and the ramp was at 103.1 MHz and climbing.
    /// `BclkController` measures Fvco on the hardware rather than trusting a constant.
    pub fn get_tuning(block: &[u8]) -> u32 {
        block[4] as u32 | ((block[5] as u32) << 8) | ((block[6] as u32) << 16) | ((block[7] as u32) << 24)
    }

    pub fn with_tuning(block: &[u8], tuning: u32) -> Vec<u8> {
        let mut copy = block.to_vec();
        copy[4] = tuning as u8;
        copy[5] = (tuning >> 8) as u8;
        copy[6] = (tuning >> 16) as u8;
        copy[7] = (tuning >> 24) as u8;
        copy
    }

    pub fn describe(block: &[u8]) -> String {
        let hex: Vec<String> = block.iter().map(|b| format!("{b:02X}")).collect();
        let tuning = Self::get_tuning(block);
        format!("{}   flags {:02X} tuning {tuning} (0x{tuning:08X})", hex.join(" "), block[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuning_round_trips_and_describes() {
        let block = [0x64u8, 0, 0, 0, 0x78, 0x56, 0x34, 0x12];
        assert_eq!(EcClockGen::get_tuning(&block), 0x1234_5678);
        let changed = EcClockGen::with_tuning(&block, 0x0ABC_DEF0);
        assert_eq!(changed, vec![0x64, 0, 0, 0, 0xF0, 0xDE, 0xBC, 0x0A]);
        assert_eq!(
            EcClockGen::describe(&block),
            "64 00 00 00 78 56 34 12   flags 64 tuning 305419896 (0x12345678)"
        );
    }
}
