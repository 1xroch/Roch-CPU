//! Observes values from existing startup operations only. It never calls a transport or driver.
//! Collection, formatting and teardown errors are deliberately isolated from hardware control.

use std::cell::RefCell;
use std::rc::Rc;

use super::core_page_probe::CorePageVerificationResult;

thread_local! {
    static CURRENT_SCOPE: RefCell<Option<Rc<ScopeInner>>> = const { RefCell::new(None) };
}

fn with_current(f: impl FnOnce(&ScopeInner)) {
    let _ = CURRENT_SCOPE.try_with(|c| {
        if let Ok(cur) = c.try_borrow() {
            if let Some(scope) = cur.as_ref() {
                f(scope);
            }
        }
    });
}

/// Begins a diagnostic scope on this thread; observations land in it until it is dropped.
pub fn begin() -> Option<Scope> {
    CURRENT_SCOPE
        .try_with(|c| {
            let mut cur = c.try_borrow_mut().ok()?;
            let inner = Rc::new(ScopeInner {
                previous: cur.clone(),
                data: RefCell::new(ScopeData::default()),
            });
            *cur = Some(inner.clone());
            Some(Scope { inner })
        })
        .ok()
        .flatten()
}

pub fn observe_ec_read(address: u16, value: u8) {
    with_current(|s| s.record_ec(format!("EC read {}(0x{address:04X})=0x{value:02X}", role(address))));
}

pub fn observe_ec_write(address: u16, value: u8, ok: bool) {
    with_current(|s| {
        s.record_ec(format!(
            "EC write {}(0x{address:04X})=0x{value:02X} result={}",
            role(address),
            if ok { "True" } else { "False" }
        ))
    });
}

pub fn observe_ec_failure(address: u16, write: bool, error: &anyhow::Error) {
    with_current(|s| {
        s.record_ec(format!(
            "EC {} {}(0x{address:04X}) threw IOException: {error}",
            if write { "write" } else { "read" },
            role(address)
        ))
    });
}

pub fn observe_register_read(address: u8, register: u8, value: Option<u16>, word: bool) {
    with_current(|s| {
        let v = match value {
            Some(v) if word => format!("0x{v:04X}"),
            Some(v) => format!("0x{:02X}", v),
            None => "failed".to_string(),
        };
        s.record_register(format!(
            "regulator address=0x{address:02X} register=0x{register:02X} {} read={v}",
            if word { "word" } else { "byte" }
        ))
    });
}

pub fn observe_ioctl_result(code: u32, requested_out_bytes: u32, ok: bool, bytes_returned: u32, error: Option<u32>) {
    with_current(|s| s.record_ioctl(code, requested_out_bytes, ok, bytes_returned, error));
}

pub fn observe_driver_not_issued(code: u32, requested_out_bytes: u32) {
    with_current(|s| {
        s.record_ec(format!(
            "IOCTL 0x{code:08X} not issued: driver not open; requestedOut={requested_out_bytes}"
        ))
    });
}

pub fn observe_probe_result(result: &CorePageVerificationResult) {
    with_current(|scope| {
        let page = |p: Option<u8>| p.map(|v| v.to_string()).unwrap_or_else(|| "not read".into());
        scope.record_result(format!(
            "startup result={} originalPAGE={} restoredPAGE={} selectionAttempted={} restorationAttempted={}",
            tf(result.succeeded),
            page(result.original_page),
            page(result.restored_page),
            tf(result.page_selection_attempted),
            tf(result.page_restoration_attempted)
        ));
        if let Some(raw) = &result.snapshot {
            scope.record_result(format!(
                "raw core snapshot F0=0x{:04X} 21=0x{:04X} 23=0x{:04X}; decodedMode={} targetMv={}",
                raw.mode_register,
                raw.target_mv,
                raw.secondary_target_mv,
                raw.mode(),
                raw.target_mv
            ));
        }
        scope.record_result(format!("startup status: {}", result.status));
    });
}

fn tf(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

fn role(address: u16) -> &'static str {
    match address {
        0x0463 => "COMMAND/staged echo",
        0x0465 => "ADDRESS/staged echo",
        0x0466 => "REGISTER/staged echo",
        0x0460 => "DOORBELL (88=transfer,C0=latch,80=idle)",
        0x0470 => "WDATA_LO/staged echo",
        0x0471 => "WDATA_HI/staged echo",
        0x04B0 => "RDATA_LO",
        0x04B1 => "RDATA_HI",
        _ => "raw",
    }
}

const MAX_EC_EVENTS: usize = 256;
const MAX_REGISTER_EVENTS: usize = 24;
const MAX_NATIVE_SAMPLES: usize = 32;
const MAX_NATIVE_FAILURES: usize = 16;
const MAX_NATIVE_SUMMARY_KEYS: usize = 32;
const MAX_LINE_CHARS: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq)]
struct IoctlObservation {
    code: u32,
    requested_out_bytes: u32,
    ok: bool,
    returned: u32,
    error: Option<u32>,
}

