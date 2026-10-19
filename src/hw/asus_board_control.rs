//! ASUS ATK frequency control. Uses the installed vendor COM service; no bundled driver.

use anyhow::{Result, bail};

use super::native;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SvidState {
    pub offset_index: u32,
    pub target_index: u32,
    pub mode: u32,
}

impl SvidState {
    pub fn new(offset_index: u32, target_index: u32, mode: u32) -> Self {
        Self { offset_index, target_index, mode }
    }
}

/// ASUS ATK frequency control. Uses the installed vendor COM service; no bundled driver.
///
/// The vendor object is late-bound through `IDispatch` (the C# `dynamic`). It lives on a
/// private apartment-threaded COM thread that creates it, makes every call on it and releases
/// it on drop, so this type is `Send + Sync` without moving an apartment-bound proxy between
/// threads (the .NET runtime callable wrapper does that marshalling implicitly).
pub struct AsusBoardControl {
    vendor: com::Vendor,
    baseline: u32,
    baseline_mhz: f64,
    voltage_description: String,
    baseline_svid: Option<SvidState>,
    baseline_sa: Option<SvidState>,
    baseline_l2: Option<SvidState>,
    baseline_ring: Option<SvidState>,
}

const ARITHMETIC_OVERFLOW: &str = "Arithmetic operation resulted in an overflow.";

/// `checked((uint)Math.Round(x))`.
fn checked_round_u32(x: f64) -> Result<u32> {
    let r = x.round_ties_even();
    if !(r > -1.0 && r < 4_294_967_296.0) {
        bail!("{ARITHMETIC_OVERFLOW}");
    }
    Ok(r as u32)
}

/// `{value:0.000}` of a `double?`: null prints empty.
fn fmt3(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.3}")).unwrap_or_default()
}

impl AsusBoardControl {
    pub const BCLK_ID: u32 = 0x03010011;
    pub const VRM_ID: u32 = 0x03020027;
    pub const CORE_ID: u32 = 0x030D0012;
    pub const SA_ID: u32 = 0x030D0022;
    pub const L2_ID: u32 = 0x030D0026;
    pub const RING_ID: u32 = 0x030D0028;

    pub fn baseline(&self) -> u32 {
        self.baseline
    }
    pub fn baseline_mhz(&self) -> f64 {
        self.baseline_mhz
    }
    pub fn voltage_description(&self) -> &str {
        &self.voltage_description
    }
    pub fn baseline_svid(&self) -> Option<SvidState> {
        self.baseline_svid
    }
    pub fn baseline_sa(&self) -> Option<SvidState> {
        self.baseline_sa
    }
    pub fn baseline_l2(&self) -> Option<SvidState> {
        self.baseline_l2
    }
    pub fn baseline_ring(&self) -> Option<SvidState> {
        self.baseline_ring
    }

    fn new(vendor: com::Vendor) -> Result<Self> {
        let (supported, voltage_description) = vendor.run(|v| {
            let initialized = v.call(com::INIT, &mut [])?.to_u32("iAcpiInit")?;
            if (initialized & 1) == 0 {
                bail!("ASUS ACPI initialization failed");
            }
            let items = v.call("ItemsOfGroup", &mut [com::Arg::U4(3)])?.to_dispatch("ItemsOfGroup")?;
            let mut supported = false;
            let mut voltage_description = "Actual VRM voltage item unavailable".to_string();
            let count = items.get("Count")?.to_i32_checked()?;
            if !(0..=512).contains(&count) {
                bail!("Invalid ASUS item count");
            }
            for i in 0..count {
                let item = items.call("Item", &mut [com::Arg::I4(i)])?.to_dispatch("Item")?;
                let id = item.get("id")?.to_u32("id")?;
                if id == Self::BCLK_ID {
                    // Evaluated left to right and short-circuited, like the C# && chain.
                    supported = item.get("name")?.to_string_cast()?.as_deref() == Some("BCLK Frequency")
                        && item.get("Minimum")?.to_u32_explicit()? == 0xFF009C40
                        && item.get("Increment")?.to_u32_explicit()? == 10
                        && item.get("DefaultValue")?.to_u32_explicit()? == 6000
                        && item.get("Entries")?.to_u32_explicit()? == 49801;
                }
                if id == Self::VRM_ID {
                    let name = item.get("name")?.display();
                    let minimum = item.get("Minimum")?.to_u32_explicit()?;
                    let increment = item.get("Increment")?.display();
                    let entries = item.get("Entries")?.display();
                    voltage_description = format!("{name}: minimum 0x{minimum:08X}, step {increment}, entries {entries}");
                }
                // `item` is released here (Marshal.FinalReleaseComObject).
            }
            Ok((supported, voltage_description))
        })?;
        if !supported {
            bail!("ASUS BCLK metadata does not match the validated encoding");
        }
        let mut control = Self {
            vendor,
            baseline: 0,
            baseline_mhz: 0.0,
            voltage_description,
            baseline_svid: None,
            baseline_sa: None,
            baseline_l2: None,
            baseline_ring: None,
        };
        control.baseline = control.read_raw(Self::BCLK_ID)?;
        control.baseline_mhz = Self::decode_bclk(control.baseline)?;
        if control.baseline_mhz < 90.0 || control.baseline_mhz > 110.0 {
            bail!("ASUS BCLK baseline is outside the supported range");
        }
        // An unsupported voltage descriptor must not enable voltage writes.
        control.baseline_svid = control.read_svid(Self::CORE_ID).ok();
        control.baseline_sa = control.read_svid(Self::SA_ID).ok();
        control.baseline_l2 = control.read_svid(Self::L2_ID).ok();
        control.baseline_ring = control.read_svid(Self::RING_ID).ok();
        Ok(control)
    }

