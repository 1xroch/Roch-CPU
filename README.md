<img src="assets/mark.png" alt="Roch" width="96">

# Roch CPU

CPU and DDR5 memory tuning for Intel LGA1700 (12th–14th Gen) and AMD Ryzen on Windows: ratios, voltages, power limits and Curve Optimizer in one compact light/dark window. Available controls depend on your hardware and BIOS.

## Install

1. Download the zip from [Releases](https://github.com/1xroch/Roch-CPU/releases).
2. Extract the whole folder; keep the driver and `pawnio` folder beside `Roch CPU.exe`.
3. Run `Roch CPU.exe` as administrator. Windows 10/11 x64; no runtime needed.

If Windows blocks the driver (Memory Integrity or the vulnerable-driver blocklist), tuning controls are unavailable.

## What's new in 1.0.7

- **Rust rewrite:** a single native executable, no .NET runtime.
- **Per-core ratios** like MSI Dragon Power: each P-core and each E-core cluster gets its own ratio. CPU and E-core ratio changes now reach every core.
- **DDR5 VDDQ above 1.435 V** on PMIC 8A12 kits; DIMM VDD/VDDQ limits match Dragon Power (up to 2.500 V).
- **Fixed:** a voltage offset could switch an override rail (SA, Core) to Auto and freeze the system; a VDD write could raise VDDQ on 8A12 kits.
- **Cleaner window:** red header values; no live line, Ring Voltage, CPU AUX or (on F CPUs) GT rows; no voltage confirmation prompts.

## Features

- **Intel:** P-core, E-core and ring ratios, per-core and per-E-core-cluster ratios, Core/E-core L2/SA/GT voltage and offset, PL1/PL2.
- **MSI boards:** CPU Core override through the board regulator, base clock and CPU VDD2, each measured after every step.
- **AMD Ryzen:** PPT, TDC, EDC, thermal limit, PBO scalar, FMax and Curve Optimizer (all-core and per-core) through the SMU. Optional [PawnIO](https://pawnio.eu) reads back the limits in force.
- **DDR5:** per-DIMM or synced VDD, VDDQ and VPP, calibrated against each PMIC's own ADC and verified after every write.
- **Safety:** every write is read back, failed writes roll back, and **0** restores the value found at start-up. The Log explains anything rejected or unavailable.

A setting the BIOS locks or caps (for example the P-core ratio) cannot be raised from Windows.

## Screenshots

<p align="center">
  <img src="screenshot.png" alt="Roch CPU on Intel" width="360">
  <img src="screenshot-amd.png" alt="Roch CPU on AMD Ryzen" width="360">
</p>

Left: 1.0.7 on an i5-14600KF / Z790MPOWER. Right: 1.0.5 on a Ryzen 7 9850X3D / X870 AORUS TACHYON ICE. Values are examples, not tuning advice.

## Build from source

Install the stable Rust toolchain for `x86_64-pc-windows-msvc` ([rustup](https://rustup.rs) with the Visual Studio C++ build tools), then:

```bat
build.cmd
cargo test --release
```

`build.cmd` writes `dist\Roch CPU.exe` with its driver files; the tests use simulated hardware only. `Roch CPU.exe --help` lists the diagnostic switches, `--probe` writes a read-only hardware report, and `--ui-preview` opens the window with sample data.

> Overclocking can cause crashes, data loss or hardware damage. Change one setting at a time and test stability; read-back does not prove a setting is stable.

## More

[Hardware reference and verification notes](docs/reference.md) · [ASUS validation](docs/asus-z790-validation.md) · [BCLK investigation](docs/b850mpower-bclk-investigation.md) · [Third-party notices](THIRD-PARTY-NOTICES.md) · [License](LICENSE)

## Credits

Ivan Rusanov's [ZenStates-Core](https://github.com/irusanov/ZenStates-Core) (AMD SMU reference), [OpenLibSys](THIRD-PARTY-NOTICES.md#winring0-driverswinring0x64sys) (WinRing0 driver), [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor) (Super I/O reference), namazso's [PawnIO](https://pawnio.eu) and [PawnIO.Modules](https://github.com/namazso/PawnIO.Modules), and [ryzen_smu](https://gitlab.com/leogx9r/ryzen_smu).

Created by **Roch Studio / [@1xroch](https://x.com/1xroch)** · [YouTube](https://www.youtube.com/@MateoPcTech) | [X](https://x.com/1xroch) | [Discord](https://discord.gg/KfzExpKQHB)
