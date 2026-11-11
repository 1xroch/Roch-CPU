//! AMD System Management Unit access: SMN registers through the north bridge index/data pair,
//! the RSMU / MP1 mailboxes, and the power table the SMU publishes in DRAM.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};

use super::KernelDriver;
use super::native::{self, OwnedHandle, lock};
use super::pawnio::PawnIo;

/// Result code of an SMU mailbox transaction. 0x01..0xFF are the SMU's own; the rest are ours.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SmuStatus {
    Ok,
    Failed,
    UnknownCommand,
    RejectedPrerequisite,
    RejectedBusy,
    NotImplemented,
    MutexTimeout,
    MailboxBusy,
    NoResponse,
    PciError,
    /// Any other status byte the SMU answered with (the C# enum can hold any byte value).
    Other(u8),
}

impl SmuStatus {
    /// The status byte (`(byte)status` in C#).
    pub const fn code(self) -> u8 {
        match self {
            SmuStatus::Ok => 0x01,
            SmuStatus::Failed => 0xFF,
            SmuStatus::UnknownCommand => 0xFE,
            SmuStatus::RejectedPrerequisite => 0xFD,
            SmuStatus::RejectedBusy => 0xFC,
            SmuStatus::NotImplemented => 0x20,
            SmuStatus::MutexTimeout => 0x30,
            SmuStatus::MailboxBusy => 0x31,
            SmuStatus::NoResponse => 0x32,
            SmuStatus::PciError => 0x33,
            SmuStatus::Other(b) => b,
        }
    }

    /// The status for a byte (`(SmuStatus)b` in C#).
    pub const fn from_code(b: u8) -> Self {
        match b {
            0x01 => SmuStatus::Ok,
            0xFF => SmuStatus::Failed,
            0xFE => SmuStatus::UnknownCommand,
            0xFD => SmuStatus::RejectedPrerequisite,
            0xFC => SmuStatus::RejectedBusy,
            0x20 => SmuStatus::NotImplemented,
            0x30 => SmuStatus::MutexTimeout,
            0x31 => SmuStatus::MailboxBusy,
            0x32 => SmuStatus::NoResponse,
            0x33 => SmuStatus::PciError,
            other => SmuStatus::Other(other),
        }
    }
}

/// Register triple of one SMU mailbox in SMN space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmuMailbox {
    pub name: &'static str,
    pub msg: u32,
    pub rsp: u32,
    pub arg: u32,
}

impl SmuMailbox {
    pub const fn new(name: &'static str, msg: u32, rsp: u32, arg: u32) -> Self {
        Self { name, msg, rsp, arg }
    }

    pub const fn is_valid(&self) -> bool {
        self.msg != 0 && self.rsp != 0 && self.arg != 0
    }
}

/// The message numbers one SMU firmware family understands. Zero means "this generation has no
/// such message" and the matching control is not offered. Numbers follow ZenStates-Core
/// (Ivan Rusanov); the ones this build was tested against are marked in the comments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmuMessageSet {
    pub family: &'static str,
    pub rsmu: SmuMailbox,
    pub mp1: SmuMailbox,
    /// Host System Management Port: dead on desktop parts, offered for the raw command line only.
    pub hsmp: SmuMailbox,
    pub is_apu: bool,
    /// Curve Optimizer range: 30 before Zen 4, 50 from Zen 4 on.
    pub co_range: i32,
    /// Zen 2 addresses a core as CCD/CCX/core; Zen 3 and later as CCD/core.
    pub ccx_in_core_mask: bool,

    pub rsmu_set_ppt: u32,
    pub rsmu_set_tdc: u32,
    pub rsmu_set_edc: u32,
    pub rsmu_set_tctl_max: u32,
    pub rsmu_set_scalar: u32,
    pub rsmu_get_scalar: u32,
    pub rsmu_set_co_margin: u32,
    pub rsmu_set_co_margin_all: u32,
    pub rsmu_get_co_margin: u32,
    /// The CPU's stock limits. ZenStates labels 0xDC / 0xDB / 0xD9 fused power / VDD TDC / SoC TDC; on a
    /// 9850X3D they answer 162000 / 120000 / 180000 for a part whose stock PPT / TDC / EDC are
    /// 162 W / 120 A / 180 A, so here 0xD9 is PPT, 0xDB TDC, 0xDC EDC. They do not track a written limit.
    pub rsmu_get_stock_ppt: u32,
    pub rsmu_get_stock_tdc: u32,
    pub rsmu_get_stock_edc: u32,
    pub rsmu_is_overclockable: u32,
    pub rsmu_get_boost_limit: u32,
    pub rsmu_set_boost_limit_all: u32,
    pub rsmu_get_dram_base: u32,
    pub rsmu_get_table_version: u32,
    pub rsmu_get_sustained_power: u32,
    pub mp1_set_ppt: u32,
    pub mp1_set_tdc: u32,
    pub mp1_set_edc: u32,
    pub mp1_set_scalar: u32,
    pub mp1_set_pbo_enable: u32,
    pub mp1_set_co_margin: u32,
    pub mp1_set_co_margin_all: u32,
    pub mp1_get_sustained_power: u32,
}

const RSMU_ZEN1: SmuMailbox = SmuMailbox::new("RSMU", 0x03B1051C, 0x03B10568, 0x03B10590);
const MP1_ZEN1: SmuMailbox = SmuMailbox::new("MP1", 0x03B10528, 0x03B10564, 0x03B10598);
const RSMU_ZEN2: SmuMailbox = SmuMailbox::new("RSMU", 0x03B10524, 0x03B10570, 0x03B10A40);
const MP1_ZEN2: SmuMailbox = SmuMailbox::new("MP1", 0x03B10530, 0x03B1057C, 0x03B109C4);
const HSMP_ZEN2: SmuMailbox = SmuMailbox::new("HSMP", 0x03B10534, 0x03B10980, 0x03B109E0);
const HSMP_ZEN5: SmuMailbox = SmuMailbox::new("HSMP", 0x03B10934, 0x03B10980, 0x03B109E0);
const RSMU_SHIMADA: SmuMailbox = SmuMailbox::new("RSMU", 0x03B10924, 0x03B10970, 0x03B10A40);
const RSMU_APU: SmuMailbox = SmuMailbox::new("RSMU", 0x03B10A20, 0x03B10A80, 0x03B10A88);
const MP1_APU: SmuMailbox = SmuMailbox::new("MP1", 0x03B10528, 0x03B10564, 0x03B10998);
const MP1_PHOENIX: SmuMailbox = SmuMailbox::new("MP1", 0x03B10528, 0x03B10578, 0x03B10998);
const MP1_STRIX: SmuMailbox = SmuMailbox::new("MP1", 0x03B10928, 0x03B10978, 0x03B10998);
const MAILBOX_NONE: SmuMailbox = SmuMailbox::new("none", 0, 0, 0);

/// Every message 0, the C# property defaults (`Hsmp` = an all-zero "HSMP" mailbox, `CoRange` 30).
const BASE: SmuMessageSet = SmuMessageSet {
    family: "",
    rsmu: MAILBOX_NONE,
    mp1: MAILBOX_NONE,
    hsmp: SmuMailbox::new("HSMP", 0, 0, 0),
    is_apu: false,
    co_range: 30,
    ccx_in_core_mask: false,
    rsmu_set_ppt: 0,
    rsmu_set_tdc: 0,
    rsmu_set_edc: 0,
    rsmu_set_tctl_max: 0,
    rsmu_set_scalar: 0,
    rsmu_get_scalar: 0,
    rsmu_set_co_margin: 0,
    rsmu_set_co_margin_all: 0,
    rsmu_get_co_margin: 0,
    rsmu_get_stock_ppt: 0,
    rsmu_get_stock_tdc: 0,
    rsmu_get_stock_edc: 0,
    rsmu_is_overclockable: 0,
    rsmu_get_boost_limit: 0,
    rsmu_set_boost_limit_all: 0,
    rsmu_get_dram_base: 0,
    rsmu_get_table_version: 0,
    rsmu_get_sustained_power: 0,
    mp1_set_ppt: 0,
    mp1_set_tdc: 0,
    mp1_set_edc: 0,
    mp1_set_scalar: 0,
    mp1_set_pbo_enable: 0,
    mp1_set_co_margin: 0,
    mp1_set_co_margin_all: 0,
    mp1_get_sustained_power: 0,
};

impl SmuMessageSet {
    pub const fn has_power_limits(&self) -> bool {
        self.rsmu_set_ppt != 0 || self.mp1_set_ppt != 0
    }
    pub const fn has_current_limits(&self) -> bool {
        (self.rsmu_set_tdc != 0 || self.mp1_set_tdc != 0) && (self.rsmu_set_edc != 0 || self.mp1_set_edc != 0)
    }
    pub const fn has_scalar(&self) -> bool {
        self.rsmu_set_scalar != 0 || self.mp1_set_scalar != 0
    }
    pub const fn has_curve_optimizer(&self) -> bool {
        self.rsmu_set_co_margin != 0 || self.mp1_set_co_margin != 0
    }
    pub const fn has_curve_optimizer_readback(&self) -> bool {
        self.rsmu_get_co_margin != 0
    }
    pub const fn has_boost_limit(&self) -> bool {
        self.rsmu_get_boost_limit != 0
    }