    /// Opens the ASUS vendor service on an ASUS board. Returns the control (or `None`) and a
    /// status line (the C# `out string status`).
    pub fn try_create(manufacturer: &str) -> (Option<AsusBoardControl>, String) {
        if !manufacturer.to_uppercase().contains("ASUS") {
            return (None, "not an ASUS board".to_string());
        }
        // On failure the vendor object is released when `Vendor` drops.
        match com::Vendor::create("atkexCom.axdata").and_then(Self::new) {
            Ok(result) => {
                let status = format!("ASUS BCLK target {:.3} MHz; {}", result.baseline_mhz, result.voltage_description);
                (Some(result), status)
            }
            Err(ex) => (None, ex.to_string()),
        }
    }

    pub fn decode_bclk(index: u32) -> Result<f64> {
        if index >= 49801 {
            bail!("Invalid ASUS BCLK index");
        }
        Ok((40000.0 + index as f64 * 10.0) / 1000.0)
    }

    pub fn encode_bclk(mhz: f64) -> Result<u32> {
        if !mhz.is_finite() || mhz < 90.0 || mhz > 110.0 {
            bail!("Supported ASUS BCLK range is 90–110 MHz");
        }
        let index = (mhz * 1000.0 - 40000.0) / 10.0;
        if (index - index.round_ties_even()).abs() > 0.00001 {
            bail!("ASUS BCLK requires 0.01 MHz steps");
        }
        checked_round_u32(index)
    }

    pub fn read_raw(&self, id: u32) -> Result<u32> {
        let (status, value) = self.vendor.run(move |v| {
            let mut value = 0u32;
            let status = v.call("iAcpiGetItem", &mut [com::Arg::U4(id), com::Arg::RefU4(&mut value)])?.to_u32("iAcpiGetItem")?;
            Ok((status, value))
        })?;
        if (status & 1) == 0 {
            bail!("ASUS read 0x{id:08X} failed: 0x{status:08X}");
        }
        Ok(value)
    }

    pub fn read_bclk(&self) -> Result<f64> {
        Self::decode_bclk(self.read_raw(Self::BCLK_ID)?)
    }

    pub fn read_vcore(&self) -> Result<f64> {
        Ok(self.read_raw(0x06020011)? as f64 / 1000.0)
    }

    pub fn read_rail_voltage(&self, id: u32) -> Result<f64> {
        let item = match id {
            Self::CORE_ID => 0x06020011,
            Self::SA_ID => 0x06020091,
            Self::L2_ID => 0x0602008F,
            _ => bail!("Specified argument was out of the range of valid values. (Parameter 'id')"),
        };
        Ok(self.read_raw(item)? as f64 / 1000.0)
    }

    fn is_sa(id: u32) -> Result<bool> {
        match id {
            Self::SA_ID => Ok(true),
            Self::CORE_ID | Self::L2_ID | Self::RING_ID => Ok(false),
            _ => bail!("Unsupported ASUS voltage item"),
        }
    }

    /// Decodes the 92-byte voltage descriptor returned for `id` (the C# default is `CORE_ID`).
    pub fn parse_svid(buffer: &[u8], size: u32, id: u32) -> Result<SvidState> {
        let sa = Self::is_sa(id)?;
        if size != 92 || (buffer.len() as u64) < size as u64 {
            bail!("Unsupported ASUS voltage descriptor size");
        }
        let w = |index: usize| u32::from_le_bytes([buffer[index * 4], buffer[index * 4 + 1], buffer[index * 4 + 2], buffer[index * 4 + 3]]);
        if w(1) != 0
            || w(5) != (if sa { 0 } else { 0x00FFFC19 })
            || w(6) != 1
            || w(7) != (if sa { 1000 } else { 1999 })
            || w(8) != 1
            || w(12) != (if sa { 700 } else { 250 })
            || w(13) != 1
            || w(14) != (if sa { 1101 } else { 1671 })
            || w(15) != 2
            || w(19) != 0
            || w(20) != 1
            || w(21) != 2
            || w(22) != u32::MAX
        {
            bail!("Unsupported ASUS voltage descriptor layout");
        }
        let state = SvidState::new(w(2), w(9), w(16));
        Self::validate_state(state, id)?;
        Ok(state)
    }

    fn validate_state(state: SvidState, id: u32) -> Result<()> {
        let sa = Self::is_sa(id)?;
        if state.offset_index > (if sa { 999 } else { 1998 }) || state.target_index > (if sa { 1100 } else { 1670 }) || state.mode > 1 {
            bail!("Invalid ASUS voltage state");
        }
        Ok(())
    }

    /// Reads the current SVID state of a voltage item (the C# default is `CORE_ID`).
    pub fn read_svid(&self, id: u32) -> Result<SvidState> {
        let _ = Self::is_sa(id)?;
        let (status, bytes, size) = self.vendor.run(move |v| {
            let mut buffer = com::Variant::bytes(&[0u8; 4096])?;
            let mut size = 4096u32;
            let status = v
                .call(
                    "iAcpiGetItemBufferRef",
                    &mut [com::Arg::U4(id), com::Arg::U4(1), com::Arg::RefVariant(&mut buffer), com::Arg::RefU4(&mut size)],
                )?
                .to_u32("iAcpiGetItemBufferRef")?;
            Ok((status, buffer.byte_array(), size))
        })?;
        let Some(bytes) = bytes.filter(|_| (status & 1) != 0) else {
            bail!("ASUS voltage metadata read failed");
        };
        Self::parse_svid(&bytes, size, id)
    }

