# C# → Rust porting conventions (Roch CPU)

The original .NET sources live in `src/RochPower/**` and `tests/**` (they are removed once the
port is complete). The Rust crate is at the repo root: `Cargo.toml`, `src/main.rs`,
`src/hw/*.rs` (hardware layer, one file per C# file, snake_case names), `src/model/*.rs`
(the C# `Core` namespace), `src/ui/*.rs`.

This is hardware-tuning code (MSRs, SMU mailboxes, voltage regulators). **Translate faithfully.**
Keep every constant, register address, bit mask, ordering of hardware accesses, sleep/delay,
retry count, bounds check, rollback path, and every user-visible message string (log lines,
error messages, notes) exactly as in C#. Do not "improve" or simplify hardware logic. Keep the
doc comments (convert `/// <summary>` to `///`), and keep the explanatory inline comments.

## Already written (do not change without reason)

- `src/hw/mod.rs` — `trait KernelDriver` and `trait Smbus` (see the file), plus `pub mod` lines
  for every module. Do not edit `mod.rs` except to add a `#[cfg(test)]` helper module if needed.
- `src/hw/native.rs` — `lock(&Mutex)`, `wide`, `win32_message`, `last_error_message`,
  `cpuid(leaf, sub) -> (eax, ebx, ecx, edx)`, `run_on_cpu(cpu, f) -> Result<T>`, `app_dir()`,
  `sleep_ms(ms)`, `is_only_instance()`, `get_system_firmware_table`, `read_hklm_string`,
  `open_device`, `device_io_control`, `OwnedHandle`.
- `src/hw/winring0.rs` (`WinRing0Driver`), `src/hw/pawnio.rs` (`PawnIo`),
  `src/hw/startup_io_diagnostics.rs` (functions `begin() -> Option<Scope>`, `observe_ec_read`,
  `observe_ec_write`, `observe_ec_failure(addr, write, &anyhow::Error)`, `observe_register_read`,
  `observe_probe_result(&CorePageVerificationResult)`; `Scope::snapshot() -> Vec<String>`; the
  scope ends when dropped).
- `src/model/setting.rs` — `Setting`, `SettingGroup`.

## Type and API mapping rules

- Names: C# `PascalCase` methods/properties → `snake_case` methods; C# types keep their names
  (`IntelCpu`, `OcMailbox`, `VfDomainSettings`, `SmuStatus`). Constants → `SCREAMING_SNAKE`
  associated consts (`OcMailbox::DOMAIN_CORE`, `SuperIo::RAIL_VDD2`, `Ddr5Dimm::VDD_MIN_V`).
  Properties become getter methods (`cpu.brand_string()`, `cpu.p_core_count()`) unless they are
  plain immutable data, in which case a `pub` field is fine — prefer getter methods for anything
  computed or mutable. Records → `#[derive(Clone, Debug, PartialEq)] pub struct` with `pub` fields.
- Integers: keep the C# widths (`uint`→`u32`, `ushort`→`u16`, `byte`→`u8`, `int`→`i32`,
  `ulong`→`u64`, `long`→`i64`). `double`→`f64`, `float`→`f32`.
- Exceptions → `anyhow::Result<T>`; `throw new IOException("msg")` → `bail!("msg")` (same text).
  `try { … } catch { }` → ignore the `Err`. Code that throws only on programmer error
  (`ArgumentException`, `ArgumentOutOfRangeException`) also becomes `bail!` with the same text.
- `bool TryX(…, out T v)` → `fn try_x(…) -> Option<T>`; `T? X()` (nullable return) → `Option<T>`.
  A method that both can throw and returns a nullable → `Result<Option<T>>`.
- `IKernelDriver` → `Arc<dyn KernelDriver>`. `ISmbus` → `Arc<dyn Smbus>` (or `&dyn Smbus`).
- **Sharing / mutability:** every hardware object is shared by reference in C# (several closures
  and objects hold the same `SuperIo`, `IntelCpu`, `BclkMeter`, `EcMailbox`, …) and the UI runs a
  background thread. So **every public hardware type uses `&self` methods and is `Send + Sync`**;
  put mutable fields in `Mutex<…>` (use `native::lock`) or atomics. A type that C# shares between
  owners is held as `Arc<T>`. Objects owned by exactly one other object can be held by value.
  `lock (x) { … }` → a `Mutex<()>` or the data mutex.
- Delegates: `Func<double?>` → `Box<dyn Fn() -> Option<f64> + Send + Sync>` (or `Arc<…>`);
  `Action<string>? log` parameters → `Option<&dyn Fn(&str)>`.
- `IDisposable.Dispose` → `impl Drop` (only if it releases something).
- Formatting: invariant culture. `{x:X2}` → `{x:02X}`; `{v:0.000}` → `{v:.3}`; `{v:0.0}` →
  `{v:.1}`; `{v:+0;-0}` → `{v:+.0}`; `{v:F1}` → `{v:.1}`. `{nullable}` of null prints empty.
  C# `bool` prints `True`/`False`.
- `Thread.Sleep(n)` → `native::sleep_ms(n)`. `Stopwatch` → `std::time::Instant`.
  `DateTime.UtcNow`/`DateTimeOffset` → `chrono::DateTime<chrono::Utc>`.
- CPUID (`X86Base.CpuId`) → `native::cpuid`. `Interlocked`/`volatile` → atomics.
- Tests: port the relevant C# tests into `#[cfg(test)] mod tests` at the bottom of the module
  they exercise (or a `src/hw/test_support.rs`-style fake if several modules share one — keep it
  `#[cfg(test)]`). Keep the test names (as snake_case fn names) and assertions. Fakes implement
  `KernelDriver` / `Smbus` with `Mutex` state.
- No `unsafe` outside FFI. No new crates without a strong reason (available: `anyhow`, `chrono`,
  `windows-sys` (features in Cargo.toml), `windows` (COM/OLE/Variant/EventLog), `eframe`).
  If you need another `windows-sys` feature, add it to `Cargo.toml`'s feature list.

## Building

`cargo build` / `cargo test` from the repo root (Windows, MSVC toolchain). Other agents are porting
other modules at the same time: errors in files you do not own are not yours to fix — but your own
files must compile and your tests must pass. Do not edit files owned by others; if you need
something from a module that is still a stub, note it in your report and code against the API the
conventions above imply (snake_case of the C# member).