    /// Summit Ridge, Naples, Whitehaven.
    pub const ZEN: SmuMessageSet = SmuMessageSet {
        family: "Zen",
        rsmu: RSMU_ZEN1,
        mp1: MP1_ZEN1,
        ccx_in_core_mask: true,
        rsmu_set_ppt: 0x58,
        mp1_set_ppt: 0x31,
        mp1_set_tdc: 0x29,
        mp1_set_edc: 0x2A,
        rsmu_get_dram_base: 0xC,
        rsmu_get_table_version: 0xD,
        rsmu_get_sustained_power: 0x5F,
        mp1_get_sustained_power: 0x36,
        ..BASE
    };

    /// Pinnacle Ridge, Colfax.
    pub const ZEN_PLUS: SmuMessageSet = SmuMessageSet {
        family: "Zen+",
        rsmu: RSMU_ZEN1,
        mp1: MP1_ZEN1,
        ccx_in_core_mask: true,
        rsmu_set_ppt: 0x64,
        rsmu_set_tdc: 0x65,
        rsmu_set_edc: 0x66,
        rsmu_set_tctl_max: 0x68,
        rsmu_set_scalar: 0x6A,
        rsmu_get_scalar: 0x6F,
        rsmu_get_dram_base: 0xC,
        rsmu_get_table_version: 0xD,
        ..BASE
    };

    /// Matisse, Castle Peak, Rome.
    pub const ZEN2: SmuMessageSet = SmuMessageSet {
        family: "Zen 2",
        rsmu: RSMU_ZEN2,
        mp1: MP1_ZEN2,
        hsmp: HSMP_ZEN2,
        ccx_in_core_mask: true,
        rsmu_set_ppt: 0x53,
        rsmu_set_tdc: 0x54,
        rsmu_set_edc: 0x55,
        rsmu_set_tctl_max: 0x56,
        rsmu_set_scalar: 0x58,
        rsmu_get_scalar: 0x6C,
        rsmu_is_overclockable: 0x6F,
        rsmu_get_boost_limit: 0x6E,
        rsmu_get_dram_base: 0x6,
        rsmu_get_table_version: 0x8,
        mp1_set_ppt: 0x3C,
        mp1_set_tdc: 0x3A,
        mp1_set_edc: 0x3B,
        mp1_set_scalar: 0x2F,
        mp1_set_pbo_enable: 0x33,
        mp1_set_co_margin: 0x34,
        mp1_set_co_margin_all: 0x35,
        mp1_get_sustained_power: 0x23,
        ..BASE
    };

    /// Vermeer, Chagall, Milan.
    pub const ZEN3: SmuMessageSet = SmuMessageSet {
        family: "Zen 3",
        rsmu: RSMU_ZEN2,
        mp1: MP1_ZEN2,
        hsmp: HSMP_ZEN2,
        rsmu_set_ppt: 0x53,
        rsmu_set_tdc: 0x54,
        rsmu_set_edc: 0x55,
        rsmu_set_tctl_max: 0x56,
        rsmu_set_scalar: 0x58,
        rsmu_get_scalar: 0x6C,
        rsmu_is_overclockable: 0x6F,
        rsmu_get_boost_limit: 0x6E,
        rsmu_set_co_margin: 0xA,
        rsmu_set_co_margin_all: 0xB,
        rsmu_get_co_margin: 0x7C,
        rsmu_get_dram_base: 0x6,
        rsmu_get_table_version: 0x8,
        mp1_set_ppt: 0x3D,
        mp1_set_tdc: 0x3B,
        mp1_set_edc: 0x3C,
        mp1_set_scalar: 0x2F,
        mp1_set_pbo_enable: 0x33,
        mp1_set_co_margin: 0x35,
        mp1_set_co_margin_all: 0x36,
        mp1_get_sustained_power: 0x23,
        ..BASE
    };

    /// Zen 4 and Zen 5 share one numbering; only the mailboxes and the read-back message differ. Tested on a 9850X3D.
    const fn zen4_family(
        family: &'static str,
        rsmu: SmuMailbox,
        mp1: SmuMailbox,
        hsmp: SmuMailbox,
        get_co_margin: u32,
    ) -> SmuMessageSet {
        SmuMessageSet {
            family,
            rsmu,
            mp1,
            hsmp,
            co_range: 50,
            rsmu_set_ppt: 0x56,
            rsmu_set_tdc: 0x57,
            rsmu_set_edc: 0x58,
            rsmu_set_tctl_max: 0x59,
            rsmu_set_scalar: 0x5B,
            rsmu_get_scalar: 0x6D,
            rsmu_is_overclockable: 0x6F,
            rsmu_get_boost_limit: 0x6E,
            rsmu_set_boost_limit_all: 0x70,
            rsmu_set_co_margin: 0x6,
            rsmu_set_co_margin_all: 0x7,
            rsmu_get_co_margin: get_co_margin,
            rsmu_get_stock_ppt: 0xD9,
            rsmu_get_stock_tdc: 0xDB,
            rsmu_get_stock_edc: 0xDC,
            rsmu_get_dram_base: 0x4,
            rsmu_get_table_version: 0x5,
            mp1_set_ppt: 0x3E,
            mp1_set_tdc: 0x3C,
            mp1_set_edc: 0x3D,
            mp1_set_scalar: 0x2F,
            mp1_set_co_margin: 0x35,
            mp1_set_co_margin_all: 0x36,
            mp1_get_sustained_power: 0x23,
            ..BASE
        }
    }

    /// Raphael, Dragon Range, Genoa, Storm Peak.
    pub const ZEN4: SmuMessageSet = Self::zen4_family("Zen 4", RSMU_ZEN2, MP1_ZEN2, HSMP_ZEN2, 0xD5);
    /// Granite Ridge, Turin, Bergamo.
    pub const ZEN5: SmuMessageSet = Self::zen4_family("Zen 5", RSMU_ZEN2, MP1_ZEN2, HSMP_ZEN5, 0xD5);
    /// Shimada Peak (Threadripper 9000): RSMU moved, MP1 unknown, different read-back message.
    pub const SHIMADA_PEAK: SmuMessageSet =
        Self::zen4_family("Zen 5 (Shimada Peak)", RSMU_SHIMADA, MAILBOX_NONE, MAILBOX_NONE, 0xA3);

    /// Renoir, Lucienne, Cezanne, Van Gogh, Mendocino. Numbers per ZenStates, untested here.
    pub const fn apu(family: &'static str, get_co_margin: u32) -> SmuMessageSet {
        SmuMessageSet {
            family,
            rsmu: RSMU_APU,
            mp1: MP1_APU,
            is_apu: true,
            rsmu_set_ppt: 0x32,
            rsmu_set_tdc: 0x38,
            rsmu_set_edc: 0x3A,
            rsmu_set_tctl_max: 0x37,
            rsmu_set_scalar: 0x3F,
            rsmu_get_scalar: 0xF,
            rsmu_is_overclockable: 0x82,
            rsmu_get_boost_limit: 0x42,
            rsmu_set_co_margin: 0x52,
            rsmu_set_co_margin_all: 0xB1,
            rsmu_get_co_margin: get_co_margin,
            rsmu_get_stock_ppt: 0x13,
            rsmu_get_stock_tdc: 0x15,
            rsmu_get_dram_base: 0x66,
            rsmu_get_table_version: 0x6,
            mp1_set_ppt: 0x15,
            mp1_set_tdc: 0x1A,
            mp1_set_edc: 0x1C,
            mp1_set_scalar: 0x49,
            mp1_set_co_margin: 0x54,
            mp1_set_co_margin_all: 0x55,
            mp1_get_sustained_power: 0x5B,
            ..BASE
        }
    }

    /// Rembrandt, Phoenix, Hawk Point, Strix Point, Strix Halo, Krackan Point. Numbers per ZenStates, untested here.
    pub const fn apu_phoenix(family: &'static str, get_co_margin: u32, strix: bool) -> SmuMessageSet {
        SmuMessageSet {
            family,
            rsmu: RSMU_APU,
            mp1: if strix { MP1_STRIX } else { MP1_PHOENIX },
            is_apu: true,
            co_range: 50,
            rsmu_set_ppt: 0x32,
            rsmu_set_tdc: 0x38,
            rsmu_set_edc: 0x3A,
            rsmu_set_tctl_max: 0x37,
            rsmu_set_scalar: 0x3E,
            rsmu_get_scalar: 0xF,
            rsmu_is_overclockable: 0x82,
            rsmu_get_boost_limit: 0x42,
            rsmu_set_boost_limit_all: 0x47,
            rsmu_set_co_margin: 0x53,
            rsmu_set_co_margin_all: 0x5D,
            rsmu_get_co_margin: get_co_margin,
            rsmu_get_stock_ppt: 0x13,
            rsmu_get_stock_tdc: 0x15,
            rsmu_get_dram_base: 0x66,
            rsmu_get_table_version: 0x6,
            mp1_set_ppt: 0x15,
            mp1_set_tdc: 0x1A,
            mp1_set_edc: 0x1C,
            mp1_set_scalar: 0x63,
            mp1_set_co_margin: 0x4B,
            mp1_set_co_margin_all: 0x4C,
            mp1_get_sustained_power: 0x5F,
            ..BASE
        }
    }
}