    /// The 4096-byte request buffer for a Manual/Auto SVID state (the C# default is `CORE_ID`).
    pub fn svid_payload(state: SvidState, id: u32) -> Result<Vec<u8>> {
        Self::validate_state(state, id)?;
        let words: [u32; 7] = [0, state.offset_index, 1, state.target_index, 2, state.mode, u32::MAX];
        let mut bytes = vec![0u8; 4096];
        for (i, w) in words.iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        Ok(bytes)
    }

    fn write_svid(&self, state: SvidState, id: u32) -> Result<()> {
        let payload = Self::svid_payload(state, id)?;
        let status = self.vendor.run(move |v| {
            let mut buffer = com::Variant::bytes(&payload)?;
            let mut size = 4096u32;
            v.call(
                "iAcpiSetItemBufferRef",
                &mut [com::Arg::U4(id), com::Arg::U4(1), com::Arg::RefVariant(&mut buffer), com::Arg::RefU4(&mut size)],
            )?
            .to_u32("iAcpiSetItemBufferRef")
        })?;
        if (status & 1) == 0 {
            bail!("ASUS rejected the voltage request");
        }
        native::sleep_ms(200);
        if self.read_svid(id)? != state {
            bail!("ASUS voltage mode/target/offset did not read back");
        }
        Ok(())
    }

    /// Applies a full SVID state, restoring the previous one if it does not read back
    /// (the C# default `id` is `CORE_ID`).
    pub fn set_svid(&self, state: SvidState, id: u32) -> Result<()> {
        Self::validate_state(state, id)?;
        let before = self.read_svid(id)?;
        if state == before {
            return Ok(());
        }
        if let Err(failure) = self.write_svid(state, id) {
            if let Err(restore) = self.write_svid(before, id) {
                bail!("{failure}; VOLTAGE RESTORE FAILED: {restore}");
            }
            return Err(failure);
        }
        Ok(())
    }

    pub fn set_core_voltage(&self, volts: f64) -> Result<()> {
        if !volts.is_finite() || volts < 0.6 || volts > 1.7 {
            bail!("Specified argument was out of the range of valid values. (Parameter 'volts')");
        }
        let current = self.read_svid(Self::CORE_ID)?;
        let target = SvidState { target_index: checked_round_u32(volts * 1000.0 - 250.0)?, mode: 1, ..current };
        self.set_svid(target, Self::CORE_ID)
    }

    pub fn read_core_target(&self) -> Result<Option<f64>> {
        self.read_voltage_target(Self::CORE_ID)
    }

    pub fn read_voltage_target(&self, id: u32) -> Result<Option<f64>> {
        let state = self.read_svid(id)?;
        Ok(if state.mode == 1 {
            Some((state.target_index as f64 + if Self::is_sa(id)? { 700.0 } else { 250.0 }) / 1000.0)
        } else {
            None
        })
    }

    pub fn with_voltage_target(id: u32, state: SvidState, volts: f64) -> Result<SvidState> {
        let sa = Self::is_sa(id)?;
        Self::validate_state(state, id)?;
        if !volts.is_finite() || volts < 0.7 || volts > 1.52 {
            bail!("Specified argument was out of the range of valid values. (Parameter 'volts')");
        }
        let mv = volts * 1000.0;
        if (mv - mv.round_ties_even()).abs() > 0.000001 {
            bail!("ASUS voltage requires 1 mV steps");
        }
        Ok(SvidState { target_index: checked_round_u32(mv - if sa { 700.0 } else { 250.0 })?, mode: 1, ..state })
    }

    pub fn set_rail_voltage(&self, id: u32, volts: f64) -> Result<()> {
        self.set_svid(Self::with_voltage_target(id, self.read_svid(id)?, volts)?, id)
    }

    // The main DIP module's live Apply method (dip4.dll 0x5235F8) passes option 2.
    // Its separate staging method (0x52369C) passes option 1; staging alone can
    // acknowledge and read back without changing the physical clock.
    // On the tested Z790-A BIOS 3202, option 2 also did not move the clock.
    // Keep this diagnostic-only until physical verification succeeds.
    fn write_bclk(&self, index: u32) -> Result<()> {
        let _ = Self::decode_bclk(index)?;
        let status = self.vendor.run(move |v| {
            v.call("iAcpiSetItem", &mut [com::Arg::U4(Self::BCLK_ID), com::Arg::U4(index), com::Arg::U4(2)])?
                .to_u32("iAcpiSetItem")
        })?;
        if (status & 1) == 0 {
            bail!("ASUS BCLK setter failed: 0x{status:08X}");
        }
        native::sleep_ms(150);
        if self.read_raw(Self::BCLK_ID)? != index {
            bail!("ASUS did not retain the requested BCLK target");
        }
        Ok(())
    }

    pub fn set_bclk(&self, target: f64, measure: &dyn Fn() -> Option<f64>, log: &dyn Fn(&str)) -> Result<()> {
        Self::change_bclk(target, &|| self.read_raw(Self::BCLK_ID), &|index| self.write_bclk(index), measure, log)
    }

