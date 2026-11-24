# Roch CPU on Linux

The same window, rows and safety rules as on Windows, on Wayland or X11. There is no driver to install: Linux already has the primitives, and the kernel's own interfaces are used.

## How it reaches the hardware

The window runs as your user. Register access needs root, and a Wayland client should not run as root: the compositor socket, settings and fonts belong to the person at the desk. So at launch Roch CPU starts itself a second time through `pkexec` (`roch-cpu --hardware-helper`), and the two talk over a pipe, one register call per message. Closing the window closes the pipe, and the helper exits. Roch Viewer uses the same design.

Run as root (`sudo roch-cpu --probe`), Roch CPU uses the interfaces directly and no helper starts. `ROCHCPU_NO_HELPER=1` stops it asking at all, and `ROCHCPU_LOG=1` mirrors the Log window to stderr.

| Windows (WinRing0) | Linux |
| --- | --- |
| RDMSR / WRMSR | `/dev/cpu/N/msr` (the `msr` module, loaded on demand) |
| I/O ports | `in`/`out` after `iopl(3)` |
| PCI configuration | `/sys/bus/pci/devices/*/config`, never CF8/CFC, which the kernel uses itself |
| PCH / FCH SMBus registers | the kernel's adapter through `/dev/i2c-N` (`i2c-dev`, loaded on demand), so its lock serialises Roch CPU with `spd5118` and other kernel users |
| `GetSystemFirmwareTable` | `/sys/firmware/acpi/tables`, `/sys/firmware/dmi/tables`, and `/sys/class/dmi/id` for the board name |
| `Global\Access_*` mutexes | `flock` on `.rochviewer-Global_Access_*.lock` in `/run/user/<uid>`, the files Roch Viewer and Roch Monitor take |

The helper refuses everything Roch CPU has no use for, however it is asked:

- **MSR writes** only to 0x8B (microcode signature), 0x150 (OC mailbox), 0x1AD (P-core turbo), 0x610 (power limits), 0x620 (ring), 0x650 (E-core turbo) and 0x38D/0x38F (the fixed counter the BCLK measurement borrows and restores).
- **Ports** below 0x200 only at the Super I/O and EC index pairs; never 0xCF8-0xCFF (0xCF9 resets the machine).
- **PCI writes** only to the AMD SMN index/data pair on the host bridge.
- **SMBus** only to DDR5 SPD hubs, PMICs and DIMM thermal sensors, and writes only to PMICs.
- **Files** only under the firmware table directories.

## What is different from Windows

- **Kernel lockdown** (`/sys/kernel/security/lockdown` not `[none]`, usually from Secure Boot) refuses MSR and port access to every program; the helper says so and the rows stay unavailable. `msr.allow_writes=off` refuses MSR writes the same way.
- The kernel logs a warning the first time a program writes an MSR it does not know, and marks itself tainted. That is expected; it is the same write a Windows tool makes.
- A Super I/O already driven by a kernel hwmon driver (`it87`, `nct6775`) is left alone: its bank register cannot be shared safely.
- Not available on Linux: ASUS board control (it uses ASUS's Windows service), the AMD SMU power-table read-back (PawnIO), and the Hyper-V restricted-MSR log.
- Reports from the diagnostic switches go to the current directory.

## Arrow Lake (LGA1851)

Read off a Core Ultra 7 270K Plus (family 6, model 0xC6, stepping 2, microcode 0x122) on a Gigabyte Z890 AORUS TACHYON ICE, BIOS F7a, with `--mailbox-scan`:

| What | Arrow Lake | LGA1700 |
| --- | --- | --- |
| P / E turbo tables | MSR 0x1AD / 0x650, as before | same |
| Per-core ratio ceiling | mailbox 0x1C/0x1D per core: x55 on the two favoured cores, x54 on the rest, matching the kernel's per-CPU maximum | same commands |
| Ring | MSR 0x620 (x8-x40), as the kernel's uncore driver uses | same |
| Mailbox domain 0 (core) | present, adaptive, IccMax 347 A | present |
| Domain 1 (GT) | refused: the iGPU tile is not on the mailbox | present on non-F parts |
| Domain 2 (ring) | present, same IccMax as the core: shares VccIA, so no ring voltage rows | present, shares Vcore |
| Domain 4 (SA) | present (BIOS override 1.196 V) | present |
| Domain 5 (E-core L2) | refused: no such rail | present |
| Domain 7 | x21, nothing else; probably the die-to-die clock, not exposed | absent |
| PCH SMBus | bus 0x80, device 31 function 4 | bus 0 |
| PL1 / PL2 | MSR 0x610, unlocked (250 W / 250 W) | same |

What changed for Arrow Lake:

- Model 0xC6 is identified as Core Ultra 200S (200S Plus when the brand string says so) on LGA1851; 0xC5 and 0xB5 as Arrow Lake-H/U laptops.
- The E-core L2 and GT voltage rows are left out when their domain does not answer.
- "F" parts are recognised in Core Ultra brand strings (`Ultra 7 265KF`), which carry no dash.
- The SMBus controller is looked for on bus 0x80 as well (Windows).
- IccMax is read as the 11-bit field it is; ten bits wrapped 347 A to 91 A.
- The MSI EC's core regulator, clock generator and VDD2 controls stay off on LGA1851 until an 800-series board is mapped.
- On the Z890 AORUS TACHYON ICE the ITE IT8696E's Vcore, VNNAON and VCCSA inputs are named, from Roch Viewer's confirmed map.

Writes on Arrow Lake go through the same mailbox and MSR paths that were validated on Raptor Lake, with the same read-back and rollback. Writes have not yet been checked against measured rails on Arrow Lake, so change one setting at a time.
