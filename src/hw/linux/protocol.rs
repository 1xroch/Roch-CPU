//! The frames between the window and the helper.
//!
//! One request, one response, in order, over a pair of pipes. Both ends are the same build of
//! the same program, and the hello carries [`VERSION`] so a mismatch is never guessed at.
//!
//! ```text
//! request:  op:u8  length:u32  payload
//! response: ok:u8  length:u32  payload   (ok = 0: payload is the error text)
//! ```

use std::io::{self, Read, Write};

use super::super::KernelSmbus;

pub const VERSION: u32 = 1;

/// What the helper says first: this, then its [`VERSION`], then a response frame saying whether
/// it could get access.
pub const HELLO: &[u8; 8] = b"ROCHCPU\n";

/// The most one frame may carry. The largest legitimate thing is an ACPI or SMBIOS table.
pub const MAX_FRAME: usize = 1 << 22;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    ReadMsr { cpu: u32, index: u32 },
    WriteMsr { cpu: u32, index: u32, value: u64 },
    /// `width` is 1, 2 or 4 bytes.
    PortIn { port: u16, width: u8 },
    PortOut { port: u16, width: u8, value: u32 },
    ReadPci { bus: u8, device: u8, function: u8, offset: u16 },
    WritePci { bus: u8, device: u8, function: u8, offset: u16, value: u32 },
    ReadFirmwareFile { path: String },
    KernelSmbus { adapter: u32, address: u8, transfer: KernelSmbus },
    /// The core-cycle BCLK measurement, run in the helper on `cpu`. Answers an f64 in MHz, or
    /// nothing when it could not measure.
    MeasureBclk { cpu: u32, sample_ms: u32 },
}

fn bad_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(16);
        let op = match self {
            Request::ReadMsr { cpu, index } => {
                p.extend_from_slice(&cpu.to_le_bytes());
                p.extend_from_slice(&index.to_le_bytes());
                1
            }
            Request::WriteMsr { cpu, index, value } => {
                p.extend_from_slice(&cpu.to_le_bytes());
                p.extend_from_slice(&index.to_le_bytes());
                p.extend_from_slice(&value.to_le_bytes());
                2
            }
            Request::PortIn { port, width } => {
                p.extend_from_slice(&port.to_le_bytes());
                p.push(*width);
                3
            }
            Request::PortOut { port, width, value } => {
                p.extend_from_slice(&port.to_le_bytes());
                p.push(*width);
                p.extend_from_slice(&value.to_le_bytes());
                4
            }
            Request::ReadPci { bus, device, function, offset } => {
                p.extend_from_slice(&[*bus, *device, *function]);
                p.extend_from_slice(&offset.to_le_bytes());
                5
            }
            Request::WritePci { bus, device, function, offset, value } => {
                p.extend_from_slice(&[*bus, *device, *function]);
                p.extend_from_slice(&offset.to_le_bytes());
                p.extend_from_slice(&value.to_le_bytes());
                6
            }
            Request::ReadFirmwareFile { path } => {
                p.extend_from_slice(path.as_bytes());
                7
            }
            Request::KernelSmbus { adapter, address, transfer } => {
                p.extend_from_slice(&adapter.to_le_bytes());
                p.push(*address);
                let (kind, register, value) = match *transfer {
                    KernelSmbus::ReadByte { register } => (0, register, 0),
                    KernelSmbus::ReadWord { register } => (1, register, 0),
                    KernelSmbus::WriteByte { register, value } => (2, register, value),
                    KernelSmbus::WriteWord { register, .. } => (3, register, 0),
                };
                p.extend_from_slice(&[kind, register, value]);
                if let KernelSmbus::WriteWord { value, .. } = *transfer {
                    p.extend_from_slice(&value.to_le_bytes());
                }
                8
            }
            Request::MeasureBclk { cpu, sample_ms } => {
                p.extend_from_slice(&cpu.to_le_bytes());
                p.extend_from_slice(&sample_ms.to_le_bytes());
                9
            }
        };
        frame(op, &p)
    }

    pub fn decode(op: u8, p: &[u8]) -> io::Result<Request> {
        let bad = || bad_data(format!("malformed request {op}"));
        let take = |at: usize, n: usize| p.get(at..at + n).ok_or_else(bad);
        let u8_at = |at: usize| -> io::Result<u8> { Ok(take(at, 1)?[0]) };
        let u16_at = |at: usize| -> io::Result<u16> { Ok(u16::from_le_bytes(take(at, 2)?.try_into().unwrap())) };
        let u32_at = |at: usize| -> io::Result<u32> { Ok(u32::from_le_bytes(take(at, 4)?.try_into().unwrap())) };
        let u64_at = |at: usize| -> io::Result<u64> { Ok(u64::from_le_bytes(take(at, 8)?.try_into().unwrap())) };
        Ok(match op {
            1 => Request::ReadMsr { cpu: u32_at(0)?, index: u32_at(4)? },
            2 => Request::WriteMsr { cpu: u32_at(0)?, index: u32_at(4)?, value: u64_at(8)? },
            3 => Request::PortIn { port: u16_at(0)?, width: u8_at(2)? },
            4 => Request::PortOut { port: u16_at(0)?, width: u8_at(2)?, value: u32_at(3)? },
            5 => Request::ReadPci { bus: u8_at(0)?, device: u8_at(1)?, function: u8_at(2)?, offset: u16_at(3)? },
            6 => Request::WritePci {
                bus: u8_at(0)?,
                device: u8_at(1)?,
                function: u8_at(2)?,
                offset: u16_at(3)?,
                value: u32_at(5)?,
            },
            7 => Request::ReadFirmwareFile { path: String::from_utf8(p.to_vec()).map_err(|_| bad())? },
            8 => Request::KernelSmbus {
                adapter: u32_at(0)?,
                address: u8_at(4)?,
                transfer: match u8_at(5)? {
                    0 => KernelSmbus::ReadByte { register: u8_at(6)? },
                    1 => KernelSmbus::ReadWord { register: u8_at(6)? },
                    2 => KernelSmbus::WriteByte { register: u8_at(6)?, value: u8_at(7)? },
                    3 => KernelSmbus::WriteWord { register: u8_at(6)?, value: u16_at(8)? },
                    _ => return Err(bad()),
                },
            },
            9 => Request::MeasureBclk { cpu: u32_at(0)?, sample_ms: u32_at(4)? },
            _ => return Err(bad()),
        })
    }
}