/// Where the limits live in the SMU power table, as float indices (-1 = not known). After every
/// limit write the matching float is checked; one that does not follow is dropped from read-back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmTableLayout {
    pub name: &'static str,
    pub ppt_limit: i32,
    pub ppt_value: i32,
    pub tdc_limit: i32,
    pub tdc_value: i32,
    pub edc_limit: i32,
    pub edc_value: i32,
    pub thm_limit: i32,
    pub thm_value: i32,
    pub socket_power: i32,
}

impl PmTableLayout {
    /// ryzen_smu's layout: limit/value pairs for PPT, TDC, THM, FIT, EDC from the top.
    pub const ZEN2_ZEN3: PmTableLayout = PmTableLayout {
        name: "Zen 2 / Zen 3 desktop",
        ppt_limit: 0,
        ppt_value: 1,
        tdc_limit: 2,
        tdc_value: 3,
        edc_limit: 8,
        edc_value: 9,
        thm_limit: 4,
        thm_value: 5,
        socket_power: -1,
    };
    /// Measured on a 9850X3D (table 0x00620105) with PPT 300 / TDC 200 / EDC 480 / Tctl 95 in force:
    /// 300 sat at float 2, 200 at 8, 95 at 10, 480 at 63, each followed by its live value; 26 is total socket power.
    pub const ZEN5: PmTableLayout = PmTableLayout {
        name: "Zen 5 desktop",
        ppt_limit: 2,
        ppt_value: 3,
        tdc_limit: 8,
        tdc_value: 9,
        edc_limit: 63,
        edc_value: 64,
        thm_limit: 10,
        thm_value: 11,
        socket_power: 26,
    };
    /// Zen 4 shares the first part of that layout (LibreHardwareMonitor's indices agree); the EDC position is not known.
    pub const ZEN4: PmTableLayout = PmTableLayout {
        name: "Zen 4 desktop (PPT/TDC/THM only)",
        ppt_limit: 2,
        ppt_value: 3,
        tdc_limit: 8,
        tdc_value: 9,
        edc_limit: -1,
        edc_value: -1,
        thm_limit: 10,
        thm_value: 11,
        socket_power: 26,
    };

    /// The limit layout for a power table version (by its major part), `None` when unknown.
    pub const fn for_version(version: u32) -> Option<PmTableLayout> {
        match version >> 16 {
            0x24 | 0x2D | 0x38 => Some(PmTableLayout::ZEN2_ZEN3),
            0x54 => Some(PmTableLayout::ZEN4),
            0x62 => Some(PmTableLayout::ZEN5),
            _ => None,
        }
    }
}

/// `Global\Access_PCI`, the named mutex other Ryzen tools take around SMN accesses.
struct PciMutex(OwnedHandle);

impl PciMutex {
    const NAME: &'static str = r"Global\Access_PCI";

    /// Same rights .NET's `Mutex` asks for: MAXIMUM_ALLOWED | SYNCHRONIZE | MUTEX_MODIFY_STATE.
    const ACCESS: u32 = 0x0200_0000 | 0x0010_0000 | 0x0000_0001;

    /// `new Mutex(false, name)`; on access denied `Mutex.OpenExisting(name)`; anything else: none.
    fn open() -> Option<Self> {
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        use windows_sys::Win32::System::Threading::{CreateMutexExW, OpenMutexW};
        let name = native::wide(Self::NAME);
        let h = unsafe { CreateMutexExW(std::ptr::null(), name.as_ptr(), 0, Self::ACCESS) };
        if !h.is_null() {
            return Some(Self(OwnedHandle(h)));
        }
        if native::last_error() != ERROR_ACCESS_DENIED {
            return None;
        }
        let h = unsafe { OpenMutexW(Self::ACCESS, 0, name.as_ptr()) };
        if h.is_null() { None } else { Some(Self(OwnedHandle(h))) }
    }

    /// `WaitOne(ms)`: an abandoned mutex counts as acquired; a timeout or failure does not.
    fn wait(&self, ms: u32) -> bool {
        use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        let r = unsafe { WaitForSingleObject(self.0.0, ms) };
        r == WAIT_OBJECT_0 || r == WAIT_ABANDONED
    }

    fn release(&self) {
        use windows_sys::Win32::System::Threading::ReleaseMutex;
        unsafe { ReleaseMutex(self.0.0) };
    }
}

/// Releases the global PCI mutex when dropped (the C# `finally { Release(); }`).
struct PciGuard<'a>(&'a AmdSmu);

impl Drop for PciGuard<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[derive(Default)]
struct State {
    version: u32,
    status: String,
    table_version: u32,
    table_address: u64,
    table_size: i32,
    layout: Option<PmTableLayout>,
    table: Option<Vec<f32>>,
    pawn: Option<Arc<PawnIo>>,
    untrusted: HashSet<String>,
    last_table_error: Option<String>,
}

/// AMD System Management Unit access: SMN registers through the north bridge index/data pair,
/// the RSMU / MP1 mailboxes, and the power table the SMU publishes in DRAM. The path Ryzen
/// Master and every third-party Ryzen tool uses; nothing here is board specific.
pub struct AmdSmu {
    drv: Arc<dyn KernelDriver>,
    pci_mutex: Option<PciMutex>,
    /// The C# `_lock`: serialises SMN / mailbox / PawnIO transactions. Lock order: `io`, then `state`.
    io: Mutex<()>,
    state: Mutex<State>,
    messages: SmuMessageSet,
}

const SMN_INDEX: u16 = 0x60; // D0F0 index/data pair
const SMN_DATA: u16 = 0x64;
const MAILBOX_ARGS: usize = 6;
const MAILBOX_TIMEOUT_ITERATIONS: usize = 8192;

/// `Args(params uint[] a)`: up to six arguments, the rest zero.
fn args(a: &[u32]) -> [u32; MAILBOX_ARGS] {
    let mut r = [0u32; MAILBOX_ARGS];
    let n = a.len().min(MAILBOX_ARGS);
    r[..n].copy_from_slice(&a[..n]);
    r
}

/// .NET `Math.Round(double)`: round half to even.
fn round_even(v: f64) -> f64 {
    v.round_ties_even()
}

/// .NET `Math.Round(double, digits)` (half to even).
pub(crate) fn round_digits(v: f64, digits: i32) -> f64 {
    if v.abs() < 1e16 {
        let p = 10f64.powi(digits);
        round_even(v * p) / p
    } else {
        v
    }
}

fn opt_display<T: std::fmt::Display>(v: &Option<T>) -> String {
    v.as_ref().map(|x| x.to_string()).unwrap_or_default()
}

impl AmdSmu {
    pub fn new(drv: Arc<dyn KernelDriver>, messages: SmuMessageSet) -> Self {
        // HWiNFO, Ryzen Master, LibreHardwareMonitor and ZenStates all take this mutex around SMN
        // accesses; two tools racing the index/data pair corrupt each other's reads.
        Self::with_pci_mutex(drv, messages, PciMutex::open())
    }

    /// An SMU that does not take the global PCI mutex (tests run against a fake driver).
    #[cfg(test)]
    pub(crate) fn without_pci_mutex(drv: Arc<dyn KernelDriver>, messages: SmuMessageSet) -> Self {
        Self::with_pci_mutex(drv, messages, None)
    }

    fn with_pci_mutex(drv: Arc<dyn KernelDriver>, messages: SmuMessageSet, pci_mutex: Option<PciMutex>) -> Self {
        Self {
            drv,
            pci_mutex,
            io: Mutex::new(()),
            state: Mutex::new(State { status: "not probed".into(), ..State::default() }),
            messages,
        }
    }

    pub fn messages(&self) -> &SmuMessageSet {
        &self.messages
    }
    pub fn version(&self) -> u32 {
        lock(&self.state).version
    }
    pub fn version_text(&self) -> String {
        Self::format_version(self.version())
    }
    fn format_version(v: u32) -> String {
        if v == 0 {
            "unknown".into()
        } else {
            format!("{}.{}.{}.{}", (v >> 24) & 0xFF, (v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF)
        }
    }
    pub fn status(&self) -> String {
        lock(&self.state).status.clone()
    }
    pub fn table_version(&self) -> u32 {
        lock(&self.state).table_version
    }
    pub fn table_address(&self) -> u64 {
        lock(&self.state).table_address
    }
    pub fn table_size(&self) -> i32 {
        lock(&self.state).table_size
    }
    pub fn layout(&self) -> Option<PmTableLayout> {
        lock(&self.state).layout
    }
    /// A copy of the last power table read.
    pub fn table(&self) -> Option<Vec<f32>> {
        lock(&self.state).table.clone()
    }
    /// True once PawnIO holds the RyzenSMU module; without it the table cannot be read.
    pub fn table_readable(&self) -> bool {
        lock(&self.state).pawn.is_some()
    }
    pub fn last_table_error(&self) -> Option<String> {
        lock(&self.state).last_table_error.clone()
    }

    // ------------------------------------------------------------------ SMN
    fn acquire(&self) -> Option<PciGuard<'_>> {
        match &self.pci_mutex {
            None => Some(PciGuard(self)),
            Some(m) => m.wait(5000).then(|| PciGuard(self)),
        }
    }

