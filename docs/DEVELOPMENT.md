# Development

pbsgui is a Cargo workspace plus a static front end. The cross-platform crates
build and test on Linux or Windows; the full desktop app, the installer, and the
Windows-only features (the service, the tray, credential storage, and SQL Server
integration) are built on Windows.

## Where to develop

- **Linux** is convenient for the cross-platform core (`pbs-client`,
  `pbsgui-ipc`, `pbsgui-engine`) and for linting and testing the whole workspace,
  including the Tauri app. Windows-only modules are compiled out, so the engine's
  service, tray, credential storage, and SQL Server code are not built there.
- **Windows** is required to build the desktop app and the NSIS installer, and to
  build, run, and test the Windows-only features end to end (SQL Server backup
  over VDI, the service, the tray, the credential store).

For work that touches the Windows-only modules, develop on Windows so you can
compile and run them directly; otherwise the only validation is the CI Windows
build plus manual testing.

## Prerequisites

### Common

- A recent stable Rust toolchain (via [rustup](https://rustup.rs)).
- A C toolchain (the TLS stack compiles native code).

### Linux (cross-platform crates and the Tauri GUI)

Install the WebKitGTK and SVG development packages the Tauri app links against,
for example on Debian/Ubuntu:

```
sudo apt-get install libwebkit2gtk-4.1-dev librsvg2-dev
```

The GUI compiles and lints on Linux, but the window cannot be shown without a
display; use Windows (or CI artifacts) to exercise the actual app.

### Windows (full app, installer, and Windows-only features)

- The Rust MSVC toolchain (`x86_64-pc-windows-msvc`).
- Visual Studio Build Tools with the "Desktop development with C++" workload,
  which provides the linker and C compiler used by the native dependencies.
- NASM, required to build the assembly in the TLS stack.
- The Tauri CLI (`cargo install tauri-cli`) for building the app and installer.
- The WebView2 runtime (present on current Windows; the installer can also bundle
  it).
- SQLVDI.dll is installed with SQL Server, so VDI is available wherever SQL Server
  is.

## Building and running

Build the engine and run it in the foreground for development:

```
cargo build -p pbsgui-engine
cargo run -p pbsgui-engine -- serve
```

Run the desktop app (it connects to the engine over the local socket; it does not
start the engine itself):

```
cargo tauri dev
```

Note that `cargo tauri dev` rebuilds the GUI but not the engine. After pulling
changes, rebuild the engine so the GUI does not connect to a stale one; the
socket name is versioned to prevent a new GUI from talking to an incompatible
engine.

## Verification

The same checks CI runs:

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Cross-checking the Windows-only code from Linux

Clippy on Linux compiles out the `cfg(windows)` modules, so it cannot catch
errors in the service, tray, credential store, or SQL Server code. To compile
those without a Windows machine, add the GNU Windows target once
(`rustup target add x86_64-pc-windows-gnu` plus the `gcc-mingw-w64-x86-64`
package) and run, before pushing changes that touch them:

```
cargo clippy -p pbsgui-engine --target x86_64-pc-windows-gnu --all-targets -- -D warnings
cargo check  -p pbsgui        --target x86_64-pc-windows-gnu
```

## Building the Windows installer

CI builds the engine, stages it as a Tauri sidecar, and produces an NSIS
installer using the sidecar build configuration. Locally on Windows, the helper
script does the same and stamps a local version:

```
scripts\build-windows-installer.bat
```

The installer is written under `target\release\bundle\nsis\`. It installs and
starts the engine service via NSIS install hooks, and removes it on uninstall.

## Running the engine as a service

The engine manages its own Windows service (run elevated):

```
pbsgui-engine service install     # register and start
pbsgui-engine service uninstall   # stop and remove
```

The installer performs the install/uninstall automatically. The service runs as
LocalSystem so scheduled backups run whether or not the GUI is open.

## Testing SQL Server backup

Two one-time bits of SQL Server setup are needed on the instance under test:

1. **Enable TCP/IP.** The TDS driver connects over TCP, and fresh installs often
   ship with TCP/IP disabled. In SQL Server Configuration Manager, enable TCP/IP
   under the instance's protocols, set the IPAll TCP Port (for example 1433),
   clear any dynamic port, and restart the SQL Server service. The SQL Servers
   tab flags instances with TCP/IP disabled, and the per-instance Check reports
   it with the fix.

2. **Grant the service identity access.** The engine connects as its service
   identity (LocalSystem, `NT AUTHORITY\SYSTEM`), and VDI requires the connecting
   login to be in the `sysadmin` server role. On many default installs
   `NT AUTHORITY\SYSTEM` is already a sysadmin; if not, grant it:

   ```sql
   CREATE LOGIN [NT AUTHORITY\SYSTEM] FROM WINDOWS;
   ALTER SERVER ROLE sysadmin ADD MEMBER [NT AUTHORITY\SYSTEM];
   ```

Then, in the GUI's SQL Servers tab, click Discover, Probe an instance to list its
databases, and back one up.

### Transaction-log backups

To keep a FULL or BULK_LOGGED database's transaction log from growing without
bound, schedule log backups:

1. Run a full backup with copy-only turned off once, so pbsgui owns the backup
   chain. (A copy-only full does not start a log chain, so `BACKUP LOG` would
   fail until a regular full exists.)
2. Create a second job over the same databases with the backup type set to
   Transaction log and a frequent schedule. Each run takes `BACKUP LOG` (never
   copy-only), which truncates the inactive log. Log snapshots are stored in a
   separate `-log` snapshot group.

If you do not need point-in-time recovery, setting the database to the SIMPLE
recovery model is the alternative: the log truncates automatically and log
backups are neither needed nor allowed.

## Continuous integration

CI runs two jobs: a Linux job that installs the WebKitGTK packages and runs
fmt, clippy, and tests across the workspace, and a Windows job that builds the
engine and the desktop app and produces the NSIS installer as an artifact. The
Windows job is the first compile of the Windows-only modules, so push changes
that touch them with that in mind.
