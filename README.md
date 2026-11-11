<img src="assets/mark.png" alt="Roch" width="96">

# Roch CPU

A Windows CPU and DDR5 memory tuning tool for supported Intel LGA1700 and AMD Ryzen systems. Adjust clocks, power limits, voltages and Curve Optimizer in a compact light/dark interface. Available controls depend on your hardware and BIOS.

## Install

1. Download the package from [Releases](https://github.com/1xroch/Roch-CPU/releases).
2. Extract the entire folder—keep the bundled driver and support files beside `Roch CPU.exe`.
3. Run `Roch CPU.exe` as administrator. Builds from the current (Rust) source need no runtime. Older .NET releases need the .NET 10 (or, before that, .NET 8) Desktop Runtime unless they were self-contained.

Windows 10/11 x64 is required. If Windows blocks a driver, some controls will be unavailable.

## What's new in 1.0.7

- **Per-core ratios, the MSI Dragon Power way:** the Per-Core Ratio window now sets each physical P-core's own ratio, and each E-core cluster's (E-cores share one clock per cluster of four). A core runs no faster than its value. The old "by active cores" turbo-table editor is gone; the all-core limits stay on the CPU Ratio and E-Core Ratio rows.
- **Ratio changes reach every core:** CPU and E-core ratios now also set the CPU's per-core ratio ceiling (OC mailbox `0x1C`/`0x1D`) on every core and cluster, as Dragon Power does. Before, only the first core's ceiling changed, so raised ratios could be capped and the second E-core cluster kept its old ratio. The E-core ratio works with E-cores enabled or disabled.
- **DDR5 VDDQ above 1.435 V:** on PMIC ID `8A12` kits, VDDQ uses the same register extension Dragon Power uses, so it goes past the 5 mV setpoint's 1.435 V top. DIMM VDD and VDDQ limits now match Dragon Power (0.800-2.500 V; the PMIC register's own maximum still applies).
- **Fixed: a voltage offset could switch an override to Auto.** On a BIOS that fixes SA (or Core) in override mode, entering an offset silently switched that rail to adaptive voltage, which hard-froze the test system with overclocked DDR5. Offsets are now refused while a rail is in override mode, and restoring a voltage puts back the BIOS's own override instead of Auto.
- **Fixed: a VDD write could raise VDDQ.** VDD was copied into PMIC register `0x23` whenever it was non-zero; on `8A12` kits that register is the VDDQ extension. It is now only mirrored when it really is a second VDD phase.
- **Safer hardware timing:** embedded-controller and SMBus polling waits at least 8 ms per poll again, as the .NET build did (the Rust port had been polling about seven times faster). Failed ratio changes now roll back the table, the core ratio and every per-core limit.
- **Interface:** header values in red; the live readout line, the Ring Voltage rows (the ring runs on the Vcore rail, so Dragon Power has none) and the read-only CPU AUX row are gone; no confirmation prompts on voltages; the window fits the monitor's work area; Apply in the per-core window runs in the background.
- **Diagnostics:** `--apply` takes several `row value` pairs in one run; `--probe` lists every core's ratio limit and the E-core clusters; `--vr-page1` reads the MSI core regulator's second output.

## Features

- **Intel tuning:** P-core, E-core and ring ratios, per-core and per-E-core-cluster ratios, voltage offsets/overrides, and PL1/PL2 power limits. BIOS locks and CPU/chipset restrictions still apply; a ratio the BIOS caps (for example x54 on the tested Z790MPOWER) cannot be raised from Windows.
- **AMD Ryzen tuning:** PPT, TDC, EDC, thermal limit, PBO scalar and supported FMax controls through the SMU. With PawnIO installed, each limit row also shows the live draw (watts, amps, temperature); without it the app says so and links to pawnio.eu.
- **Per-core Curve Optimizer:** edit individual AMD core offsets, refresh hardware readings and apply changes from a dedicated window.
- **DDR5 memory voltages:** per-DIMM VDD, VDDQ and VPP controls where the PMIC permits writes, with calibration and read-back checks against the PMIC's own ADC. Sync DIMMs sets matching rails together. VDD/VDDQ 0.800-2.500 V, VPP 1.500-2.135 V, within what each PMIC register can hold.
- **Board-specific controls:** CPU Core override, base clock and CPU VDD2 on supported MSI Intel 600/700-series boards. Core override updates both the Renesas regulator and Intel OC mailbox, with read-back and rail verification. AMD BCLK writing is not enabled on the tested B850MPOWER.
- **Light and dark modes:** light by default, switched with the sun/moon button beside minimize. Grouped clock, power, boost and memory sections in a compact window, allowed ranges in each row's tooltip, and a log explaining rejected or unavailable settings.
- **Social links:** YouTube | X | Discord in the bottom-left footer, matching Roch Viewer.
- **Hardware-read header:** CPU, core/thread topology, microcode, motherboard, AGESA (AMD) and BIOS values, with the values in red. On AMD the Base Clock row shows the measured BCLK (read-only).
- **Intel voltage read-back:** programmed Core, E-core L2, SA and GT targets are shown when firmware supplies them. On supported MSI boards, Core displays the regulator target rather than a potentially stale mailbox-only value. `Auto` means the CPU holds no explicit target (MSI Dragon Power shows a built-in 1.050 V for E-core L2 in that case; it is not a reading).
- **Apply and Reset:** Apply writes your edits; Reset restores the tool's startup baseline where supported.