    fn release(&self) {
        if let Some(m) = &self.pci_mutex {
            m.release();
        }
    }

    fn smn_read_no_lock(&self, address: u32) -> Option<u32> {
        if !self.drv.write_pci_config(0, 0, 0, SMN_INDEX, address) {
            return None;
        }
        // The driver client calls all-ones "absent"; here it is a legal value, so the raw read
        // (which only fails when the driver call fails) is used.
        self.drv.read_pci_config_raw(0, 0, 0, SMN_DATA)
    }

    fn smn_write_no_lock(&self, address: u32, value: u32) -> bool {
        self.drv.write_pci_config(0, 0, 0, SMN_INDEX, address) && self.drv.write_pci_config(0, 0, 0, SMN_DATA, value)
    }

    /// Reads one SMN register under the global PCI mutex; `None` on failure.
    pub fn read_smn(&self, address: u32) -> Option<u32> {
        let _io = lock(&self.io);
        let _pci = self.acquire()?;
        self.smn_read_no_lock(address)
    }

    // ------------------------------------------------------------------ mailbox
    fn wait_ready(&self, mb: &SmuMailbox) -> bool {
        for _ in 0..MAILBOX_TIMEOUT_ITERATIONS {
            if matches!(self.smn_read_no_lock(mb.rsp), Some(rsp) if rsp != 0) {
                return true;
            }
        }
        false
    }

    /// One transaction: wait for the previous response, clear the response register, write the
    /// arguments and the message, wait, then read the status and the arguments back.
    pub fn send(&self, mb: &SmuMailbox, message: u32, args: &mut [u32; MAILBOX_ARGS]) -> SmuStatus {
        if !mb.is_valid() || message == 0 {
            return SmuStatus::NotImplemented;
        }
        let _io = lock(&self.io);
        let Some(_pci) = self.acquire() else { return SmuStatus::MutexTimeout };
        if !self.wait_ready(mb) {
            return SmuStatus::MailboxBusy;
        }
        if !self.smn_write_no_lock(mb.rsp, 0) {
            return SmuStatus::PciError;
        }
        for (i, a) in args.iter().enumerate() {
            if !self.smn_write_no_lock(mb.arg.wrapping_add((i * 4) as u32), *a) {
                return SmuStatus::PciError;
            }
        }
        if !self.smn_write_no_lock(mb.msg, message) {
            return SmuStatus::PciError;
        }
        if !self.wait_ready(mb) {
            return SmuStatus::NoResponse;
        }
        let Some(status) = self.smn_read_no_lock(mb.rsp) else { return SmuStatus::PciError };
        if status > 0xFF {
            return SmuStatus::Failed;
        }
        let status = SmuStatus::from_code(status as u8);
        if status == SmuStatus::Ok {
            for (i, a) in args.iter_mut().enumerate() {
                match self.smn_read_no_lock(mb.arg.wrapping_add((i * 4) as u32)) {
                    Some(v) => *a = v,
                    None => return SmuStatus::PciError,
                }
            }
        }
        status
    }

    pub fn send_rsmu(&self, message: u32, args: &mut [u32; MAILBOX_ARGS]) -> SmuStatus {
        self.send(&self.messages.rsmu, message, args)
    }
    pub fn send_mp1(&self, message: u32, args: &mut [u32; MAILBOX_ARGS]) -> SmuStatus {
        self.send(&self.messages.mp1, message, args)
    }

    /// Sends to the RSMU and, when that mailbox lacks the message or refuses, to MP1.
    fn send_either(&self, rsmu_msg: u32, mp1_msg: u32, arg: u32) -> SmuStatus {
        let mut st = SmuStatus::NotImplemented;
        if rsmu_msg != 0 {
            st = self.send_rsmu(rsmu_msg, &mut args(&[arg]));
        }
        if st != SmuStatus::Ok && mp1_msg != 0 {
            st = self.send_mp1(mp1_msg, &mut args(&[arg]));
        }
        st
    }

    pub fn describe(s: SmuStatus) -> String {
        match s {
            SmuStatus::Ok => "OK".into(),
            SmuStatus::Failed => "SMU reports failure".into(),
            SmuStatus::UnknownCommand => "unknown command for this SMU firmware".into(),
            SmuStatus::RejectedPrerequisite => {
                "rejected: prerequisite not met (PBO disabled in BIOS, or the board locked the limit)".into()
            }
            SmuStatus::RejectedBusy => "rejected: SMU busy".into(),
            SmuStatus::NotImplemented => "not available on this CPU generation".into(),
            SmuStatus::MutexTimeout => "another tool holds the PCI bus lock".into(),
            SmuStatus::MailboxBusy => "mailbox never became ready".into(),
            SmuStatus::NoResponse => "no response from the SMU".into(),
            SmuStatus::PciError => "SMN access failed".into(),
            SmuStatus::Other(b) => format!("status 0x{b:02X}"),
        }
    }

    fn throw(what: &str, s: SmuStatus) -> Result<()> {
        bail!("{what}: {}.", Self::describe(s))
    }

    // ------------------------------------------------------------------ probing
    /// Test message (0x1) and firmware version (0x2) on the RSMU; fills `status`.
    pub fn probe(&self) -> bool {
        let mut a = args(&[7]);
        let st = self.send_rsmu(0x1, &mut a);
        if st != SmuStatus::Ok || a[0] != 8 {
            lock(&self.state).status = format!(
                "RSMU mailbox at 0x{:08X} not answering ({})",
                self.messages.rsmu.msg,
                Self::describe(st)
            );
            return false;
        }
        let mut a = args(&[]);
        if self.send_rsmu(0x2, &mut a) == SmuStatus::Ok {
            lock(&self.state).version = a[0];
        }
        let status = format!(
            "{} SMU firmware {} ({} 0x{:08X})",
            self.messages.family,
            self.version_text(),
            self.messages.rsmu.name,
            self.messages.rsmu.msg
        );
        lock(&self.state).status = status;
        true
    }

    /// IsOverclockable flags: bit 0 OC enabled, bit 1 power limits, bit 2 PBO.
    pub fn read_oc_capabilities(&self) -> Option<u32> {
        let mut a = args(&[]);
        (self.send_rsmu(self.messages.rsmu_is_overclockable, &mut a) == SmuStatus::Ok).then_some(a[0])
    }

    // ------------------------------------------------------------------ limits
    /// Limits take milliwatts / milliamperes. Mobile SMUs want the MP1 variant first.
    fn set_limit(&self, name: &str, rsmu_msg: u32, mp1_msg: u32, value: f64) -> Result<()> {
        let raw = round_even(value * 1000.0) as u32;
        let mut st = if self.messages.is_apu && mp1_msg != 0 {
            self.send_mp1(mp1_msg, &mut args(&[raw]))
        } else {
            SmuStatus::NotImplemented
        };
        if st != SmuStatus::Ok {
            st = self.send_either(rsmu_msg, mp1_msg, raw);
        }
        if st != SmuStatus::Ok {
            return Self::throw(name, st);
        }
        Ok(())
    }

    pub fn set_ppt(&self, watts: f64) -> Result<()> {
        self.set_limit("PPT", self.messages.rsmu_set_ppt, self.messages.mp1_set_ppt, watts)
    }
    pub fn set_tdc(&self, amps: f64) -> Result<()> {
        self.set_limit("TDC", self.messages.rsmu_set_tdc, self.messages.mp1_set_tdc, amps)
    }
    pub fn set_edc(&self, amps: f64) -> Result<()> {
        self.set_limit("EDC", self.messages.rsmu_set_edc, self.messages.mp1_set_edc, amps)
    }

    pub fn set_tctl_max(&self, celsius: f64) -> Result<()> {
        let st = self.send_either(self.messages.rsmu_set_tctl_max, 0, round_even(celsius) as u32);
        if st != SmuStatus::Ok {
            return Self::throw("Tctl max", st);
        }
        Ok(())
    }

    /// The CPU's stock PPT (W), TDC (A) and EDC (A) as fused into the part. Constants: writing 150 W
    /// and asking again still gives 162 on a 9850X3D, so they are what "0" restores, not a read-back.
    pub fn read_stock_limits(&self) -> (Option<f64>, Option<f64>, Option<f64>) {
        let q = |msg: u32| -> Option<f64> {
            if msg == 0 {
                return None;
            }
            let mut a = args(&[]);
            (self.send_rsmu(msg, &mut a) == SmuStatus::Ok && a[0] != 0).then(|| a[0] as f64 / 1000.0)
        };
        (
            q(self.messages.rsmu_get_stock_ppt),
            q(self.messages.rsmu_get_stock_tdc),
            q(self.messages.rsmu_get_stock_edc),
        )
    }

    /// Sustained power limit (W) and thermal limit (C) the platform configured: `(power, temp)`.
    pub fn read_sustained_limits(&self) -> Option<(i32, i32)> {
        let mut a = args(&[]);
        let st = if self.messages.rsmu_get_sustained_power != 0 {
            self.send_rsmu(self.messages.rsmu_get_sustained_power, &mut a)
        } else if self.messages.mp1_get_sustained_power != 0 {
            self.send_mp1(self.messages.mp1_get_sustained_power, &mut a)
        } else {
            SmuStatus::NotImplemented
        };
        if st != SmuStatus::Ok {
            return None;
        }
        let power = ((a[0] >> 16) & 0xFF) as i32;
        let temp = (a[0] & 0xFF) as i32;
        (power > 0).then_some((power, temp))
    }