    pub fn change_bclk(
        target: f64,
        read: &dyn Fn() -> Result<u32>,
        write: &dyn Fn(u32) -> Result<()>,
        measure: &dyn Fn() -> Option<f64>,
        log: &dyn Fn(&str),
    ) -> Result<()> {
        let goal = Self::encode_bclk(target)?;
        let start = read()?;
        let mut current = Self::decode_bclk(start)?;
        if goal == start {
            return Ok(());
        }
        let before = measure();
        let measured = match before {
            Some(m) if m.is_finite() && (m - current).abs() <= 0.5 => m,
            _ => bail!(
                "BCLK measurement disagrees with ASUS target ({} vs {current:.3} MHz); nothing changed",
                fmt3(before)
            ),
        };
        // The unhalted counter can under-read a nominal clock due to pauses or spread spectrum.
        // Verify movement relative to the measured baseline, and report the absolute reading too.
        let measurement_offset = measured - current;
        let stepped = (|| -> Result<()> {
            while (target - current).abs() > 0.005 {
                current += (target - current).clamp(-0.5, 0.5);
                let step = Self::encode_bclk(current)?;
                write(step)?;
                if read()? != step {
                    bail!("ASUS did not retain the requested BCLK target");
                }
                let actual = measure();
                log(&format!("ASUS BCLK target {current:.3} MHz; measured {} MHz", fmt3(actual)));
                match actual {
                    Some(value) if value.is_finite() && (value - current - measurement_offset).abs() <= 0.15 => {}
                    _ => bail!("Physical BCLK did not follow the requested change"),
                }
            }
            Ok(())
        })();
        if let Err(failure) = stepped {
            let restore = (|| -> Result<()> {
                write(start)?;
                if read()? != start {
                    bail!("Restored target was not retained");
                }
                log(&format!(
                    "ASUS BCLK target restored to {:.3} MHz; measured {} MHz",
                    Self::decode_bclk(start)?,
                    fmt3(measure())
                ));
                Ok(())
            })();
            if let Err(restore) = restore {
                bail!("{failure}; RESTORE FAILED: {restore}");
            }
            return Err(failure);
        }
        Ok(())
    }

    pub fn restore(&self, measure: &dyn Fn() -> Option<f64>, log: &dyn Fn(&str)) -> Result<()> {
        self.set_bclk(self.baseline_mhz, measure, log)
    }
}

/// Late-bound COM access to the vendor object (the C# `dynamic` / `InvokeMember` path).
mod com {
    use std::mem::ManuallyDrop;
    use std::sync::Mutex;
    use std::sync::mpsc::{self, Sender};
    use std::thread::JoinHandle;

    use anyhow::{Result, anyhow, bail};
    use windows::Win32::Foundation::DISP_E_EXCEPTION;
    use windows::Win32::System::Com::{
        CLSCTX_SERVER, CLSIDFromProgID, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
        DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET, DISPPARAMS, EXCEPINFO, IDispatch, SAFEARRAY,
    };
    use windows::Win32::System::Ole::{
        SafeArrayAccessData, SafeArrayCreateVector, SafeArrayGetDim, SafeArrayGetLBound, SafeArrayGetUBound,
        SafeArrayUnaccessData,
    };
    use windows::Win32::System::Variant::{VARENUM, VARIANT, VariantClear};
    use windows::core::{GUID, HSTRING, Interface, PCWSTR};

    use crate::hw::native::lock;

    pub const INIT: &str = "iAcpiInit";

    // VARENUM values.
    const VT_EMPTY: u16 = 0;
    const VT_NULL: u16 = 1;
    const VT_I2: u16 = 2;
    const VT_I4: u16 = 3;
    const VT_R4: u16 = 4;
    const VT_R8: u16 = 5;
    const VT_BSTR: u16 = 8;
    const VT_DISPATCH: u16 = 9;
    const VT_BOOL: u16 = 11;
    const VT_VARIANT: u16 = 12;
    const VT_UNKNOWN: u16 = 13;
    const VT_I1: u16 = 16;
    const VT_UI1: u16 = 17;
    const VT_UI2: u16 = 18;
    const VT_UI4: u16 = 19;
    const VT_I8: u16 = 20;
    const VT_UI8: u16 = 21;
    const VT_INT: u16 = 22;
    const VT_UINT: u16 = 23;
    const VT_ARRAY: u16 = 0x2000;
    const VT_BYREF: u16 = 0x4000;

    /// Locale passed to GetIDsOfNames/Invoke, as the .NET COM binder does.
    const LCID: u32 = 0;

    type Job = Box<dyn FnOnce(&Dispatch) + Send>;

    /// Owner of the vendor object: an STA thread that created it and runs every call on it.
    pub struct Vendor {
        jobs: Mutex<Option<Sender<Job>>>,
        thread: Mutex<Option<JoinHandle<()>>>,
    }