/// One frame: its tag byte, its length, its payload.
pub fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(tag);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Reads one frame: `(tag, payload)`.
pub fn read_frame(reader: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    reader.read_exact(&mut head)?;
    let length = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if length > MAX_FRAME {
        return Err(bad_data(format!("a frame of {length} bytes")));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

/// Writes a response frame carrying `result`.
pub fn write_response(writer: &mut impl Write, result: Result<Vec<u8>, String>) -> io::Result<()> {
    let bytes = match result {
        Ok(payload) => frame(1, &payload),
        Err(error) => frame(0, error.as_bytes()),
    };
    writer.write_all(&bytes)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(request: Request) {
        let bytes = request.encode();
        let (op, payload) = read_frame(&mut bytes.as_slice()).unwrap();
        assert_eq!(Request::decode(op, &payload).unwrap(), request);
    }

    #[test]
    fn every_request_survives_the_pipe() {
        round_trip(Request::ReadMsr { cpu: 7, index: 0x150 });
        round_trip(Request::WriteMsr { cpu: 23, index: 0x1AD, value: 0x3939_3939_3939_3939 });
        round_trip(Request::PortIn { port: 0x2E, width: 1 });
        round_trip(Request::PortOut { port: 0xA45, width: 4, value: 0xDEAD_BEEF });
        round_trip(Request::ReadPci { bus: 0x80, device: 31, function: 4, offset: 0x20 });
        round_trip(Request::WritePci { bus: 0, device: 0, function: 0, offset: 0x60, value: 0x5A_0000 });
        round_trip(Request::ReadFirmwareFile { path: "/sys/firmware/acpi/tables/FACP".into() });
        for transfer in [
            KernelSmbus::ReadByte { register: 0x31 },
            KernelSmbus::ReadWord { register: 0x05 },
            KernelSmbus::WriteByte { register: 0x30, value: 0x05 },
            KernelSmbus::WriteWord { register: 0x21, value: 0x0528 },
        ] {
            round_trip(Request::KernelSmbus { adapter: 22, address: 0x48, transfer });
        }
        round_trip(Request::MeasureBclk { cpu: 1, sample_ms: 150 });
    }

    #[test]
    fn a_truncated_request_is_refused_rather_than_read_as_zero() {
        assert!(Request::decode(2, &[0; 11]).is_err());
        assert!(Request::decode(99, &[]).is_err());
    }

    #[test]
    fn an_oversized_frame_is_refused_before_it_is_allocated() {
        let mut bytes = vec![1u8];
        bytes.extend_from_slice(&(MAX_FRAME as u32 + 1).to_le_bytes());
        assert!(read_frame(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn responses_carry_either_the_bytes_or_the_reason() {
        let mut out = Vec::new();
        write_response(&mut out, Ok(vec![1, 2])).unwrap();
        assert_eq!(read_frame(&mut out.as_slice()).unwrap(), (1, vec![1, 2]));
        let mut out = Vec::new();
        write_response(&mut out, Err("no".into())).unwrap();
        assert_eq!(read_frame(&mut out.as_slice()).unwrap(), (0, b"no".to_vec()));
    }
}