    // ------------------------------------------------------------------ scalar
    /// PBO scalar 1..10. `None` when unreadable; 0 means the SMU is in manual OC mode.
    pub fn read_scalar(&self) -> Option<f64> {
        if self.messages.rsmu_get_scalar == 0 {
            return None;
        }
        let mut a = args(&[]);
        if self.send_rsmu(self.messages.rsmu_get_scalar, &mut a) != SmuStatus::Ok {
            return None;
        }
        let f = f32::from_bits(a[0]);
        if f.is_nan() || f < 0.0 || f > 10.0 { None } else { Some(round_digits(f as f64, 2)) }
    }

    pub fn set_scalar(&self, scalar: f64) -> Result<()> {
        if self.messages.mp1_set_pbo_enable != 0 {
            self.send_mp1(self.messages.mp1_set_pbo_enable, &mut args(&[])); // Zen 2 / Zen 3: allow PBO overdrive
        }
        let st = self.send_either(
            self.messages.rsmu_set_scalar,
            self.messages.mp1_set_scalar,
            round_even(scalar * 100.0) as u32,
        );
        if st != SmuStatus::Ok {
            return Self::throw("PBO scalar", st);
        }
        Ok(())
    }

    // ------------------------------------------------------------------ curve optimizer
    /// The core address the SMU wants: CCD in bits 31:28, CCX in 27:24 (Zen 2 only), core in 23:20. APUs take the plain index.
    pub fn core_mask(&self, ccd: i32, core_in_ccd: i32) -> u32 {
        Self::core_mask_for(&self.messages, ccd, core_in_ccd)
    }

    fn core_mask_for(m: &SmuMessageSet, ccd: i32, core_in_ccd: i32) -> u32 {
        if m.is_apu {
            return core_in_ccd as u32;
        }
        if m.ccx_in_core_mask {
            return ((ccd as u32) << 28) | (((core_in_ccd / 4) as u32) << 24) | (((core_in_ccd % 4) as u32) << 20);
        }
        ((ccd as u32) << 28) | (((core_in_ccd % 8) as u32) << 20)
    }

    /// Margin as the SMU encodes it: 16-bit two's complement in the low half of the argument.
    fn margin_arg(margin: i32) -> u32 {
        (if margin < 0 { 0x10000 + margin } else { margin }) as u32 & 0xFFFF
    }

    /// MP1 is preferred for the Curve Optimizer, as ZenStates does; RSMU is the fallback.
    fn send_co(&self, mp1_msg: u32, rsmu_msg: u32, arg: u32) -> SmuStatus {
        if mp1_msg != 0 {
            self.send_mp1(mp1_msg, &mut args(&[arg]))
        } else if rsmu_msg != 0 {
            self.send_rsmu(rsmu_msg, &mut args(&[arg]))
        } else {
            SmuStatus::NotImplemented
        }
    }

    pub fn set_curve_optimizer(&self, ccd: i32, core_in_ccd: i32, margin: i32) -> Result<()> {
        // APUs take the plain core index (0-7), which the 0xFFF00000 mask below clears: every
        // per-core write would carry the same core field and land on one core (inherited from the
        // .NET build). Until the APU core encoding is confirmed on hardware, refuse rather than
        // undervolt the wrong core. The all-core message needs no core address and still works.
        if self.messages.is_apu {
            bail!(
                "Per-core Curve Optimizer is not supported on this APU yet (its core addressing is unverified); \
                 use the all-core Curve Optimizer value instead."
            );
        }
        let margin = margin.clamp(-self.messages.co_range, self.messages.co_range);
        let st = self.send_co(
            self.messages.mp1_set_co_margin,
            self.messages.rsmu_set_co_margin,
            (self.core_mask(ccd, core_in_ccd) & 0xFFF0_0000) | Self::margin_arg(margin),
        );
        if st != SmuStatus::Ok {
            return Self::throw(&format!("Curve Optimizer core {ccd}/{core_in_ccd}"), st);
        }
        Ok(())
    }

    pub fn set_curve_optimizer_all(&self, margin: i32) -> Result<()> {
        let margin = margin.clamp(-self.messages.co_range, self.messages.co_range);
        let st = self.send_co(
            self.messages.mp1_set_co_margin_all,
            self.messages.rsmu_set_co_margin_all,
            Self::margin_arg(margin),
        );
        if st != SmuStatus::Ok {
            return Self::throw("Curve Optimizer (all cores)", st);
        }
        Ok(())
    }

    /// Current margin of one core, or `None` when this firmware cannot report it.
    pub fn read_curve_optimizer(&self, ccd: i32, core_in_ccd: i32) -> Option<i32> {
        if self.messages.rsmu_get_co_margin == 0 {
            return None;
        }
        let mut a = args(&[self.core_mask(ccd, core_in_ccd)]);
        if self.send_rsmu(self.messages.rsmu_get_co_margin, &mut a) != SmuStatus::Ok {
            return None;
        }
        Self::decode_margin(a[0])
    }

    fn decode_margin(v: u32) -> Option<i32> {
        // 16-bit or 32-bit two's complement
        let m = if v > 0x7FFF && v < 0x10000 { v as i32 - 0x10000 } else { v as i32 };
        (m.unsigned_abs() <= 127).then_some(m)
    }

    // ------------------------------------------------------------------ boost limit
    pub fn read_boost_limit_mhz(&self) -> Option<i32> {
        if self.messages.rsmu_get_boost_limit == 0 {
            return None;
        }
        let mut a = args(&[]);
        (self.send_rsmu(self.messages.rsmu_get_boost_limit, &mut a) == SmuStatus::Ok && a[0] > 0)
            .then_some(a[0] as i32)
    }

    pub fn set_boost_limit_mhz(&self, mhz: i32) -> Result<()> {
        let st = self.send_rsmu(self.messages.rsmu_set_boost_limit_all, &mut args(&[(mhz as u32) & 0xFFFFF]));
        if st != SmuStatus::Ok {
            return Self::throw("FMax", st);
        }
        Ok(())
    }

    // ------------------------------------------------------------------ power table
    /// Asks the SMU for the table version and address and picks a layout for the limits.
    /// `Ok(status)` when the table was located, `Err(status)` with the reason when not.
    pub fn locate_table(&self) -> std::result::Result<String, String> {
        {
            let mut s = lock(&self.state);
            s.table_address = 0;
            s.table_size = 0;
            s.table_version = 0;
            s.layout = None;
        }
        let m = &self.messages;
        if m.rsmu_get_table_version == 0 || m.rsmu_get_dram_base == 0 {
            return Err("no power table on this SMU".into());
        }
        let mut a = args(&[]);
        let mut table_version = 0u32;
        if self.send_rsmu(m.rsmu_get_table_version, &mut a) == SmuStatus::Ok {
            table_version = a[0];
            lock(&self.state).table_version = table_version;
        }

        let table_address: u64;
        if *m == SmuMessageSet::ZEN || *m == SmuMessageSet::ZEN_PLUS {
            // Three-step handshake on the first generation.
            if self.send_rsmu(m.rsmu_get_dram_base - 1, &mut args(&[])) != SmuStatus::Ok {
                return Err("power table address handshake refused".into());
            }
            let mut a = args(&[]);
            if self.send_rsmu(m.rsmu_get_dram_base, &mut a) != SmuStatus::Ok {
                return Err("power table address refused".into());
            }
            table_address = a[0] as u64;
            lock(&self.state).table_address = table_address;
            self.send_rsmu(m.rsmu_get_dram_base + 2, &mut args(&[]));
        } else {
            let mut a = args(&[1, 1]);
            let st = self.send_rsmu(m.rsmu_get_dram_base, &mut a);
            if st != SmuStatus::Ok {
                return Err(format!("power table address refused ({})", Self::describe(st)));
            }
            table_address = ((a[1] as u64) << 32) | a[0] as u64;
            lock(&self.state).table_address = table_address;
        }
        if table_address == 0 {
            return Err("power table address is zero".into());
        }

        let table_size = Self::known_table_size(table_version);
        let layout = PmTableLayout::for_version(table_version);
        {
            let mut s = lock(&self.state);
            s.table_size = table_size;
            s.layout = layout;
        }
        Ok(format!("power table v0x{table_version:08X}, {table_size} bytes at 0x{table_address:X}")
            + &match layout {
                None => ", limit layout unknown for this version".to_string(),
                Some(l) => format!(", layout {}", l.name),
            })
    }

    pub fn known_table_size(version: u32) -> i32 {
        match version {
            0x240902 => 0x514,
            0x240903 => 0x518,
            0x240802 => 0x7E0,
            0x240803 => 0x7E4,
            0x2D0903 => 0x594,
            0x380904 => 0x5A4,
            0x380905 => 0x5D0,
            0x2D0803 => 0x894,
            0x380804 => 0x8A4,
            0x380805 => 0x8F0,
            0x540004 => 0x948,
            0x540104 => 0x950,
            0x540108 => 0x6BC,
            0x540208 => 0x8D0,
            0x620105 => 0x724,
            0x620205 => 0x994,
            0x621102 => 0x724,
            0x621202 => 0x994,
            _ => match version >> 16 {
                0x73 => 0x1004,
                0x5C => 0xDA8,
                _ => 0x994,
            },
        }
    }

