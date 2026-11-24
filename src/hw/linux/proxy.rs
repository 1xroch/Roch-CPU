//! The window's half: the [`KernelDriver`] that asks the helper.

use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use anyhow::{Result, anyhow, bail};

use super::super::native::{self, lock};
use super::super::{KernelDriver, KernelSmbus};
use super::HELPER_ARG;
use super::protocol::{self, Request};

struct Channel {
    /// Kept so the helper is reaped with the window rather than left a zombie.
    child: Child,
    /// `None` only while dropping.
    input: Option<ChildStdin>,
    output: ChildStdout,
}

impl Drop for Channel {
    fn drop(&mut self) {
        // Closing stdin is the helper's cue to exit; then wait so it is reaped.
        drop(self.input.take());
        let _ = self.child.wait();
    }
}

/// A running helper.
pub struct Proxy {
    channel: Mutex<Option<Channel>>,
    last_error: Mutex<Option<String>>,
}

impl Proxy {
    /// Starts this executable as the helper through `pkexec`.
    pub fn start() -> std::result::Result<Proxy, String> {
        let exe = std::env::current_exe().map_err(|e| format!("Could not find this program: {e}"))?;
        let mut pkexec = Command::new("pkexec");
        // pkexec refuses to run at all when SHELL is not listed in /etc/shells, as one installed
        // by a version manager often is, and it has no use for the value.
        pkexec.env_remove("SHELL").arg(&exe);
        Self::attempt(pkexec, "pkexec")
    }

    fn attempt(mut command: Command, label: &str) -> std::result::Result<Proxy, String> {
        command.arg(HELPER_ARG).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == io::ErrorKind::NotFound && label == "pkexec" => {
                return Err("Tuning needs administrator access, and pkexec (polkit) is not installed to ask for it. \
                            Install polkit, or start Roch CPU with sudo."
                    .into());
            }
            Err(e) => return Err(format!("Could not start {label}: {e}")),
        };
        let (Some(input), Some(mut output)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(format!("Could not talk to {label}"));
        };
        let mut hello = [0u8; protocol::HELLO.len()];
        if output.read_exact(&mut hello).is_err() || &hello != protocol::HELLO {
            // No hello at all: pkexec exited without running anything, which is what declining
            // the prompt looks like.
            let status = child.wait().ok().and_then(|s| s.code());
            return Err(no_hello_message(label, status));
        }
        let mut version = [0u8; 4];
        if output.read_exact(&mut version).is_err() {
            let _ = child.wait();
            return Err(format!("{label} stopped before it was ready"));
        }
        let version = u32::from_le_bytes(version);
        if version != protocol::VERSION {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("The helper speaks protocol version {version}, this window {}.", protocol::VERSION));
        }
        match protocol::read_frame(&mut output) {
            Ok((1, _)) => Ok(Proxy {
                channel: Mutex::new(Some(Channel { child, input: Some(input), output })),
                last_error: Mutex::new(None),
            }),
            Ok((_, reason)) => {
                let _ = child.wait();
                Err(String::from_utf8_lossy(&reason).into_owned())
            }
            Err(e) => {
                let _ = child.wait();
                Err(format!("{label} stopped before it was ready: {e}"))
            }
        }
    }

    fn call(&self, request: Request) -> Result<Vec<u8>> {
        let mut slot = lock(&self.channel);
        let Some(channel) = slot.as_mut() else {
            bail!("The register helper has stopped.");
        };
        let exchange = (|| -> io::Result<(u8, Vec<u8>)> {
            let input = channel.input.as_mut().ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?;
            input.write_all(&request.encode())?;
            input.flush()?;
            protocol::read_frame(&mut channel.output)
        })();
        match exchange {
            Ok((1, payload)) => Ok(payload),
            Ok((_, reason)) => Err(anyhow!(String::from_utf8_lossy(&reason).into_owned())),
            Err(e) => {
                // Nothing after this can be trusted to be in step.
                *slot = None;
                Err(anyhow!("The register helper has stopped: {e}"))
            }
        }
    }

    fn call_fixed<const N: usize>(&self, request: Request) -> Result<[u8; N]> {
        self.call(request)?.try_into().map_err(|_| anyhow!("The register helper gave an answer of the wrong size."))
    }

    fn record<T>(&self, r: Result<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                *lock(&self.last_error) = Some(e.to_string());
                None
            }
        }
    }

    /// -1 is "the CPU this thread is on": this thread's, not the helper's.
    fn resolve_cpu(cpu: i32) -> u32 {
        if cpu < 0 { native::current_cpu() as u32 } else { cpu as u32 }
    }

    fn port_in(&self, port: u16, width: u8) -> Result<u32> {
        Ok(u32::from_le_bytes(self.call_fixed(Request::PortIn { port, width })?))
    }

    fn port_out(&self, port: u16, width: u8, value: u32) -> Result<()> {
        self.call(Request::PortOut { port, width, value }).map(|_| ())
    }
}