impl std::fmt::Display for IoctlObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "code=0x{:08X} BOOL={} bytesReturned={} requestedOutBytes={} Win32Error={}",
            self.code,
            if self.ok { "TRUE" } else { "FALSE" },
            self.returned,
            self.requested_out_bytes,
            self.error.map(|e| e.to_string()).unwrap_or_else(|| "none".into())
        )
    }
}

#[derive(Default)]
struct ScopeData {
    events: Vec<String>,
    result: Vec<String>,
    /// Insertion-ordered summary, like the .NET dictionary it replaces.
    native_counts: Vec<(IoctlObservation, usize)>,
    sequence: usize,
    ec_seen: usize,
    register_seen: usize,
    native_samples: usize,
    native_failures: usize,
    native_seen: usize,
    native_summary_dropped: usize,
}

struct ScopeInner {
    previous: Option<Rc<ScopeInner>>,
    data: RefCell<ScopeData>,
}

fn bound_line(value: String) -> String {
    if value.chars().count() <= MAX_LINE_CHARS {
        value
    } else {
        let cut: String = value.chars().take(MAX_LINE_CHARS - 12).collect();
        cut + " [truncated]"
    }
}

impl ScopeInner {
    fn record_ec(&self, value: String) {
        if let Ok(mut d) = self.data.try_borrow_mut() {
            d.sequence += 1;
            let seq = d.sequence;
            d.ec_seen += 1;
            if d.ec_seen <= MAX_EC_EVENTS {
                d.events.push(bound_line(format!("#{seq} {value}")));
            }
        }
    }

    fn record_register(&self, value: String) {
        if let Ok(mut d) = self.data.try_borrow_mut() {
            d.sequence += 1;
            let seq = d.sequence;
            d.register_seen += 1;
            if d.register_seen <= MAX_REGISTER_EVENTS {
                d.events.push(bound_line(format!("#{seq} {value}")));
            }
        }
    }

    fn record_result(&self, value: String) {
        if let Ok(mut d) = self.data.try_borrow_mut() {
            if d.result.len() < 3 {
                d.result.push(bound_line(value));
            }
        }
    }

    fn record_ioctl(&self, code: u32, requested_out_bytes: u32, ok: bool, returned: u32, error: Option<u32>) {
        if let Ok(mut d) = self.data.try_borrow_mut() {
            d.sequence += 1;
            let seq = d.sequence;
            d.native_seen += 1;
            let key = IoctlObservation { code, requested_out_bytes, ok, returned, error };
            if let Some(entry) = d.native_counts.iter_mut().find(|(k, _)| *k == key) {
                entry.1 += 1;
            } else if d.native_counts.len() < MAX_NATIVE_SUMMARY_KEYS {
                d.native_counts.push((key, 1));
            } else {
                d.native_summary_dropped += 1;
            }
            let sample = if ok {
                d.native_samples += 1;
                d.native_samples <= MAX_NATIVE_SAMPLES
            } else {
                d.native_failures += 1;
                d.native_failures <= MAX_NATIVE_FAILURES
            };
            if sample {
                d.events.push(bound_line(format!("#{seq} DeviceIoControl {key}")));
            }
        }
    }
}

/// A diagnostic scope. Dropping it makes the previous scope current again.
pub struct Scope {
    inner: Rc<ScopeInner>,
}

impl Scope {
    pub fn snapshot(&self) -> Vec<String> {
        let Ok(d) = self.inner.data.try_borrow() else { return Vec::new() };
        let mut lines = d.events.clone();
        for (k, n) in &d.native_counts {
            lines.push(format!("DeviceIoControl summary count={n} {k}"));
        }
        lines.push(format!(
            "trace bounds: ECseen={} ECcap={MAX_EC_EVENTS}; registerSeen={} registerCap={MAX_REGISTER_EVENTS}; \
             nativeSeen={} nativeSampleCap={MAX_NATIVE_SAMPLES} nativeFailureCap={MAX_NATIVE_FAILURES} \
             nativeSummaryKeys={}/{MAX_NATIVE_SUMMARY_KEYS} summaryDropped={}",
            d.ec_seen,
            d.register_seen,
            d.native_seen,
            d.native_counts.len(),
            d.native_summary_dropped
        ));
        lines.extend(d.result.iter().cloned());
        lines
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let _ = CURRENT_SCOPE.try_with(|c| {
            if let Ok(mut cur) = c.try_borrow_mut() {
                if cur.as_ref().is_some_and(|s| Rc::ptr_eq(s, &self.inner)) {
                    *cur = self.inner.previous.clone();
                }
            }
        });
    }
}