Controls vary by processor, motherboard, BIOS and driver access. Optional [PawnIO](https://pawnio.eu) enables AMD SMU power-table read-back for PPT/TDC/EDC; Roch CPU does not install it automatically. See the [hardware reference](docs/reference.md) for platform details.

## Screenshots

<p align="center">
  <img src="screenshot.png" alt="Roch CPU Intel tuning controls" width="360">
  <img src="screenshot-amd.png" alt="Roch CPU AMD Ryzen tuning controls" width="360">
</p>

Left: Roch CPU 1.0.7 on an Intel Core i5-14600KF (6P + 8E) / Z790MPOWER, with the board-backed Core override, CPU VDD2 and synced DDR5 targets. Right: Roch CPU 1.0.5 on an AMD Ryzen 7 9850X3D / X870 AORUS TACHYON ICE in the default light theme, with the AGESA version, measured base clock and live PPT/TDC/EDC/temperature readings (PawnIO installed). Available rows follow the detected hardware. Displayed settings are examples, not tuning recommendations.

## Build the latest source

Roch CPU is written in Rust. Install the stable **Rust toolchain** for `x86_64-pc-windows-msvc`
(via [rustup](https://rustup.rs), with the Visual Studio C++ build tools), then run:

```bat
build.cmd
```

Open `dist\Roch CPU.exe`. The current version is **1.0.7**. The executable has no runtime
dependency; keep the driver and `pawnio` folder that `build.cmd` copies beside it.

### Intel write verification

Voltage requests are checked against the OC mailbox response. Rejected or ignored targets
produce an error and an attempt to restore the previous mailbox value. Ratio and power
register writes are also read back; the UI says **read back**, which verifies the stored
request, not sustained clocks or physical voltage. Live VID is shown separately from the
editable voltage target; VID is a CPU request, not a measured Vcore rail.
Supported Super I/O monitors provide a separate live Vcore reading. ASUS NCT6798D
detection uses chip ID `0xD42B` and verifies the monitor vendor ID before reading it.
Noisy rail samples report an unverified response without asserting a firmware cause.

On the validated ASUS Z790-A GAMING WIFI D4 configuration, CPU Core uses ASUS's
Global Core SVID Manual mode through the installed ASUS control service. Complete
mode/target/offset readback and rollback protect against a partially applied request.
The live Vcore reading comes from ASUS hardware monitoring. A 1.300 V target produced
about 1.341 V measured in testing; requested voltage is not a promise of exact rail voltage.
The ASUS voltage row labels the editable value as a target and shows measured Vcore
and its difference from the applied target immediately below. Unsaved edits are not
used as the comparison target, and no automatic compensation is applied.
The ASUS CPU VRM offset control has been removed. SA and CPU E-core L2 targets now
use the dedicated ASUS board controls, with rail-specific metadata validation,
complete state readback and rollback. Both were tested at 1.100 V and restored to
Auto: measured SA was about 1.089 V and L2 about 1.104 V. Their own sensor readings
are logged after applying; these are separate from Vcore.
Ring voltage uses ASUS Cache SVID with complete state readback and rollback. A
temporary 1.300 V request was verified in both ASUS state and the CPU cache-domain
register, then restored to Auto; ring ratio remained 50x. Ring and cores share
Vcore, so this is a requested target, not a separate measured ring supply.
Board rail rows require a validated sensor mapping. MSI VDD2/AUX channel labels
are no longer incorrectly displayed on the ASUS DDR4 board.
The original MSI regulator path is preserved. ASUS BCLK remains read-only: the vendor
target accepted changes in testing while the physical clock stayed unchanged.
See [ASUS validation notes](docs/asus-z790-validation.md) for results and dependencies.

When a hypervisor is detected, the app warns that register values may be virtualized.
Windows VBS/Memory Integrity and BIOS policy can restrict tuning. The app does not disable
security features or bypass BIOS locks. See [Intel's VBS compatibility guidance](https://www.intel.com/content/www/us/en/support/articles/000093813/processors/processor-utilities-and-programs.html).

`--probe` collects readings without tuning writes. `--intel-audit` collects per-thread
register data. `--intel-validate 1.3` is an explicit hardware test: it briefly lowers the
P-core ceiling under a single-thread load, tests the current E-core table and a 1.300 V
mailbox target, and restores the captured P-core and voltage settings. It refuses to run
with another Roch CPU instance open. When a Vcore monitor is available, it also records
settled measurements before the override, with it applied, and after restoration.
Voltage readback alone does not establish physical Vcore.

Ring control uses MSR `0x620` and additionally updates a populated ring mailbox domain.
A completely empty domain no longer causes a verified ring-register write to be rolled
back. This preserves the mailbox path used on MSI boards. `--ring-validate` briefly lowers
the ring ceiling, checks the running ring ratio, and restores the original register. It
requires one Roch CPU instance and writes `ring-validation.txt` beside the executable.
The running P-core, E-core and ring ratios are in HWiNFO or `--probe`; the window shows the limits.

E-core control preserves unused zero entries in the turbo table. Boards exposing a
single active entry update that entry; fully populated tables continue to update all
groups. MSR `0x650` and the ratio ceiling are written on every E-core cluster.

The Per-Core Ratio window sets each P-core's and each E-core cluster's own ratio ceiling
(OC mailbox `0x1D`, run on that core). Clusters are found from the CPU's L2 cache
topology, not assumed. A failed write puts every changed limit back.

When Hyper-V is active, the app checks Windows event 12550 from the current boot.
If Windows explicitly reports restricted writes to the voltage mailbox, voltage rows
are disabled with the recorded cause. This leaves P-core and ring control available.
Old events from previous boots do not disable controls. `--windows-audit` exports this
evidence without opening the kernel driver. `--ecore-validate 44` briefly tests the E-core
target and restores its table; it requires one instance and permits at most a one-step
increase above the current ceiling, with an absolute test maximum of 44.

Regression checks use a simulated driver and never open the hardware driver:

```bat
cargo test --release
```

`Roch CPU.exe --ui-preview [--dark]` opens the window with sample rows and no hardware access.

> Overclocking can cause crashes, data loss or hardware damage. Change one setting at a time and test stability. Read-back checks do not prove a setting is stable.

## Credits

- **Ivan Rusanov — [ZenStates-Core](https://github.com/irusanov/ZenStates-Core), ZenStates and SMUDebugTool:** references for AMD SMU messages, mailbox addresses and Curve Optimizer core mapping.
- **[OpenLibSys / Noriyuki Miyazaki](THIRD-PARTY-NOTICES.md#winring0-driverswinring0x64sys):** the bundled WinRing0 low-level access driver.
- **[LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor):** Super I/O register/access references and the source package for the bundled WinRing0 binary.
- **namazso — [PawnIO](https://pawnio.eu) and [PawnIO.Modules](https://github.com/namazso/PawnIO.Modules):** optional driver access and the bundled signed RyzenSMU module for AMD power-table reads.
- **[ryzen_smu](https://gitlab.com/leogx9r/ryzen_smu):** AMD SMN access reference.

Created by **Roch Studio / [@1xroch](https://x.com/1xroch)**. See [third-party notices](THIRD-PARTY-NOTICES.md) for attribution, component licenses and implementation details.

[YouTube](https://www.youtube.com/@MateoPcTech) | [X](https://x.com/1xroch) | [Discord](https://discord.gg/KfzExpKQHB)

[Hardware details](docs/reference.md) · [BCLK investigation](docs/b850mpower-bclk-investigation.md) · [License](LICENSE)