/// What pkexec's exit code says. 126 is a dismissed or refused prompt, 127 no way to ask at all.
pub fn no_hello_message(label: &str, status: Option<i32>) -> String {
    if label != "pkexec" {
        return format!("{label} did not start ({status:?}).");
    }
    match status {
        Some(126) => "Tuning needs administrator access, and the prompt was declined. Open Roch CPU again to be asked again.".into(),
        Some(127) => "Tuning needs administrator access, and there is no polkit agent running to ask for it. \
                      Start Roch CPU with sudo instead."
            .into(),
        other => format!("Tuning needs administrator access, and pkexec could not provide it ({other:?})."),
    }
}

impl KernelDriver for Proxy {
    fn name(&self) -> String {
        "Linux kernel interfaces through the pkexec helper".into()
    }
    fn is_open(&self) -> bool {
        lock(&self.channel).is_some()
    }
    fn last_error(&self) -> Option<String> {
        lock(&self.last_error).clone()
    }
    fn read_msr(&self, index: u32, cpu: i32) -> Option<u64> {
        let r = self.call_fixed(Request::ReadMsr { cpu: Self::resolve_cpu(cpu), index }).map(u64::from_le_bytes);
        self.record(r)
    }
    fn write_msr(&self, index: u32, value: u64, cpu: i32) -> bool {
        let r = self.call(Request::WriteMsr { cpu: Self::resolve_cpu(cpu), index, value });
        self.record(r).is_some()
    }
    fn read_io_port_byte(&self, port: u16) -> Result<u8> {
        Ok(self.port_in(port, 1)? as u8)
    }
    fn read_io_port_word(&self, port: u16) -> Result<u16> {
        Ok(self.port_in(port, 2)? as u16)
    }
    fn read_io_port_dword(&self, port: u16) -> Result<u32> {
        self.port_in(port, 4)
    }
    fn write_io_port_byte(&self, port: u16, value: u8) -> Result<()> {
        self.port_out(port, 1, value as u32)
    }
    fn write_io_port_word(&self, port: u16, value: u16) -> Result<()> {
        self.port_out(port, 2, value as u32)
    }
    fn write_io_port_dword(&self, port: u16, value: u32) -> Result<()> {
        self.port_out(port, 4, value)
    }
    fn read_pci_config(&self, bus: u8, device: u8, function: u8, offset: u16) -> Option<u32> {
        self.read_pci_config_raw(bus, device, function, offset).filter(|&v| v != 0xFFFF_FFFF)
    }
    fn read_pci_config_raw(&self, bus: u8, device: u8, function: u8, offset: u16) -> Option<u32> {
        let r = self.call(Request::ReadPci { bus, device, function, offset });
        match self.record(r)? {
            v if v.len() == 4 => Some(u32::from_le_bytes(v.try_into().unwrap())),
            _ => None,
        }
    }
    fn write_pci_config(&self, bus: u8, device: u8, function: u8, offset: u16, value: u32) -> bool {
        let r = self.call(Request::WritePci { bus, device, function, offset, value });
        self.record(r).is_some()
    }
    fn kernel_smbus(&self, adapter: u32, address: u8, transfer: KernelSmbus) -> Result<u16> {
        Ok(u16::from_le_bytes(self.call_fixed(Request::KernelSmbus { adapter, address, transfer })?))
    }
    fn read_firmware_file(&self, path: &str) -> Result<Vec<u8>> {
        self.call(Request::ReadFirmwareFile { path: path.to_string() })
    }
    fn measure_bclk_from_cycles(&self, cpu: i32, sample_ms: i32) -> Option<Result<Option<f64>>> {
        let request = Request::MeasureBclk { cpu: Self::resolve_cpu(cpu), sample_ms: sample_ms.max(0) as u32 };
        Some(self.call(request).map(|v| v.try_into().ok().map(f64::from_le_bytes)))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in helper: a shell script given the protocol's bytes.
    fn script(text: &str) -> std::result::Result<Proxy, String> {
        let mut command = Command::new("sh");
        command.args(["-c", text]);
        Proxy::attempt(command, "test helper")
    }

    const HELLO: &str = r"printf 'ROCHCPU\n\001\000\000\000'";
    const READY: &str = r"printf '\001\000\000\000\000'";

    #[test]
    fn one_call_goes_there_and_back() {
        // A ReadMsr is 13 bytes: tag, length, cpu, index. It answers 0x1122334455667788, then the
        // same request again is refused with a reason.
        let text = format!(
            r"{HELLO}; {READY}; head -c 13 >/dev/null; printf '\001\010\000\000\000\210\167\146\125\104\063\042\021'; \
              head -c 13 >/dev/null; printf '\000\002\000\000\000no'; cat >/dev/null"
        );
        let proxy = script(&text).expect("the helper was not taken up");
        assert!(proxy.is_open());
        assert_eq!(proxy.read_msr(0x150, 0), Some(0x1122_3344_5566_7788));
        assert_eq!(proxy.read_msr(0x150, 0), None);
        assert_eq!(proxy.last_error().as_deref(), Some("no"));
        assert!(proxy.is_open(), "a refusal does not end the session");
    }

    #[test]
    fn a_helper_that_stops_is_reported_once_and_not_written_to_again() {
        let proxy = script(&format!("{HELLO}; {READY}; exit 0")).expect("the helper was not taken up");
        assert!(proxy.read_io_port_byte(0x2E).unwrap_err().to_string().contains("stopped"));
        assert!(!proxy.is_open());
        assert_eq!(proxy.read_io_port_byte(0x2E).unwrap_err().to_string(), "The register helper has stopped.");
    }

    #[test]
    fn an_answer_of_the_wrong_size_is_refused_rather_than_read_as_zero() {
        let text = format!(r"{HELLO}; {READY}; cat <&0 >/dev/null & printf '\001\001\000\000\000\377'; wait");
        let proxy = script(&text).expect("the helper was not taken up");
        assert!(proxy.read_io_port_dword(0x1808).unwrap_err().to_string().contains("wrong size"));
    }

    #[test]
    fn a_helper_that_cannot_get_access_says_why() {
        match script(&format!(r"{HELLO}; printf '\000\004\000\000\000nope'")) {
            Err(reason) => assert_eq!(reason, "nope"),
            Ok(_) => panic!("that is a refusal"),
        }
    }

    #[test]
    fn a_declined_prompt_says_so() {
        assert!(no_hello_message("pkexec", Some(126)).contains("declined"));
        assert!(no_hello_message("pkexec", Some(127)).contains("polkit agent"));
        assert!(script("exit 126").is_err());
    }
}