    impl Vendor {
        /// `Type.GetTypeFromProgID(progId)` + `Activator.CreateInstance` on a new STA thread.
        pub fn create(prog_id: &str) -> Result<Vendor> {
            let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
            let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<()>>(1);
            let prog_id = prog_id.to_string();
            let thread = std::thread::Builder::new().name("asus-com".into()).spawn(move || {
                // Apartment-threaded, like the C# STA main thread that owned the object.
                let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
                let created = (|| -> Result<Dispatch> {
                    if hr.is_err() {
                        bail!("COM could not be initialised: {}", hr.message());
                    }
                    let Ok(clsid) = (unsafe { CLSIDFromProgID(&HSTRING::from(prog_id.as_str())) }) else {
                        bail!("ASUS control service is not installed");
                    };
                    let disp: IDispatch = unsafe { CoCreateInstance(&clsid, None, CLSCTX_SERVER) }.map_err(|e| {
                        anyhow!(
                            "Retrieving the COM class factory for component with CLSID {{{clsid:?}}} failed due to the following error: {:08x} {} (0x{:08X}).",
                            e.code().0 as u32,
                            e.message(),
                            e.code().0 as u32
                        )
                    })?;
                    Ok(Dispatch(disp))
                })();
                match created {
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                    Ok(vendor) => {
                        let _ = ready_tx.send(Ok(()));
                        // Runs until the owning `Vendor` drops its sender.
                        for job in jobs_rx {
                            job(&vendor);
                        }
                        drop(vendor); // Marshal.FinalReleaseComObject
                    }
                }
                if hr.is_ok() {
                    unsafe { CoUninitialize() };
                }
            })?;
            let vendor = Vendor { jobs: Mutex::new(Some(jobs_tx)), thread: Mutex::new(Some(thread)) };
            match ready_rx.recv() {
                Ok(Ok(())) => Ok(vendor),
                Ok(Err(e)) => Err(e),
                Err(_) => bail!("ASUS COM thread ended unexpectedly"),
            }
        }