    /// Loads the bundled RyzenSMU module into an installed PawnIO, which reads the table in the kernel.
    /// The bundled WinRing0 cannot: its physical-memory read is limited to the BIOS ROM window.
    /// `Ok(status)` when attached, `Err(status)` with the reason when not.
    pub fn try_attach_pawn_io(&self, module_path: &Path) -> std::result::Result<String, String> {
        if !PawnIo::is_installed() {
            return Err("PawnIO is not installed (https://pawnio.eu), so the SMU power table cannot be read and PPT/TDC/EDC have no read-back".into());
        }
        let pawn = match PawnIo::load_module(module_path) {
            Ok(p) => p,
            Err(e) => return Err(format!("PawnIO: {e}")),
        };
        let _io = lock(&self.io);
        // Dropping `pawn` on an early return is the C# `pawn.Dispose()`.
        let Some(_pci) = self.acquire() else { return Err("PawnIO: PCI bus lock busy".into()) };
        let resolved = pawn.execute("ioctl_resolve_pm_table", &[], 2);
        let resolved = match resolved {
            Some(r) if r.len() >= 2 => r,
            _ => {
                return Err(format!(
                    "PawnIO RyzenSMU module could not resolve the table ({})",
                    opt_display(&pawn.last_error())
                ));
            }
        };
        let version = resolved[0] as u32;
        let mut s = lock(&self.state);
        if s.table_version != 0 && version != s.table_version {
            return Err(format!(
                "PawnIO reports table v0x{version:08X} but the SMU told us v0x{:08X}; not trusting it",
                s.table_version
            ));
        }
        if s.table_address == 0 {
            s.table_address = resolved[1] as u64;
        }
        if s.table_version == 0 {
            s.table_version = version;
            s.table_size = Self::known_table_size(version);
        }
        s.pawn = Some(Arc::new(pawn));
        Ok(format!(
            "PawnIO {} with the RyzenSMU module reads the power table",
            opt_display(&PawnIo::installed_version())
        ))
    }

    /// Has the module refresh the table (TransferTableToDram) and reads it; one page per call.
    pub fn refresh_table(&self) -> bool {
        let (pawn, table_size) = {
            let mut s = lock(&self.state);
            let Some(pawn) = s.pawn.clone() else {
                s.last_table_error = Some("PawnIO not attached".into());
                return false;
            };
            if s.table_size == 0 {
                s.last_table_error = Some("table not located".into());
                return false;
            }
            (pawn, s.table_size)
        };
        let _io = lock(&self.io);
        let Some(_pci) = self.acquire() else {
            lock(&self.state).last_table_error = Some("PCI bus lock busy".into());
            return false;
        };
        if pawn.execute("ioctl_update_pm_table", &[], 0).is_none() {
            lock(&self.state).last_table_error = Some(format!("PawnIO {}", opt_display(&pawn.last_error())));
            return false;
        }
        let raw = pawn.execute("ioctl_read_pm_table", &[], ((table_size + 7) / 8) as usize);
        let raw = match raw {
            Some(r) if !r.is_empty() => r,
            _ => {
                lock(&self.state).last_table_error = Some(format!("PawnIO {}", opt_display(&pawn.last_error())));
                return false;
            }
        };
        let floats = Self::floats_from_raw(&raw, table_size);
        let mut s = lock(&self.state);
        if floats.iter().all(|&f| f == 0.0) {
            s.last_table_error = Some("table reads as all zeros".into());
            return false;
        }
        s.last_table_error = None;
        s.table = Some(floats);
        true
    }

