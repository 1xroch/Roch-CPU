# Roch CPU on Linux

The same window, rows and safety rules as on Windows, on Wayland or X11. There is no driver to install: Linux already has the primitives, and the kernel's own interfaces are used.

## How it reaches the hardware

The window runs as your user. Register access needs root, and a Wayland client should not run as root: the compositor socket, settings and fonts belong to the person at the desk. So at launch Roch CPU starts itself a second time through `pkexec` (`rochcpu --hardware-helper`), and the two talk over a pipe, one register call per message. Closing the window closes the pipe, and the helper exits. Roch Viewer uses the same design.

### No password at each launch

By default `pkexec` asks for your password every time the window starts. `sudo ~/.local/bin/rochcpu setup` stops that: it copies this program to `/usr/local/bin/rochcpu`, owned by root and writable only by root, and writes `/etc/polkit-1/rules.d/49-rochcpu.rules`, which lets the person who ran it start exactly `/usr/local/bin/rochcpu --hardware-helper` from their own active local session without a password. A rule naming the copy in `~/.local/bin` would hand out root, since anything running as you can replace that file; nobody but root can change the copy in `/usr/local/bin`.

Installed from a package (`/usr/bin/rochcpu`), there is no copy: the program is already root-owned, so the rule names it directly, and an update installed in its place keeps working with no setup to repeat.

The trade: anything running as you can then start the helper silently and do what it allows, which is changing CPU ratios, voltages and power limits and the DIMM rails. The window uses the installed copy only while it is byte for byte the running build, so after an update it asks again until setup is re-run. `rochcpu setup --status` reports the state, `rochcpu setup --show` lists what it would change, and `sudo rochcpu setup --undo` removes both files.

Install with `packaging/linux/install.sh` (the binary as `~/.local/bin/rochcpu`, a launcher entry and its icon), or run `target/release/roch-cpu` from the build directly. Run as root (`sudo rochcpu --probe`), Roch CPU uses the interfaces directly and no helper starts. `ROCHCPU_NO_HELPER=1` stops it asking at all, and `ROCHCPU_LOG=1` mirrors the Log window to stderr.

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
- The OC mailbox status codes are Intel's: Arrow Lake answers 2 (invalid domain) for the missing E-core L2 domain and 5 (not supported) for GT.
- At start-up each voltage domain is checked with a write of its own value, which changes nothing. A domain that refuses it is shown read-only with the reason, instead of rows that always fail.

### Write test

`sudo rochcpu --write-test` (or the same through `pkexec`) writes every Intel row a small step, measures the effect with something other than the register it wrote, and then writes back a raw snapshot of every register it could have touched and reads it all again. On the 270K Plus above, Linux, 2026-11-23:

| Row | Change | Measured | Result |
| --- | --- | --- | --- |
| CPU Ratio (P-Core) | x54 → x52 | all P-cores busy ran x52 | pass |
| Per-core P ratio | core 0 x50, others x54 | core 0 ran x50, core 1 x54 | pass |
| E-Core Ratio | x47 → x45 | all E-cores busy ran x45 | pass |
| Per-cluster E ratio | cluster 0 x44 | cluster 0 ran x44, cluster 1 x47 | pass |
| Ring Ratio | x40 → x38 | MSR 0x621 under load x38 | pass |
| PL1 / PL2 | 250 → 240 W | the kernel's RAPL powercap reads 240 W | pass |
| SA Voltage | 1.196 → 1.205 V | read back; the board's VCCSA input did not move a 12 mV step | pass |
| SA offset while SA is fixed | -5 mV | refused, SA kept fixed | pass |
| DIMM VDD / VDDQ / VPP, both DIMMs | +15 mV | each PMIC's ADC moved 10-15 mV | pass |
| CPU Core Voltage Offset | -10 mV | loaded VID 1.2937 → 1.2837 V (-10.0 mV); 0 put it back | pass (Undervolt Protection off) |
| CPU Core Voltage (fixed) | override 1.310 V | read back as override with target 0 and ratio x55: the CPU discards the target in this format; rolled back | read-only on Arrow Lake |
| VCCSA / VNNAON (board regulator) | +10 mV | the regulator's output followed (1.307 → 1.315 V, 0.850 → 0.858 V) | pass |
| Restore | | every register back to its start value | pass |

With the BIOS's **Undervolt Protection** on, every change to the core domain answers 0x13; with it off the same write is accepted. 0x08 means busy: straight after a voltage change the CPU refuses the next write for a moment (putting an offset back to 0 at once failed, 200 ms later it was accepted), and writing a domain's own value back answers it too. Writes retry on 0x08 for up to a second.

The board's Vcore sensor swings by several hundred millivolts at idle on Arrow Lake, so it cannot confirm a 10 mV offset; there the CPU's own VID following the request is taken as the confirmation, and the Log says so. The kernel logged "Write to unrecognized MSR ... tainting CPU_OUT_OF_SPEC" for the MSR writes, as expected.

### Board regulators: VCCSA (NGU) and VNNAON (D2D)

On the Z890 AORUS TACHYON ICE two Renesas PMBus controllers sit on the PCH's serial I2C bus (`i2c-0`, the first DesignWare adapter), which Linux can reach:

| Address / page | Rail | Mode as found |
| --- | --- | --- |
| 0x60 / 0 | Vcore (VccIA) | SVID: the CPU sets it |
| 0x60 / 1 | VccGT | off |
| 0x62 / 1 | VCCSA: memory controller and NGU | fixed by the board at 1.320 V |
| 0x62 / 2 | VNNAON: always-on SoC, D2D | fixed by the board at 0.850 V |

A rail the board holds fixed follows VOUT_COMMAND, the register the BIOS set it through, so **VCCSA** and **VNNAON** rows (Board rails) change it there and check the regulator's own output reading. With +10 mV each, the output moved 1.307 → 1.316 V and 0.849 → 0.858 V. The helper allows only page selects and targets of 0.5-1.5 V on these two chips, never the mode register, so a rail the CPU drives stays the CPU's: Vcore is shown, read-only, with who controls it.

Because the board holds VCCSA fixed, the CPU's own SA request (mailbox domain 4) changes nothing on this board, so the mailbox SA rows are replaced by the regulator row here.

VDD2 is not on a bus Linux can see: neither PCH I2C nor SMBus has a third regulator, so it is probably behind the ITE chip's own SMBus master. DLVR is inside the CPU and follows the core domain, which undervolt protection locks.

### D2D and NGU

Arrow Lake has two more fabric clocks: D2D, the die-to-die link between the compute and SoC tiles (30x on 200S Plus, 21x on the original 200S), and NGU, the SoC tile's network on chip (26-28x), on VnnAON and VccSA respectively. Neither is offered here:

- D2D "can only be set at boot and cannot be changed in the operating system" (SkatterBencher), and HWiNFO's author reports that the interface for reading it is locked.
- No documented register sets or reports NGU at runtime.
- Mailbox domain 7 reads ratio x21 on this CPU and nothing else. That is the original Arrow Lake D2D default, not this CPU's 30x, so it is not assumed to be D2D. Changing D2D or NGU in the BIOS and comparing `--mailbox-scan` before and after would show whether it follows either.

Writes on Arrow Lake go through the same mailbox and MSR paths as on Raptor Lake, with the same read-back and rollback, and were checked on hardware as above. Read-back and a short load test do not prove a setting is stable: change one at a time.