        /// Runs `f` on the COM thread and waits for its result.
        pub fn run<R: Send + 'static>(&self, f: impl FnOnce(&Dispatch) -> Result<R> + Send + 'static) -> Result<R> {
            let jobs = lock(&self.jobs).clone().ok_or_else(|| anyhow!("ASUS control service was released"))?;
            let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<R>>(1);
            jobs.send(Box::new(move |d| {
                let _ = reply_tx.send(f(d));
            }))
            .map_err(|_| anyhow!("ASUS COM thread ended unexpectedly"))?;
            reply_rx.recv().map_err(|_| anyhow!("ASUS COM thread ended unexpectedly"))?
        }
    }

    impl Drop for Vendor {
        fn drop(&mut self) {
            // Closing the channel ends the COM thread, which releases the object.
            lock(&self.jobs).take();
            if let Some(t) = lock(&self.thread).take() {
                let _ = t.join();
            }
        }
    }

    /// One positional argument, as the C# call site's static type marshals it.
    pub enum Arg<'a> {
        /// `uint` → VT_UI4
        U4(u32),
        /// `int` → VT_I4
        I4(i32),
        /// `ref uint` → VT_BYREF | VT_UI4
        RefU4(&'a mut u32),
        /// `ref object` → VT_BYREF | VT_VARIANT
        RefVariant(&'a mut Variant),
    }

    fn raw_variant(vt: u16) -> VARIANT {
        let mut v = VARIANT::default();
        unsafe { (*v.Anonymous.Anonymous).vt = VARENUM(vt) };
        v
    }

    impl Arg<'_> {
        fn to_variant(&mut self) -> VARIANT {
            unsafe {
                match self {
                    Arg::U4(x) => {
                        let mut v = raw_variant(VT_UI4);
                        (*v.Anonymous.Anonymous).Anonymous.ulVal = *x;
                        v
                    }
                    Arg::I4(x) => {
                        let mut v = raw_variant(VT_I4);
                        (*v.Anonymous.Anonymous).Anonymous.lVal = *x;
                        v
                    }
                    Arg::RefU4(r) => {
                        let mut v = raw_variant(VT_BYREF | VT_UI4);
                        (*v.Anonymous.Anonymous).Anonymous.byref = (&mut **r as *mut u32).cast();
                        v
                    }
                    Arg::RefVariant(r) => {
                        let mut v = raw_variant(VT_BYREF | VT_VARIANT);
                        (*v.Anonymous.Anonymous).Anonymous.pvarVal = &mut r.0;
                        v
                    }
                }
            }
        }
    }

    /// An owned VARIANT, cleared on drop.
    pub struct Variant(VARIANT);

    impl Drop for Variant {
        fn drop(&mut self) {
            unsafe {
                let _ = VariantClear(&mut self.0);
            }
        }
    }

    /// A VARIANT's value as the .NET COM binder would box it.
    enum Value {
        Null,
        Signed(i64, &'static str),
        Unsigned(u64, &'static str),
        Float(f64, &'static str),
        Bool(bool),
        Str(String),
        Other(u16),
    }

    impl Variant {
        fn empty() -> Self {
            Variant(VARIANT::default())
        }

        /// A `byte[]` boxed in a VARIANT (VT_ARRAY | VT_UI1).
        pub fn bytes(data: &[u8]) -> Result<Self> {
            unsafe {
                let psa = SafeArrayCreateVector(VARENUM(VT_UI1), 0, data.len() as u32);
                if psa.is_null() {
                    bail!("Insufficient memory to continue the execution of the program.");
                }
                let mut v = raw_variant(VT_ARRAY | VT_UI1);
                (*v.Anonymous.Anonymous).Anonymous.parray = psa;
                let owned = Variant(v);
                let mut p = std::ptr::null_mut();
                SafeArrayAccessData(psa, &mut p)?;
                std::ptr::copy_nonoverlapping(data.as_ptr(), p.cast::<u8>(), data.len());
                SafeArrayUnaccessData(psa)?;
                Ok(owned)
            }
        }

        fn vt(&self) -> u16 {
            unsafe { self.0.Anonymous.Anonymous.vt.0 }
        }

        /// The contents if this holds a one-dimensional, zero-based `byte[]` (C# `is byte[]`).
        pub fn byte_array(&self) -> Option<Vec<u8>> {
            unsafe {
                let vt = self.vt();
                let psa: *mut SAFEARRAY = if vt == VT_ARRAY | VT_UI1 {
                    self.0.Anonymous.Anonymous.Anonymous.parray
                } else if vt == VT_BYREF | VT_ARRAY | VT_UI1 {
                    let pp = self.0.Anonymous.Anonymous.Anonymous.pparray;
                    if pp.is_null() { std::ptr::null_mut() } else { *pp }
                } else {
                    return None;
                };
                if psa.is_null() || SafeArrayGetDim(psa) != 1 || (*psa).cbElements != 1 {
                    return None;
                }
                let lb = SafeArrayGetLBound(psa, 1).ok()?;
                let ub = SafeArrayGetUBound(psa, 1).ok()?;
                if lb != 0 {
                    return None;
                }
                let len = (ub as i64 - lb as i64 + 1).max(0) as usize;
                let mut out = vec![0u8; len];
                if len > 0 {
                    let mut p = std::ptr::null_mut();
                    SafeArrayAccessData(psa, &mut p).ok()?;
                    std::ptr::copy_nonoverlapping(p.cast::<u8>(), out.as_mut_ptr(), len);
                    let _ = SafeArrayUnaccessData(psa);
                }
                Some(out)
            }
        }

        fn value(&self) -> Value {
            unsafe {
                let u = &self.0.Anonymous.Anonymous.Anonymous;
                match self.vt() {
                    VT_EMPTY | VT_NULL => Value::Null,
                    VT_I1 => Value::Signed(u.cVal as i64, "sbyte"),
                    VT_I2 => Value::Signed(u.iVal as i64, "short"),
                    VT_I4 => Value::Signed(u.lVal as i64, "int"),
                    VT_INT => Value::Signed(u.intVal as i64, "int"),
                    VT_I8 => Value::Signed(u.llVal, "long"),
                    VT_UI1 => Value::Unsigned(u.bVal as u64, "byte"),
                    VT_UI2 => Value::Unsigned(u.uiVal as u64, "ushort"),
                    VT_UI4 => Value::Unsigned(u.ulVal as u64, "uint"),
                    VT_UINT => Value::Unsigned(u.uintVal as u64, "uint"),
                    VT_UI8 => Value::Unsigned(u.ullVal, "ulong"),
                    VT_R4 => Value::Float(u.fltVal as f64, "float"),
                    VT_R8 => Value::Float(u.dblVal, "double"),
                    VT_BOOL => Value::Bool(u.boolVal.0 != 0),
                    VT_BSTR => Value::Str(u.bstrVal.to_string()),
                    other => Value::Other(other),
                }
            }
        }

        /// `uint x = dynamicValue;` — only the implicit numeric conversions C# allows.
        pub fn to_u32(&self, member: &str) -> Result<u32> {
            match self.value() {
                Value::Unsigned(v, "byte" | "ushort" | "uint") => Ok(v as u32),
                Value::Null => bail!("Cannot convert null to 'uint' because it is a non-nullable value type ({member})"),
                Value::Signed(_, t) | Value::Unsigned(_, t) | Value::Float(_, t) => {
                    bail!("Cannot implicitly convert type '{t}' to 'uint' ({member})")
                }
                Value::Bool(_) => bail!("Cannot implicitly convert type 'bool' to 'uint' ({member})"),
                Value::Str(_) => bail!("Cannot implicitly convert type 'string' to 'uint' ({member})"),
                Value::Other(vt) => bail!("Cannot convert VARIANT type {vt} to 'uint' ({member})"),
            }
        }

        /// `(uint)dynamicValue` in an unchecked context: any numeric type, wrapping.
        pub fn to_u32_explicit(&self) -> Result<u32> {
            match self.value() {
                Value::Signed(v, _) => Ok(v as u32),
                Value::Unsigned(v, _) => Ok(v as u32),
                Value::Float(v, _) => Ok(v as u32),
                Value::Null => bail!("Cannot convert null to 'uint' because it is a non-nullable value type"),
                Value::Bool(_) => bail!("Cannot convert type 'bool' to 'uint'"),
                Value::Str(_) => bail!("Cannot convert type 'string' to 'uint'"),
                Value::Other(vt) => bail!("Cannot convert VARIANT type {vt} to 'uint'"),
            }
        }

        /// `checked((int)dynamicValue)`.
        pub fn to_i32_checked(&self) -> Result<i32> {
            const OVERFLOW: &str = "Arithmetic operation resulted in an overflow.";
            match self.value() {
                Value::Signed(v, _) => i32::try_from(v).map_err(|_| anyhow!(OVERFLOW)),
                Value::Unsigned(v, _) => i32::try_from(v).map_err(|_| anyhow!(OVERFLOW)),
                Value::Float(v, _) => {
                    if v > -2147483649.0 && v < 2147483648.0 {
                        Ok(v as i32)
                    } else {
                        bail!(OVERFLOW)
                    }
                }
                Value::Null => bail!("Cannot convert null to 'int' because it is a non-nullable value type"),
                Value::Bool(_) => bail!("Cannot convert type 'bool' to 'int'"),
                Value::Str(_) => bail!("Cannot convert type 'string' to 'int'"),
                Value::Other(vt) => bail!("Cannot convert VARIANT type {vt} to 'int'"),
            }
        }

        /// `(string)dynamicValue`: a BSTR, or `None` for an empty/null VARIANT.
        pub fn to_string_cast(&self) -> Result<Option<String>> {
            match self.value() {
                Value::Str(s) => Ok(Some(s)),
                Value::Null => Ok(None),
                Value::Signed(_, t) | Value::Unsigned(_, t) | Value::Float(_, t) => bail!("Cannot convert type '{t}' to 'string'"),
                Value::Bool(_) => bail!("Cannot convert type 'bool' to 'string'"),
                Value::Other(vt) => bail!("Cannot convert VARIANT type {vt} to 'string'"),
            }
        }

        /// The value as C# string interpolation prints the boxed object.
        pub fn display(&self) -> String {
            match self.value() {
                Value::Null => String::new(),
                Value::Signed(v, _) => v.to_string(),
                Value::Unsigned(v, _) => v.to_string(),
                Value::Float(v, _) => v.to_string(),
                Value::Bool(b) => (if b { "True" } else { "False" }).to_string(),
                Value::Str(s) => s,
                Value::Other(_) => "System.__ComObject".to_string(),
            }
        }

        /// The object reference this holds (a nested late-bound object).
        pub fn to_dispatch(&self, member: &str) -> Result<Dispatch> {
            unsafe {
                let u = &self.0.Anonymous.Anonymous.Anonymous;
                let disp = match self.vt() {
                    VT_DISPATCH => (*u.pdispVal).clone(),
                    VT_UNKNOWN => (*u.punkVal).as_ref().and_then(|p| p.cast::<IDispatch>().ok()),
                    _ => None,
                };
                disp.map(Dispatch).ok_or_else(|| anyhow!("ASUS {member} did not return an object"))
            }
        }
    }

    /// A late-bound object. Only ever touched on the COM thread that owns it.
    pub struct Dispatch(IDispatch);

    impl Dispatch {
        /// `obj.Name(args…)`.
        pub fn call(&self, name: &str, args: &mut [Arg<'_>]) -> Result<Variant> {
            self.invoke(name, DISPATCH_FLAGS(DISPATCH_METHOD.0 | DISPATCH_PROPERTYGET.0), args)
        }

        /// `obj.Name` (property get).
        pub fn get(&self, name: &str) -> Result<Variant> {
            self.invoke(name, DISPATCH_PROPERTYGET, &mut [])
        }

        fn invoke(&self, name: &str, flags: DISPATCH_FLAGS, args: &mut [Arg<'_>]) -> Result<Variant> {
            let wname = HSTRING::from(name);
            let names = [PCWSTR(wname.as_ptr())];
            let mut dispid = 0i32;
            unsafe { self.0.GetIDsOfNames(&GUID::zeroed(), names.as_ptr(), 1, LCID, &mut dispid) }
                .map_err(|e| anyhow!("ASUS control object has no member '{name}': {}", e.message()))?;
            // DISPPARAMS takes the arguments in reverse order.
            let mut rgvarg: Vec<VARIANT> = args.iter_mut().rev().map(|a| a.to_variant()).collect();
            let params = DISPPARAMS {
                rgvarg: if rgvarg.is_empty() { std::ptr::null_mut() } else { rgvarg.as_mut_ptr() },
                rgdispidNamedArgs: std::ptr::null_mut(),
                cArgs: rgvarg.len() as u32,
                cNamedArgs: 0,
            };
            let mut result = Variant::empty();
            let mut excep = EXCEPINFO::default();
            let mut arg_err = 0u32;
            let r = unsafe {
                self.0.Invoke(dispid, &GUID::zeroed(), LCID, flags, &params, Some(&mut result.0), Some(&mut excep), Some(&mut arg_err))
            };
            // The argument VARIANTs own nothing (values or pointers into caller storage).
            drop(rgvarg);
            let description = unsafe {
                if r.as_ref().err().is_some_and(|e| e.code() == DISP_E_EXCEPTION) {
                    if let Some(fill) = excep.pfnDeferredFillIn {
                        let _ = fill(&mut excep);
                    }
                }
                let d = excep.bstrDescription.to_string();
                ManuallyDrop::drop(&mut excep.bstrSource);
                ManuallyDrop::drop(&mut excep.bstrDescription);
                ManuallyDrop::drop(&mut excep.bstrHelpFile);
                d
            };
            match r {
                Ok(()) => Ok(result),
                Err(_) if !description.is_empty() => Err(anyhow!("{description}")),
                Err(e) => Err(anyhow!("{}", e.message())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    type A = AsusBoardControl;

    fn words_to_bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn asus_bclk_metadata_maps_nominal_target_precisely() {
        assert!(A::decode_bclk(6000).unwrap() == 100.0, "Default mapping");
        assert!(A::encode_bclk(101.0).unwrap() == 6100, "101 MHz mapping");
        assert!(A::encode_bclk(f64::NAN).is_err());
        assert!(A::encode_bclk(101.001).is_err());
    }

    #[test]
    fn asus_bclk_steps_are_bounded_and_physical_movement_is_verified() {
        let raw = Cell::new(6025u32);
        let writes = RefCell::new(Vec::<u32>::new());
        A::change_bclk(
            101.0,
            &|| Ok(raw.get()),
            &|v| {
                writes.borrow_mut().push(v);
                raw.set(v);
                Ok(())
            },
            &|| Some(A::decode_bclk(raw.get()).unwrap() - 0.25),
            &|_| {},
        )
        .unwrap();
        assert!(*writes.borrow() == vec![6075, 6100], "Unbounded or wrong steps");
    }

    #[test]
    fn asus_acknowledged_but_physically_ignored_clock_restores() {
        let raw = Cell::new(6025u32);
        let r = A::change_bclk(
            101.0,
            &|| Ok(raw.get()),
            &|v| {
                raw.set(v);
                Ok(())
            },
            &|| Some(100.0),
            &|_| {},
        );
        assert!(r.is_err());
        assert!(raw.get() == 6025, "Failed change not rolled back");
    }

    #[test]
    fn asus_invalid_measurement_refuses_all_writes() {
        let writes = Cell::new(0);
        let r = A::change_bclk(
            101.0,
            &|| Ok(6025),
            &|_| {
                writes.set(writes.get() + 1);
                Ok(())
            },
            &|| Some(f64::NAN),
            &|_| {},
        );
        assert!(r.is_err());
        assert!(writes.get() == 0, "Wrote without valid measurement");
    }

    #[test]
    fn asus_svid_descriptors_distinguish_current_state_from_defaults() {
        let words = [
            0, 0, 999, 0x08000000, 0, 0x00FFFC19, 1, 1999, 1, 1022, 0x08000000, 750, 250, 1, 1671, 2, 0, 0x08000000, 0, 0,
            1, 2, u32::MAX,
        ];
        let mut bytes = words_to_bytes(&words);
        assert!(A::parse_svid(&bytes, 92, A::CORE_ID).unwrap() == SvidState::new(999, 1022, 0), "Read defaults instead of current indices");
        assert!(A::parse_svid(&bytes, 88, A::CORE_ID).is_err());
        bytes[48] = 0;
        assert!(A::parse_svid(&bytes, 92, A::CORE_ID).is_err());
    }

    #[test]
    fn asus_manual_payload_preserves_offset_and_terminates_descriptor_pairs() {
        let payload = A::svid_payload(SvidState::new(999, 1050, 1), A::CORE_ID).unwrap();
        let words: Vec<u32> = payload[..28].chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        assert!(words == vec![0, 999, 1, 1050, 2, 1, u32::MAX], "Wrong manual voltage encoding");
        assert!(payload.len() == 4096, "Vendor expects a preallocated buffer");
        assert!(A::svid_payload(SvidState::new(999, 1050, 2), A::CORE_ID).is_err());
    }

    #[test]
    fn asus_sa_descriptor_uses_its_700_mv_minimum_instead_of_the_core_minimum() {
        let words = [
            0, 0, 0, 0x08000000, 0, 0, 1, 1000, 1, 0, 0x08000000, 300, 700, 1, 1101, 2, 0, 0x08000000, 0, 0, 1, 2, u32::MAX,
        ];
        let bytes = words_to_bytes(&words);
        let state = A::parse_svid(&bytes, 92, A::SA_ID).unwrap();
        let target = A::with_voltage_target(A::SA_ID, state, 1.1).unwrap();
        assert!(target == SvidState::new(0, 400, 1), "SA request used core voltage encoding");
        let payload = A::svid_payload(target, A::SA_ID).unwrap();
        assert!(u32::from_le_bytes(payload[12..16].try_into().unwrap()) == 400, "SA payload lost the correct index");
        assert!(A::parse_svid(&bytes, 92, A::L2_ID).is_err());
    }

    #[test]
    fn asus_l2_voltage_preserves_its_offset_while_entering_manual_mode() {
        let state = SvidState::new(974, 0, 0);
        let target = A::with_voltage_target(A::L2_ID, state, 1.1).unwrap();
        assert!(target == SvidState::new(974, 850, 1), "L2 offset changed or target mapped incorrectly");
        assert!(A::with_voltage_target(A::L2_ID, state, 1.1005).is_err());
        assert!(A::with_voltage_target(A::L2_ID, state, f64::NAN).is_err());
        assert!(A::with_voltage_target(0x030D0099, state, 1.1).is_err());
    }

    #[test]
    fn asus_sa_state_bounds_reject_core_style_offsets_and_oversized_targets() {
        assert!(A::svid_payload(SvidState::new(1000, 400, 1), A::SA_ID).is_err());
        assert!(A::svid_payload(SvidState::new(0, 1101, 1), A::SA_ID).is_err());
    }

    #[test]
    fn asus_cache_svid_matches_the_captured_descriptor_and_preserves_adaptive_offset() {
        let words = [
            0, 0, 999, 134217728, 0, 16776217, 1, 1999, 1, 0, 134217728, 750, 250, 1, 1671, 2, 0, 134217728, 0, 0, 1, 2,
            u32::MAX,
        ];
        let bytes = words_to_bytes(&words);
        let state = A::parse_svid(&bytes, 92, A::RING_ID).unwrap();
        assert!(state == SvidState::new(999, 0, 0), "Cache Auto state decoded incorrectly");
        let target = A::with_voltage_target(A::RING_ID, state, 1.3).unwrap();
        assert!(target == SvidState::new(999, 1050, 1), "Cache target changed offset or selected wrong mode");
        let payload = A::svid_payload(target, A::RING_ID).unwrap();
        assert!(u32::from_le_bytes(payload[12..16].try_into().unwrap()) == 1050, "Cache target index differs from captured encoding");
        assert!(A::parse_svid(&bytes, 92, A::SA_ID).is_err());
    }

    #[test]
    fn non_asus_board_is_not_probed() {
        let (control, status) = A::try_create("Micro-Star International Co., Ltd.");
        assert!(control.is_none());
        assert_eq!(status, "not an ASUS board");
    }

    /// Talks to the installed vendor service, if any (`cargo test -- --ignored`).
    #[test]
    #[ignore]
    fn try_create_smoke() {
        let (control, status) = A::try_create("ASUSTeK COMPUTER INC.");
        println!("{status} (created: {})", control.is_some());
        assert!(!status.is_empty());
    }
}