    /// `Buffer.BlockCopy` of the module's longs into `min(size / 4, longs * 2)` floats.
    fn floats_from_raw(raw: &[i64], table_size: i32) -> Vec<f32> {
        let count = ((table_size / 4) as usize).min(raw.len() * 2);
        let bytes: Vec<u8> = raw.iter().flat_map(|v| v.to_le_bytes()).collect();
        (0..count)
            .map(|i| f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap()))
            .collect()
    }

    fn field(s: &State, index: i32) -> Option<f32> {
        match &s.table {
            Some(t) if index >= 0 && (index as usize) < t.len() => Some(t[index as usize]),
            _ => None,
        }
    }

    fn limit(s: &State, name: &str, index: i32) -> Option<f32> {
        if s.untrusted.contains(name) { None } else { Self::field(s, index) }
    }

    fn with_layout(&self, f: impl FnOnce(&State, &PmTableLayout) -> Option<f32>) -> Option<f32> {
        let s = lock(&self.state);
        let l = s.layout?;
        f(&s, &l)
    }

    pub fn ppt_limit(&self) -> Option<f32> {
        self.with_layout(|s, l| Self::limit(s, "ppt", l.ppt_limit))
    }
    pub fn ppt_value(&self) -> Option<f32> {
        self.with_layout(|s, l| Self::field(s, l.ppt_value))
    }
    pub fn tdc_limit(&self) -> Option<f32> {
        self.with_layout(|s, l| Self::limit(s, "tdc", l.tdc_limit))
    }
    pub fn tdc_value(&self) -> Option<f32> {
        self.with_layout(|s, l| Self::field(s, l.tdc_value))
    }
    pub fn edc_limit(&self) -> Option<f32> {
        self.with_layout(|s, l| Self::limit(s, "edc", l.edc_limit))
    }
    pub fn edc_value(&self) -> Option<f32> {
        self.with_layout(|s, l| Self::field(s, l.edc_value))
    }
    pub fn thm_limit(&self) -> Option<f32> {
        self.with_layout(|s, l| Self::limit(s, "tctl", l.thm_limit))
    }
    pub fn thm_value(&self) -> Option<f32> {
        self.with_layout(|s, l| Self::field(s, l.thm_value))
    }
    pub fn socket_power(&self) -> Option<f32> {
        self.with_layout(|s, l| Self::field(s, l.socket_power))
    }

    /// After a limit write, checks that the table float the layout points at took the new value.
    /// `Some(true)`: it followed. `Some(false)`: it did not, and that field is no longer read back.
    /// `None`: nothing to check.
    pub fn verify_layout(&self, field: &str, read: &dyn Fn() -> Option<f32>, written: f64) -> Option<bool> {
        {
            let s = lock(&self.state);
            if s.layout.is_none() || s.pawn.is_none() || s.untrusted.contains(field) {
                return None;
            }
        }
        for _ in 0..3 {
            native::sleep_ms(60);
            if !self.refresh_table() {
                continue;
            }
            let f = read()?; // no index for this field on this layout
            if (f as f64 - written).abs() < 0.5 {
                return Some(true);
            }
        }
        let mut s = lock(&self.state);
        if s.table.is_none() {
            return None;
        }
        s.untrusted.insert(field.to_string());
        Some(false)
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A `KernelDriver` that emulates the SMN index/data pair and the SMU mailboxes.

    use std::any::Any;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::{SmuMailbox, SmuStatus};
    use crate::hw::KernelDriver;
    use crate::hw::native::lock;

    pub type Handler = Box<dyn FnMut(&str, u32, &mut [u32; 6]) -> u32 + Send>;

    #[derive(Default)]
    pub struct FakeState {
        pub index: u32,
        pub regs: HashMap<u32, u32>,
        pub mailboxes: Vec<SmuMailbox>,
        /// Every message sent: (mailbox name, message, args as written).
        pub sent: Vec<(String, u32, [u32; 6])>,
        pub msrs: HashMap<u32, u64>,
        /// Make SMN data reads fail with a fresh driver error.
        pub fail_reads: bool,
        pub errors: u32,
        pub last_error: Option<String>,
    }

    pub struct FakeSmn {
        pub state: Mutex<FakeState>,
        pub handler: Mutex<Handler>,
    }

    impl FakeSmn {
        pub fn new(mailboxes: &[SmuMailbox], handler: Handler) -> Self {
            let mut st = FakeState { mailboxes: mailboxes.to_vec(), ..FakeState::default() };
            for mb in mailboxes {
                st.regs.insert(mb.rsp, 1); // idle mailboxes answer "ready"
            }
            Self { state: Mutex::new(st), handler: Mutex::new(handler) }
        }

        pub fn sent(&self) -> Vec<(String, u32, [u32; 6])> {
            lock(&self.state).sent.clone()
        }
    }

    impl KernelDriver for FakeSmn {
        fn name(&self) -> String {
            "fake".into()
        }
        fn is_open(&self) -> bool {
            true
        }
        fn last_error(&self) -> Option<String> {
            lock(&self.state).last_error.clone()
        }
        fn read_msr(&self, index: u32, _cpu: i32) -> Option<u64> {
            lock(&self.state).msrs.get(&index).copied()
        }
        fn write_msr(&self, _index: u32, _value: u64, _cpu: i32) -> bool {
            false
        }
        fn read_io_port_byte(&self, _port: u16) -> anyhow::Result<u8> {
            anyhow::bail!("no ports")
        }
        fn read_io_port_word(&self, _port: u16) -> anyhow::Result<u16> {
            anyhow::bail!("no ports")
        }
        fn read_io_port_dword(&self, _port: u16) -> anyhow::Result<u32> {
            anyhow::bail!("no ports")
        }
        fn write_io_port_byte(&self, _port: u16, _value: u8) -> anyhow::Result<()> {
            anyhow::bail!("no ports")
        }
        fn write_io_port_word(&self, _port: u16, _value: u16) -> anyhow::Result<()> {
            anyhow::bail!("no ports")
        }
        fn write_io_port_dword(&self, _port: u16, _value: u32) -> anyhow::Result<()> {
            anyhow::bail!("no ports")
        }
        fn read_pci_config(&self, bus: u8, device: u8, function: u8, offset: u16) -> Option<u32> {
            assert_eq!((bus, device, function, offset), (0, 0, 0, 0x64));
            let mut st = lock(&self.state);
            if st.fail_reads {
                st.errors += 1;
                st.last_error = Some(format!("read failed #{}", st.errors));
                return None;
            }
            let v = st.regs.get(&st.index).copied().unwrap_or(0);
            // Like WinRing0: all-ones is reported as "absent".
            (v != 0xFFFF_FFFF).then_some(v)
        }
        fn read_pci_config_raw(&self, bus: u8, device: u8, function: u8, offset: u16) -> Option<u32> {
            assert_eq!((bus, device, function, offset), (0, 0, 0, 0x64));
            let mut st = lock(&self.state);
            if st.fail_reads {
                st.errors += 1;
                st.last_error = Some(format!("read failed #{}", st.errors));
                return None;
            }
            Some(st.regs.get(&st.index).copied().unwrap_or(0))
        }
        fn write_pci_config(&self, bus: u8, device: u8, function: u8, offset: u16, value: u32) -> bool {
            assert_eq!((bus, device, function), (0, 0, 0));
            let mut st = lock(&self.state);
            match offset {
                0x60 => st.index = value,
                0x64 => {
                    let index = st.index;
                    st.regs.insert(index, value);
                    let mb = st.mailboxes.iter().find(|m| m.msg == index).copied();
                    if let Some(mb) = mb {
                        let mut a = [0u32; 6];
                        for (i, x) in a.iter_mut().enumerate() {
                            *x = st.regs.get(&(mb.arg + (i * 4) as u32)).copied().unwrap_or(0);
                        }
                        st.sent.push((mb.name.to_string(), value, a));
                        drop(st);
                        let status = {
                            let mut h = lock(&self.handler);
                            (&mut *h)(mb.name, value, &mut a)
                        };
                        let mut st = lock(&self.state);
                        for (i, x) in a.iter().enumerate() {
                            st.regs.insert(mb.arg + (i * 4) as u32, *x);
                        }
                        st.regs.insert(mb.rsp, status);
                    }
                }
                _ => return false,
            }
            true
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    pub const OK: u32 = SmuStatus::Ok.code() as u32;
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeSmn, OK};
    use super::*;

    fn smu_with(messages: SmuMessageSet, handler: fake::Handler) -> (Arc<FakeSmn>, AmdSmu) {
        let drv = Arc::new(FakeSmn::new(&[messages.rsmu, messages.mp1], handler));
        let smu = AmdSmu::with_pci_mutex(drv.clone(), messages, None);
        (drv, smu)
    }

    #[test]
    fn status_codes_round_trip() {
        for b in 0..=255u8 {
            assert_eq!(SmuStatus::from_code(b).code(), b);
        }
        assert_eq!(SmuStatus::from_code(0x01), SmuStatus::Ok);
        assert_eq!(SmuStatus::from_code(0x33), SmuStatus::PciError);
        assert_eq!(SmuStatus::from_code(0x10), SmuStatus::Other(0x10));
    }

    #[test]
    fn describe_matches_csharp_text() {
        assert_eq!(AmdSmu::describe(SmuStatus::Ok), "OK");
        assert_eq!(AmdSmu::describe(SmuStatus::UnknownCommand), "unknown command for this SMU firmware");
        assert_eq!(
            AmdSmu::describe(SmuStatus::RejectedPrerequisite),
            "rejected: prerequisite not met (PBO disabled in BIOS, or the board locked the limit)"
        );
        assert_eq!(AmdSmu::describe(SmuStatus::NotImplemented), "not available on this CPU generation");
        assert_eq!(AmdSmu::describe(SmuStatus::PciError), "SMN access failed");
        assert_eq!(AmdSmu::describe(SmuStatus::Other(0x0A)), "status 0x0A");
    }

    #[test]
    fn message_set_flags() {
        let z = SmuMessageSet::ZEN;
        assert!(z.has_power_limits() && !z.has_scalar() && !z.has_curve_optimizer() && !z.has_boost_limit());
        assert!(z.has_current_limits()); // MP1 TDC + EDC
        assert_eq!(z.hsmp, SmuMailbox::new("HSMP", 0, 0, 0));
        assert_eq!(z.co_range, 30);
        let z4 = SmuMessageSet::ZEN4;
        assert_eq!(z4.co_range, 50);
        assert!(z4.has_curve_optimizer_readback() && z4.has_boost_limit());
        assert_eq!(z4.rsmu_get_co_margin, 0xD5);
        assert_eq!(SmuMessageSet::ZEN5.hsmp.msg, 0x03B10934);
        assert_eq!(SmuMessageSet::SHIMADA_PEAK.rsmu.msg, 0x03B10924);
        assert!(!SmuMessageSet::SHIMADA_PEAK.mp1.is_valid());
        assert_eq!(SmuMessageSet::SHIMADA_PEAK.rsmu_get_co_margin, 0xA3);
        let p = SmuMessageSet::apu_phoenix("Zen 5 APU", 0xAF, true);
        assert_eq!(p.mp1.msg, 0x03B10928);
        assert!(p.is_apu && p.co_range == 50 && p.rsmu_set_boost_limit_all == 0x47);
        assert_eq!(SmuMessageSet::apu_phoenix("x", 0, false).mp1.rsp, 0x03B10578);
        let a = SmuMessageSet::apu("Zen 2 APU", 0);
        assert!(!a.has_curve_optimizer_readback() && a.co_range == 30 && a.mp1_set_scalar == 0x49);
    }

    #[test]
    fn layout_lookup_and_table_sizes() {
        assert_eq!(PmTableLayout::for_version(0x00380805), Some(PmTableLayout::ZEN2_ZEN3));
        assert_eq!(PmTableLayout::for_version(0x00540108), Some(PmTableLayout::ZEN4));
        assert_eq!(PmTableLayout::for_version(0x00620105), Some(PmTableLayout::ZEN5));
        assert_eq!(PmTableLayout::for_version(0x00730000), None);
        assert_eq!(AmdSmu::known_table_size(0x620105), 0x724);
        assert_eq!(AmdSmu::known_table_size(0x540208), 0x8D0);
        assert_eq!(AmdSmu::known_table_size(0x730001), 0x1004);
        assert_eq!(AmdSmu::known_table_size(0x5C0001), 0xDA8);
        assert_eq!(AmdSmu::known_table_size(0), 0x994);
    }

    #[test]
    fn core_mask_encoding() {
        assert_eq!(AmdSmu::core_mask_for(&SmuMessageSet::ZEN2, 1, 5), (1 << 28) | (1 << 24) | (1 << 20));
        assert_eq!(AmdSmu::core_mask_for(&SmuMessageSet::ZEN3, 1, 7), (1 << 28) | (7 << 20));
        assert_eq!(AmdSmu::core_mask_for(&SmuMessageSet::ZEN5, 0, 3), 3 << 20);
        assert_eq!(AmdSmu::core_mask_for(&SmuMessageSet::apu("Zen 3 APU", 0xC3), 0, 6), 6);
    }

    #[test]
    fn margin_encoding_and_decoding() {
        assert_eq!(AmdSmu::margin_arg(-30), 0xFFE2);
        assert_eq!(AmdSmu::margin_arg(15), 15);
        assert_eq!(AmdSmu::decode_margin(0xFFE2), Some(-30));
        assert_eq!(AmdSmu::decode_margin(0xFFFF_FFFF), Some(-1));
        assert_eq!(AmdSmu::decode_margin(12), Some(12));
        assert_eq!(AmdSmu::decode_margin(200), None);
        assert_eq!(AmdSmu::decode_margin(0x8000_0000), None);
    }

    #[test]
    fn rounding_is_half_even_like_dotnet() {
        assert_eq!(round_even(2.5), 2.0);
        assert_eq!(round_even(3.5), 4.0);
        assert_eq!(round_digits(1.005f32 as f64, 2), 1.0);
        assert_eq!(round_digits(1.25, 1), 1.2);
    }

    #[test]
    fn probe_reads_version() {
        let (_drv, smu) = smu_with(
            SmuMessageSet::ZEN5,
            Box::new(|_, msg, a| {
                match msg {
                    0x1 => a[0] += 1,
                    0x2 => a[0] = 0x005D_2A00,
                    _ => {}
                }
                OK
            }),
        );
        assert_eq!(smu.status(), "not probed");
        assert!(smu.probe());
        assert_eq!(smu.version_text(), "0.93.42.0");
        assert_eq!(smu.status(), "Zen 5 SMU firmware 0.93.42.0 (RSMU 0x03B10524)");
    }

    #[test]
    fn probe_failure_reports_mailbox() {
        let (_drv, smu) = smu_with(SmuMessageSet::ZEN4, Box::new(|_, _, _| 0xFE));
        assert!(!smu.probe());
        assert_eq!(smu.status(), "RSMU mailbox at 0x03B10524 not answering (unknown command for this SMU firmware)");
        assert_eq!(smu.version_text(), "unknown");
    }

    #[test]
    fn send_writes_args_and_reads_them_back() {
        let (drv, smu) = smu_with(
            SmuMessageSet::ZEN4,
            Box::new(|_, _, a| {
                a[5] = 0xFFFF_FFFF; // all-ones is a legal argument value
                a[0] = a[0] * 2;
                OK
            }),
        );
        let mut a = [21, 2, 3, 4, 5, 6];
        let mb = SmuMessageSet::ZEN4.rsmu;
        assert_eq!(smu.send(&mb, 0x42, &mut a), SmuStatus::Ok);
        assert_eq!(a, [42, 2, 3, 4, 5, 0xFFFF_FFFF]);
        assert_eq!(drv.sent(), vec![("RSMU".to_string(), 0x42, [21, 2, 3, 4, 5, 6])]);
        assert_eq!(smu.send(&mb, 0, &mut a), SmuStatus::NotImplemented);
        assert_eq!(smu.send(&MAILBOX_NONE, 1, &mut a), SmuStatus::NotImplemented);
    }

    #[test]
    fn send_reports_smu_status_without_reading_args() {
        let (_drv, smu) = smu_with(
            SmuMessageSet::ZEN4,
            Box::new(|_, _, a| {
                a[0] = 99;
                0xFD
            }),
        );
        let mut a = args(&[5]);
        assert_eq!(smu.send_rsmu(0x56, &mut a), SmuStatus::RejectedPrerequisite);
        assert_eq!(a[0], 5);
    }

    #[test]
    fn failed_smn_reads_are_errors() {
        let (drv, smu) = smu_with(SmuMessageSet::ZEN4, Box::new(|_, _, _| OK));
        lock(&drv.state).regs.insert(0x59800, 0xFFFF_FFFF);
        assert_eq!(smu.read_smn(0x59800), Some(0xFFFF_FFFF));
        lock(&drv.state).fail_reads = true;
        assert_eq!(smu.read_smn(0x59800), None);
        assert_eq!(smu.send_rsmu(0x6E, &mut args(&[])), SmuStatus::MailboxBusy);
    }

    #[test]
    fn apu_limits_go_to_mp1_first() {
        let (drv, smu) = smu_with(SmuMessageSet::apu("Zen 3 APU", 0xC3), Box::new(|_, _, _| OK));
        smu.set_ppt(25.0005).unwrap();
        assert_eq!(drv.sent(), vec![("MP1".to_string(), 0x15, args(&[25000]))]);
    }

    #[test]
    fn desktop_limit_falls_back_to_mp1_and_reports_failure() {
        let (drv, smu) = smu_with(SmuMessageSet::ZEN4, Box::new(|mb, _, _| if mb == "RSMU" { 0xFD } else { 0xFE }));
        let err = smu.set_tdc(120.0).unwrap_err().to_string();
        assert_eq!(err, "TDC: unknown command for this SMU firmware.");
        let sent: Vec<(String, u32)> = drv.sent().into_iter().map(|(m, msg, _)| (m, msg)).collect();
        assert_eq!(sent, vec![("RSMU".to_string(), 0x57), ("MP1".to_string(), 0x3C)]);
    }

    #[test]
    fn curve_optimizer_prefers_mp1_and_clamps() {
        let (drv, smu) = smu_with(SmuMessageSet::ZEN4, Box::new(|_, _, _| OK));
        smu.set_curve_optimizer(1, 3, -80).unwrap();
        smu.set_curve_optimizer_all(7).unwrap();
        let sent = drv.sent();
        assert_eq!(sent[0], ("MP1".to_string(), 0x35, args(&[(1 << 28) | (3 << 20) | 0xFFCE])));
        assert_eq!(sent[1], ("MP1".to_string(), 0x36, args(&[7])));
    }

    #[test]
    fn curve_optimizer_readback_of_minus_one() {
        let (drv, smu) = smu_with(SmuMessageSet::ZEN5, Box::new(|_, _, a| {
            a[0] = 0xFFFF_FFFF;
            OK
        }));
        assert_eq!(smu.read_curve_optimizer(0, 2), Some(-1));
        assert_eq!(drv.sent()[0], ("RSMU".to_string(), 0xD5, args(&[2 << 20])));
    }

    #[test]
    fn scalar_boost_and_sustained_reads() {
        let (_drv, smu) = smu_with(
            SmuMessageSet::ZEN3,
            Box::new(|mb, msg, a| {
                match (mb, msg) {
                    ("RSMU", 0x6C) => a[0] = 2.5f32.to_bits(),
                    ("RSMU", 0x6E) => a[0] = 5050,
                    ("MP1", 0x23) => a[0] = (142 << 16) | 90,
                    _ => {}
                }
                OK
            }),
        );
        assert_eq!(smu.read_scalar(), Some(2.5));
        assert_eq!(smu.read_boost_limit_mhz(), Some(5050));
        assert_eq!(smu.read_sustained_limits(), Some((142, 90)));
        assert_eq!(smu.read_stock_limits(), (None, None, None));
    }

    #[test]
    fn locate_table_zen5() {
        let (drv, smu) = smu_with(
            SmuMessageSet::ZEN5,
            Box::new(|_, msg, a| {
                match msg {
                    0x5 => a[0] = 0x0062_0105,
                    0x4 => {
                        a[0] = 0xD000_0000;
                        a[1] = 0x1;
                    }
                    _ => {}
                }
                OK
            }),
        );
        let st = smu.locate_table().unwrap();
        assert_eq!(st, "power table v0x00620105, 1828 bytes at 0x1D0000000, layout Zen 5 desktop");
        assert_eq!(smu.table_address(), 0x1_D000_0000);
        assert_eq!(smu.layout(), Some(PmTableLayout::ZEN5));
        assert_eq!(drv.sent()[1], ("RSMU".to_string(), 0x4, args(&[1, 1])));
        // No table read yet: every field is unknown and verification has nothing to check.
        assert_eq!(smu.ppt_limit(), None);
        assert!(!smu.table_readable());
        assert_eq!(smu.verify_layout("ppt", &|| smu.ppt_limit(), 150.0), None);
        assert!(!smu.refresh_table());
        assert_eq!(smu.last_table_error().as_deref(), Some("PawnIO not attached"));
    }

    #[test]
    fn locate_table_zen_handshake() {
        let (drv, smu) = smu_with(
            SmuMessageSet::ZEN,
            Box::new(|_, msg, a| {
                if msg == 0xC {
                    a[0] = 0x1234_0000;
                }
                OK
            }),
        );
        let st = smu.locate_table().unwrap();
        assert_eq!(st, "power table v0x00000000, 2452 bytes at 0x12340000, limit layout unknown for this version");
        let msgs: Vec<u32> = drv.sent().into_iter().map(|(_, m, _)| m).collect();
        assert_eq!(msgs, vec![0xD, 0xB, 0xC, 0xE]);
    }

    #[test]
    fn locate_table_refused() {
        let (_drv, smu) = smu_with(SmuMessageSet::ZEN4, Box::new(|_, msg, _| if msg == 0x4 { 0xFC } else { OK }));
        assert_eq!(smu.locate_table(), Err("power table address refused (rejected: SMU busy)".to_string()));
        let (_drv, smu) = smu_with(SmuMessageSet::apu("x", 0), Box::new(|_, _, a| {
            *a = [0; 6];
            OK
        }));
        assert_eq!(smu.locate_table(), Err("power table address is zero".to_string()));
    }

    #[test]
    fn table_fields_follow_layout_and_trust() {
        let (_drv, smu) = smu_with(SmuMessageSet::ZEN4, Box::new(|_, _, _| OK));
        {
            let mut s = lock(&smu.state);
            s.layout = Some(PmTableLayout::ZEN4);
            s.table = Some((0..20).map(|i| i as f32).collect());
        }
        assert_eq!(smu.ppt_limit(), Some(2.0));
        assert_eq!(smu.tdc_value(), Some(9.0));
        assert_eq!(smu.edc_limit(), None);
        assert_eq!(smu.thm_limit(), Some(10.0));
        assert_eq!(smu.socket_power(), None); // index 26 beyond this table
        lock(&smu.state).untrusted.insert("ppt".into());
        assert_eq!(smu.ppt_limit(), None);
        assert_eq!(smu.ppt_value(), Some(3.0));
    }

    #[test]
    fn floats_from_module_longs() {
        let a = 1.5f32.to_bits() as u64 | ((2.0f32.to_bits() as u64) << 32);
        let raw = [a as i64, 3.0f32.to_bits() as i64];
        assert_eq!(AmdSmu::floats_from_raw(&raw, 12), vec![1.5, 2.0, 3.0]);
        assert_eq!(AmdSmu::floats_from_raw(&raw, 100), vec![1.5, 2.0, 3.0, 0.0]);
    }
}
