//! The privileged half: read a request, make the call, write the answer.
//!
//! Nothing here decides anything. What is permitted is [`Direct`]'s business, so a request is
//! refused the same way whichever side of the pipe it came from.

use std::io::{self, Write};
use std::sync::{Arc, OnceLock};

use super::super::KernelDriver;
use super::super::bclk_meter::BclkMeter;
use super::direct::Direct;
use super::protocol::{self, Request};

/// Serves the parent until it goes away; the process's exit code. Stdout is the protocol, so
/// nothing else may be written to it; the parent closing its end is the normal way this stops.
pub fn serve() -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();

    let opened = Direct::open();
    let hello = (|| -> io::Result<()> {
        output.write_all(protocol::HELLO)?;
        output.write_all(&protocol::VERSION.to_le_bytes())?;
        protocol::write_response(&mut output, opened.as_ref().map(|_| Vec::new()).map_err(|e| e.to_string()))
    })();
    let direct = match (hello, opened) {
        (Ok(()), Ok(direct)) => Arc::new(direct),
        _ => return 1,
    };
    let meter: OnceLock<BclkMeter> = OnceLock::new();

    loop {
        let Ok((op, payload)) = protocol::read_frame(&mut input) else {
            return 0;
        };
        let result = match Request::decode(op, &payload) {
            Ok(Request::MeasureBclk { cpu, sample_ms }) => {
                let meter = meter.get_or_init(|| BclkMeter::new(direct.clone() as Arc<dyn KernelDriver>));
                match meter.measure_bclk_from_cycles(cpu.min(i32::MAX as u32) as i32, sample_ms.clamp(10, 2000) as i32) {
                    Ok(Some(mhz)) => Ok(mhz.to_le_bytes().to_vec()),
                    Ok(None) => Ok(Vec::new()),
                    Err(e) => Err(e.to_string()),
                }
            }
            Ok(request) => answer(&direct, request),
            Err(error) => Err(error.to_string()),
        };
        if protocol::write_response(&mut output, result).is_err() {
            return 0;
        }
    }
}

fn answer(d: &Direct, request: Request) -> Result<Vec<u8>, String> {
    let r: anyhow::Result<Vec<u8>> = match request {
        Request::ReadMsr { cpu, index } => d.read_msr_on(index, cpu).map(|v| v.to_le_bytes().to_vec()),
        Request::WriteMsr { cpu, index, value } => d.write_msr_on(index, value, cpu).map(|()| Vec::new()),
        Request::PortIn { port, width } => d.port_in(port, width).map(|v| v.to_le_bytes().to_vec()),
        Request::PortOut { port, width, value } => d.port_out(port, width, value).map(|()| Vec::new()),
        Request::ReadPci { bus, device, function, offset } => d.read_pci(bus, device, function, offset).map(|v| match v {
            Some(v) => v.to_le_bytes().to_vec(),
            None => Vec::new(),
        }),
        Request::WritePci { bus, device, function, offset, value } => {
            d.write_pci(bus, device, function, offset, value).map(|()| Vec::new())
        }
        Request::ReadFirmwareFile { path } => d.firmware_file(&path),
        Request::KernelSmbus { adapter, address, transfer } => {
            d.smbus(adapter, address, transfer).map(|v| v.to_le_bytes().to_vec())
        }
        Request::MeasureBclk { .. } => unreachable!("answered in serve"),
    };
    r.map_err(|e| e.to_string())
}
