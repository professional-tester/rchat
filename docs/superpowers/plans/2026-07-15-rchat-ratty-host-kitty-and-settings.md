# RChat Ratty Host, Kitty Compatibility, and Settings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `rchat-tui` prefer and automatically launch inside Ratty, restore Kitty-based inline previews inside Ratty, and add opt-in Ghostty appearance import plus reset and Ratty-path controls to TUI Settings.

**Architecture:** Ratty remains a separate GPU terminal process; it is not linked into `rchat-tui`. Ratty will advertise its session and fully answer the Kitty graphics and cell-size capability queries that `ratatui-image` already sends. RChat will discover and launch a configured, sibling, or `PATH` Ratty executable, retain Kitty as the fallback, and store launcher preferences plus an optional generated Ratty config under RChat's application-data directory.

**Tech Stack:** Rust 2021 (`rchat-tui`), Rust 2024 (Ratty), Clap, serde/serde_json, TOML serialization, `ratatui-image`, Crossterm, Bevy/wgpu, `portable-pty`, Cargo nextest.

## Global Constraints

- Work in the existing `/Users/atasesli/Desktop/VsCode/rchat` and `/Users/atasesli/Desktop/VsCode/ratty` checkouts; do not create a replacement checkout or reset either dirty worktree.
- Preserve all unrelated staged, unstaged, and untracked work. Stage only the exact paths listed by each task.
- Keep Ratty as a separate executable and PTY host. Do not add Ratty as an RChat Cargo dependency.
- Ratty is the preferred runtime. Kitty in the current terminal is the failsafe when Ratty cannot be resolved or started.
- Do not set `TERM=xterm-kitty`; Ratty must advertise only the capabilities it actually implements through protocol replies and `TERM_PROGRAM` metadata.
- Ghostty import is user-triggered from Settings. It must never modify Ghostty's files or the user's standalone Ratty configuration.
- Reset removes only RChat's managed Ratty import and returns to Ratty's normal default/config discovery behavior.
- Normal development and launch commands must not require `DYLD_LIBRARY_PATH`.
- Do not change GUI behavior, Tauri APIs, RChat network behavior, or media codec ownership.
- Use `cargo nextest run`, not `cargo test`, for ordinary Rust test execution.

---

## File Structure

### Ratty checkout

- Modify `../ratty/src/kitty.rs`: parse Kitty `a=q` capability requests and model their replies without storing the probe image.
- Modify `../ratty/src/inline.rs`: return Kitty query replies to the PTY child and cover the exact `ratatui-image` probe bytes.
- Modify `../ratty/src/runtime.rs`: advertise `RATTY_SESSION`, `TERM_PROGRAM`, and `TERM_PROGRAM_VERSION`; answer `CSI 16t` using a supplied cell-pixel size.
- Modify `../ratty/src/terminal.rs`: expose the current physical pixel dimensions of one terminal cell.
- Modify `../ratty/src/systems.rs`: update parser callbacks with current cell dimensions before processing PTY output.

### RChat checkout

- Create `src-tauri/crates/rchat-tui/src/ratty_host.rs`: launcher preferences, executable discovery, automatic relaunch, managed-config paths, status reporting, and reset.
- Create `src-tauri/crates/rchat-tui/src/ghostty_import.rs`: Ghostty config/theme discovery, compatible-value translation, safe managed Ratty TOML generation, and import metadata.
- Modify `src-tauri/crates/rchat-tui/src/lib.rs`: export the two focused modules.
- Modify `src-tauri/crates/rchat-tui/src/main.rs`: run the Ratty host bootstrap before entering the TUI.
- Modify `src-tauri/crates/rchat-tui/src/app.rs`: accept the host bypass flag, defensively prefer Kitty when Ratty bitmap v1 is present, expose runtime diagnostics, and wire Settings actions.
- Modify `src-tauri/crates/rchat-tui/src/state.rs`: add Ratty settings fields and focus routing under the existing Media settings section.
- Modify `src-tauri/crates/rchat-tui/Cargo.toml`: add direct `serde` and `toml` dependencies used by launcher preferences and managed config generation.
- Modify `src-tauri/Cargo.lock`: record the manifest change without unrelated dependency updates.

---

### Task 1: Make Ratty identify its child sessions and answer media capability queries

**Files:**
- Modify: `../ratty/src/kitty.rs` (`KittyParserState::consume_sequence`, `KittyOperation`, unit tests)
- Modify: `../ratty/src/inline.rs` (`TerminalInlineObjects::handle_apc_sequence`, unit tests)
- Modify: `../ratty/src/runtime.rs` (`TerminalParserCallbacks`, `TerminalRuntime::spawn`, unit tests)
- Modify: `../ratty/src/terminal.rs` (`TerminalSurface` cell-size helper, unit tests)
- Modify: `../ratty/src/systems.rs` (`pump_pty_output`)

**Interfaces:**
- Produces: `RATTY_SESSION=1`, `TERM_PROGRAM=ratty`, and `TERM_PROGRAM_VERSION=<crate version>` in every Ratty PTY child.
- Produces: Kitty query reply `\x1b_Gi=<id>;OK\x1b\\` for a valid direct-transfer `a=q` probe, without adding an inline object.
- Produces: `TerminalParserCallbacks::set_cell_pixel_size(width: u16, height: u16)` and `CSI 16t` reply `\x1b[6;<height>;<width>t`.
- Produces: `TerminalSurface::cell_pixel_dimensions(&self) -> (u16, u16)`.
- Consumes: the current Ratty terminal texture size, column count, and row count.

- [ ] **Step 1: Add failing Kitty capability-query tests**

Add tests in `../ratty/src/kitty.rs` and `../ratty/src/inline.rs` using the exact query emitted by `ratatui-image`:

```rust
const RATATUI_IMAGE_KITTY_QUERY: &[u8] =
    b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";

#[test]
fn kitty_query_reports_support_without_storing_an_image() {
    let mut objects = TerminalInlineObjects::default();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let replies = objects.consume_pty_output(RATATUI_IMAGE_KITTY_QUERY, &mut parser);

    assert_eq!(replies, [b"\x1b_Gi=31;OK\x1b\\".to_vec()]);
    assert!(objects.objects.is_empty());
    assert!(objects.anchors.is_empty());
}
```

Also add rejection coverage for malformed query data:

```rust
#[test]
fn invalid_kitty_query_reports_error_without_mutating_state() {
    let mut objects = TerminalInlineObjects::default();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let replies = objects.consume_pty_output(
        b"\x1b_Gi=9,s=2,v=2,a=q,t=d,f=24;AAAA\x1b\\",
        &mut parser,
    );

    assert_eq!(replies, [b"\x1b_Gi=9;EINVAL:invalid pixel data\x1b\\".to_vec()]);
    assert!(objects.objects.is_empty());
}
```

- [ ] **Step 2: Run the focused Ratty tests and confirm the missing reply**

Run:

```bash
cd /Users/atasesli/Desktop/VsCode/ratty
cargo nextest run kitty_query
```

Expected: both tests fail because `a=q` is currently converted to `KittyOperation::Ignored` and produces no PTY reply.

- [ ] **Step 3: Implement Kitty query validation and reply generation**

Extend `KittyOperation` in `../ratty/src/kitty.rs`:

```rust
Query {
    image_id: u32,
    result: Result<(), &'static str>,
},
```

In `KittyParserState::consume_sequence`, handle `a=q` before the persistent transfer branches. Parse `i`, `f`, `s`, `v`, `t`, base64-decode the payload, and validate these supported combinations:

```rust
fn validate_direct_query(
    format: u32,
    width: u32,
    height: u32,
    medium: &str,
    payload: &[u8],
) -> Result<(), &'static str> {
    if medium != "d" {
        return Err("unsupported transmission medium");
    }
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    let expected = match format {
        24 => pixels.saturating_mul(3),
        32 => pixels.saturating_mul(4),
        100 => return image::load_from_memory_with_format(payload, image::ImageFormat::Png)
            .map(|_| ())
            .map_err(|_| "invalid PNG data"),
        _ => return Err("unsupported pixel format"),
    };
    if payload.len() as u64 != expected {
        return Err("invalid pixel data");
    }
    Ok(())
}
```

Return `KittyOperation::Query` without touching `self.transfer`, `self.next_object_id`, or stored objects. In `TerminalInlineObjects::handle_apc_sequence`, convert the query result to a printable protocol reply:

```rust
KittyOperation::Query { image_id, result } => {
    let message = match result {
        Ok(()) => "OK".to_string(),
        Err(error) => format!("EINVAL:{error}"),
    };
    (
        true,
        Some(format!("\x1b_Gi={image_id};{message}\x1b\\").into_bytes()),
    )
}
```

Honor Kitty's `q=1`/`q=2` suppression rules if the query includes them: `q=1` suppresses `OK`, and `q=2` suppresses errors.

- [ ] **Step 4: Run the Kitty query tests and confirm they pass**

Run:

```bash
cd /Users/atasesli/Desktop/VsCode/ratty
cargo nextest run kitty_query
```

Expected: both tests pass and no query creates a stored image or placement.

- [ ] **Step 5: Add failing `CSI 16t` and Ratty-session environment tests**

Add callback coverage in `../ratty/src/runtime.rs`:

```rust
#[test]
fn cell_size_query_reports_current_pixel_dimensions() {
    let mut parser = Parser::new_with_callbacks(
        24,
        80,
        0,
        TerminalParserCallbacks::default(),
    );
    parser.callbacks_mut().set_cell_pixel_size(10, 20);

    parser.process(b"\x1b[16t");

    assert_eq!(
        parser.callbacks_mut().take_replies(),
        [b"\x1b[6;20;10t".to_vec()]
    );
}
```

Factor child environment application into a testable helper and add:

```rust
#[test]
fn ratty_child_environment_identifies_the_terminal() {
    let mut command = CommandBuilder::new("rchat-tui");

    apply_terminal_identity(&mut command);

    assert_eq!(command.get_env("RATTY_SESSION"), Some(OsStr::new("1")));
    assert_eq!(command.get_env("TERM_PROGRAM"), Some(OsStr::new("ratty")));
    assert_eq!(
        command.get_env("TERM_PROGRAM_VERSION"),
        Some(OsStr::new(env!("CARGO_PKG_VERSION")))
    );
}
```

- [ ] **Step 6: Run the new runtime tests and confirm they fail**

Run:

```bash
cd /Users/atasesli/Desktop/VsCode/ratty
cargo nextest run cell_size_query_reports_current_pixel_dimensions
cargo nextest run ratty_child_environment_identifies_the_terminal
```

Expected: failure because callbacks have no cell-size state and Ratty does not yet identify the PTY child.

- [ ] **Step 7: Implement cell-size replies and terminal identity**

Add `cell_pixel_size: Option<(u16, u16)>` to `TerminalParserCallbacks`, plus:

```rust
pub fn set_cell_pixel_size(&mut self, width: u16, height: u16) {
    self.cell_pixel_size = Some((width.max(1), height.max(1)));
}
```

Handle `CSI 16t` in `unhandled_csi` before the warning path:

```rust
if i1.is_none()
    && i2.is_none()
    && c == 't'
    && params.len() == 1
    && params[0] == [16]
{
    if let Some((width, height)) = self.cell_pixel_size {
        self.pending_replies
            .push(format!("\x1b[6;{height};{width}t").into_bytes());
    }
    return;
}
```

Expose current physical cell dimensions from `TerminalSurface`:

```rust
pub fn cell_pixel_dimensions(&self) -> (u16, u16) {
    let pixels = self.pixmap_dimensions();
    let width = pixels.x.div_ceil(u32::from(self.cols.max(1)));
    let height = pixels.y.div_ceil(u32::from(self.rows.max(1)));
    (
        width.clamp(1, u32::from(u16::MAX)) as u16,
        height.clamp(1, u32::from(u16::MAX)) as u16,
    )
}
```

Add `terminal: Res<TerminalSurface>` to `pump_pty_output` and call `set_cell_pixel_size` before draining PTY chunks. In `TerminalRuntime::spawn`, apply configured environment first, then force the terminal identity so user config cannot accidentally remove it:

```rust
fn apply_terminal_identity(command: &mut CommandBuilder) {
    command.env("RATTY_SESSION", "1");
    command.env("TERM_PROGRAM", "ratty");
    command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
}
```

- [ ] **Step 8: Run all focused Ratty protocol/runtime tests**

Run:

```bash
cd /Users/atasesli/Desktop/VsCode/ratty
cargo nextest run kitty_query
cargo nextest run cell_size_query
cargo nextest run ratty_child_environment
```

Expected: all matching tests pass; `CSI 16t` no longer reaches the unhandled-warning branch.

- [ ] **Step 9: Commit the Ratty capability work**

```bash
cd /Users/atasesli/Desktop/VsCode/ratty
git add src/kitty.rs src/inline.rs src/runtime.rs src/terminal.rs src/systems.rs
git commit -m "fix: advertise kitty media support to terminal clients"
```

Expected: only these five Ratty paths are staged. Existing unrelated Ratty modifications remain untouched unless they overlap these files, in which case review the staged diff hunk-by-hunk before committing.

---

### Task 2: Add the RChat Ratty host bootstrap and executable discovery

**Files:**
- Create: `src-tauri/crates/rchat-tui/src/ratty_host.rs`
- Modify: `src-tauri/crates/rchat-tui/src/lib.rs`
- Modify: `src-tauri/crates/rchat-tui/src/main.rs`
- Modify: `src-tauri/crates/rchat-tui/src/app.rs` (`Cli` only in this task)
- Modify: `src-tauri/crates/rchat-tui/Cargo.toml`
- Modify: `src-tauri/Cargo.lock`

**Interfaces:**
- Produces: `RattyHostPreferences { executable_path: Option<PathBuf> }` stored at `<app-data>/ratty-host.json`.
- Produces: `resolved_ratty(app_dir: &Path) -> Result<DiscoveryResult>`.
- Produces: `launch_if_needed(args: &[OsString]) -> Result<LaunchOutcome>`.
- Produces: `managed_ratty_config_path(app_dir: &Path) -> PathBuf`, fixed at `<app-data>/ratty/ratty.toml`.
- Produces: hidden `--no-ratty` and environment bypass `RCHAT_NO_RATTY=1`.
- Consumes: `RATTY_SESSION=1` from Task 1 and `rchat_core::runtime::default_app_data_dir()`.

- [ ] **Step 1: Add direct serialization dependencies**

Add to `src-tauri/crates/rchat-tui/Cargo.toml`:

```toml
serde = { version = "1", features = ["derive"] }
toml = "0.8"
```

Run:

```bash
cargo check --manifest-path src-tauri/Cargo.toml -p rchat-tui
```

Expected: Cargo updates only the package manifest/lockfile relationship; no broad dependency upgrade occurs.

- [ ] **Step 2: Write failing discovery, recursion, fallback, and argument-forwarding tests**

Create `src-tauri/crates/rchat-tui/src/ratty_host.rs` with test-only expectations first:

```rust
#[test]
fn discovery_prefers_configured_then_environment_then_sibling_then_path() {
    let temp = tempfile::tempdir().unwrap();
    let configured = touch(temp.path().join("configured-ratty"));
    let env_override = touch(temp.path().join("env-ratty"));
    let sibling = touch(temp.path().join("ratty"));
    let current = temp.path().join("rchat-tui");

    let resolved = resolve_ratty_executable(
        Some(&configured),
        Some(env_override.as_os_str()),
        &current,
        None,
    )
    .unwrap();

    assert_eq!(resolved.path, configured);
    assert_eq!(resolved.source, RattyPathSource::Configured);
    assert!(sibling.is_file());
}

#[test]
fn hosted_or_bypassed_process_runs_in_current_terminal() {
    assert!(should_run_here(Some("1"), false, false));
    assert!(should_run_here(None, true, false));
    assert!(should_run_here(None, false, true));
}

#[test]
fn ratty_command_forwards_title_config_executable_and_arguments() {
    let command = build_ratty_args(
        Path::new("/tmp/rchat-tui"),
        &[OsString::from("media-smoke"), OsString::from("--fps"), OsString::from("10")],
        Some(Path::new("/tmp/ratty.toml")),
    );
    assert_eq!(
        command,
        [
            "--title", "RChat", "--config-file", "/tmp/ratty.toml",
            "--command", "/tmp/rchat-tui", "media-smoke", "--fps", "10"
        ]
        .map(OsString::from)
    );
}
```

Add a fallback test using an injected process runner that returns `io::ErrorKind::NotFound`; assert `LaunchOutcome::RunHere` contains a readable warning rather than aborting RChat.

- [ ] **Step 3: Run the host tests and confirm the module is incomplete**

Run:

```bash
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui ratty_host
```

Expected: compilation/test failure until the interfaces below are implemented.

- [ ] **Step 4: Implement preferences and deterministic executable discovery**

Define these public types:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RattyHostPreferences {
    pub executable_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RattyPathSource {
    Configured,
    Environment,
    Sibling,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRatty {
    pub path: PathBuf,
    pub source: RattyPathSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryResult {
    pub resolved: Option<ResolvedRatty>,
    pub warning: Option<String>,
}

pub enum LaunchOutcome {
    RunHere { warning: Option<String> },
    HostedExited(ExitStatus),
}
```

Implement preference paths and JSON load/save:

```rust
pub fn preferences_path(app_dir: &Path) -> PathBuf {
    app_dir.join("ratty-host.json")
}

pub fn managed_ratty_config_path(app_dir: &Path) -> PathBuf {
    app_dir.join("ratty").join("ratty.toml")
}
```

Discovery order must be exactly:

1. Non-empty saved `executable_path` that points to a file.
2. Non-empty `RCHAT_RATTY_PATH` that points to a file.
3. `ratty`/`ratty.exe` beside `std::env::current_exe()`.
4. The first `ratty`/`ratty.exe` file found by iterating `PATH` with `std::env::split_paths`.

If a configured path exists in preferences but is invalid, continue discovery and preserve that warning in `DiscoveryResult`, whether or not a fallback candidate is found. The launcher prints it and Settings displays it rather than making the application unusable.

- [ ] **Step 5: Implement automatic relaunch and safe fallback**

`launch_if_needed` must:

```rust
pub fn launch_if_needed(args: &[OsString]) -> Result<LaunchOutcome> {
    if std::env::var("RATTY_SESSION").ok().as_deref() == Some("1")
        || std::env::var("RCHAT_NO_RATTY").ok().as_deref() == Some("1")
        || args.iter().any(|arg| arg == OsStr::new("--no-ratty"))
    {
        return Ok(LaunchOutcome::RunHere { warning: None });
    }

    let app_dir = rchat_core::runtime::default_app_data_dir()?;
    let discovery = resolved_ratty(&app_dir)?;
    let Some(resolved) = discovery.resolved else {
        return Ok(LaunchOutcome::RunHere {
            warning: Some(discovery.warning.unwrap_or_else(|| {
                "Ratty was not found; using the current terminal with Kitty fallback".into()
            })),
        });
    };

    let current_exe = std::env::current_exe()?;
    let child_args = args.iter().skip(1).cloned().collect::<Vec<_>>();
    let managed_config = managed_ratty_config_path(&app_dir);
    let config = managed_config.is_file().then_some(managed_config.as_path());
    let status = Command::new(&resolved.path)
        .args(build_ratty_args(&current_exe, &child_args, config))
        .current_dir(std::env::current_dir()?)
        .env("RATTY_SESSION", "1")
        .status();

    match status {
        Ok(status) => Ok(LaunchOutcome::HostedExited(status)),
        Err(error) => Ok(LaunchOutcome::RunHere {
            warning: Some(format!(
                "failed to start Ratty at {}: {error}; using the current terminal with Kitty fallback",
                resolved.path.display()
            )),
        }),
    }
}
```

Setting `RATTY_SESSION=1` on the Ratty process is intentional: it is inherited by the child even when the locally installed Ratty predates Task 1. Task 1 remains necessary so manually running `ratty --command rchat-tui` also avoids recursion.

- [ ] **Step 6: Wire the bootstrap through `main.rs` and accept the bypass flag**

Export `ratty_host` from `lib.rs`. Add a hidden global flag to `Cli` so Clap accepts the argument after the bootstrap decides to stay in the current terminal:

```rust
#[derive(Debug, Parser)]
pub struct Cli {
    #[arg(long, global = true, hide = true)]
    no_ratty: bool,
    #[command(subcommand)]
    command: Option<Command>,
}
```

Update `main.rs`:

```rust
fn main() -> anyhow::Result<()> {
    let args = std::env::args_os().collect::<Vec<_>>();
    match rchat_tui::ratty_host::launch_if_needed(&args)? {
        rchat_tui::ratty_host::LaunchOutcome::RunHere { warning } => {
            if let Some(warning) = warning {
                eprintln!("{warning}");
            }
            rchat_tui::app::run()
        }
        rchat_tui::ratty_host::LaunchOutcome::HostedExited(status) if status.success() => Ok(()),
        rchat_tui::ratty_host::LaunchOutcome::HostedExited(status) => {
            Err(anyhow::anyhow!("Ratty exited with {status}"))
        }
    }
}
```

- [ ] **Step 7: Run focused and package tests**

Run:

```bash
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui ratty_host
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui parses_default_interactive_shell
```

Expected: discovery precedence, recursion prevention, fallback, argument forwarding, and existing CLI parsing all pass.

- [ ] **Step 8: Commit the host bootstrap**

```bash
git add \
  src-tauri/crates/rchat-tui/Cargo.toml \
  src-tauri/Cargo.lock \
  src-tauri/crates/rchat-tui/src/lib.rs \
  src-tauri/crates/rchat-tui/src/main.rs \
  src-tauri/crates/rchat-tui/src/app.rs \
  src-tauri/crates/rchat-tui/src/ratty_host.rs
git commit -m "feat(tui): launch through preferred Ratty host"
```

---

### Task 3: Implement opt-in Ghostty-to-Ratty settings import and reset

**Files:**
- Create: `src-tauri/crates/rchat-tui/src/ghostty_import.rs`
- Modify: `src-tauri/crates/rchat-tui/src/lib.rs`
- Test: inline tests in `ghostty_import.rs`

**Interfaces:**
- Produces: `import_from_ghostty(app_dir: &Path) -> Result<GhosttyImportResult>`.
- Produces: `reset_managed_ratty_config(app_dir: &Path) -> Result<bool>`.
- Produces: `managed_import_status(app_dir: &Path) -> Result<Option<GhosttyImportMetadata>>`.
- Consumes: `ratty_host::managed_ratty_config_path(app_dir)`.
- Writes only: `<app-data>/ratty/ratty.toml` and `<app-data>/ratty/import.json`.

- [ ] **Step 1: Write failing Ghostty discovery, layering, theme, keybinding, and reset tests**

Use temporary directory fixtures rather than the developer's live Ghostty files. Cover:

```rust
#[test]
fn macos_config_overrides_xdg_and_included_files_load_last() {
    // XDG sets font-size=11, macOS sets font-size=13, included file sets font-size=15.
    // The resolved value must be 15, matching Ghostty's documented load order.
}

#[test]
fn named_theme_and_user_overrides_become_ratty_theme_sections() {
    // Fake Adventure theme supplies foreground/background/palette 0..15.
    // User config overrides cursor-color and background-opacity.
    // Generated TOML must deserialize and retain every mapped color.
}

#[test]
fn current_ghostty_style_imports_font_and_named_theme() {
    let source = r#"
theme = Adventure
font-family = "FiraCode Nerd Font Mono"
window-padding-x = 10
window-padding-y = 10
"#;
    // Unsupported padding keys are ignored; font and theme are imported.
}

#[test]
fn only_supported_single_chord_bindings_are_translated() {
    // Map copy_to_clipboard, paste_from_clipboard, increase_font_size,
    // decrease_font_size, and reset_font_size. Ignore multi-chord and GUI actions.
}

#[test]
fn reset_deletes_only_rchat_managed_files() {
    // Create managed files plus fake Ghostty and standalone ~/.config/ratty files.
    // Reset removes managed files and leaves both external files byte-for-byte unchanged.
}

#[test]
fn failed_import_keeps_previous_managed_config() {
    // Seed a valid managed Ratty TOML, then import malformed color data.
    // The old managed config must remain unchanged.
}
```

- [ ] **Step 2: Run the importer tests and confirm the module lacks implementation**

Run:

```bash
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui ghostty_import
```

Expected: tests fail to compile or fail assertions until config discovery and translation exist.

- [ ] **Step 3: Implement Ghostty config discovery and deterministic layering**

Represent repeated Ghostty keys without losing palette/keybinding entries:

```rust
#[derive(Debug, Default)]
struct GhosttyValues {
    entries: Vec<(String, String)>,
}

impl GhosttyValues {
    fn last(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    }

    fn all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> {
        self.entries
            .iter()
            .filter(move |(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    }
}
```

Search existing files in this order, applying later values last:

1. `$XDG_CONFIG_HOME/ghostty/config.ghostty`
2. `$XDG_CONFIG_HOME/ghostty/config`
3. On macOS, `$HOME/Library/Application Support/com.mitchellh.ghostty/config.ghostty`
4. On macOS, `$HOME/Library/Application Support/com.mitchellh.ghostty/config`

If `XDG_CONFIG_HOME` is unset, use `$HOME/.config`. Process every `config-file` directive after the containing file's ordinary keys, resolve relative includes against the containing file, support optional `?path`, and reject cycles using canonical paths in a `HashSet<PathBuf>`.

Parse only Ghostty's `key = value` grammar: trim whitespace, ignore blank/full-comment lines, split on the first unquoted `=`, remove matching outer quotes, and strip unquoted trailing comments. A malformed line must include its source file and line number in the returned error.

- [ ] **Step 4: Resolve named Ghostty themes and merge their colors before user overrides**

Resolve a `theme` value from:

1. Absolute theme path.
2. `<Ghostty config directory>/themes/<name>`.
3. `$XDG_CONFIG_HOME/ghostty/themes/<name>`.
4. On macOS, `/Applications/Ghostty.app/Contents/Resources/ghostty/themes/<name>`.
5. On macOS, `$HOME/Applications/Ghostty.app/Contents/Resources/ghostty/themes/<name>`.

For `light:name,dark:name`, select the `dark:` entry because the RChat TUI currently defaults to a dark surface. Apply theme-file values first and direct user values second. For repeated `font-family` values, preserve order and treat `font-family = ""` as a reset; import the first family after the final reset. Import only:

- `font-family`, first configured family.
- `font-style`, mapped to Ratty `Regular`, `Bold`, `Italic`, or `BoldItalic` when recognizable.
- `font-size`, rounded to the nearest integer and rejected if outside `1..=96`.
- `foreground`, `background`, `cursor-color`.
- `palette` indices `0..=15`.
- `background-opacity`, clamped to `0.0..=1.0` only after successful numeric parsing.
- Supported `keybind` entries described in Step 5.

Unsupported Ghostty keys must be ignored without appearing in the managed Ratty config.

- [ ] **Step 5: Translate the supported Ghostty keybindings**

Support only a single chord with modifiers `super`, `ctrl`, `alt`, and `shift`. Ignore key sequences containing `>` and actions outside this table:

| Ghostty action | Ratty action |
|---|---|
| `copy_to_clipboard` | `Copy` |
| `paste_from_clipboard` | `Paste` |
| `increase_font_size` or `increase_font_size:<amount>` | `IncreaseFontSize` |
| `decrease_font_size` or `decrease_font_size:<amount>` | `DecreaseFontSize` |
| `reset_font_size` | `ResetFontSize` |

Translate modifiers to Ratty's case-insensitive names `Super`, `Control`, `Alt`, and `Shift`, joined with ` | `. Translate printable alphabetic keys to uppercase Winit names and the common Ghostty names `equal`, `minus`, `zero`, `page_up`, `page_down`, `up`, and `down` to Ratty's `Equal`, `Minus`, `Digit0`, `PageUp`, `PageDown`, `Up`, and `Down`.

- [ ] **Step 6: Serialize a valid partial Ratty configuration and write atomically**

Use serde-serializable output structs with `skip_serializing_if = "Option::is_none"` and `skip_serializing_if = "Vec::is_empty"`. The generated TOML must use Ratty's existing schema:

```toml
[window]
opacity = 0.9

[font]
family = "FiraCode Nerd Font Mono"
style = "Regular"
size = 13

[theme]
foreground = "#ffffff"
background = "#000000"
cursor = "#ffffff"

[theme.normal]
black = "#000000"

[theme.bright]
black = "#666666"

[bindings]
keys = [
  { key = "C", with = "Super", action = "Copy" },
]
```

Validate the finished text with `toml::from_str::<toml::Value>()` before touching the previous managed config. Write `ratty.toml.tmp`, flush it, and rename it over `ratty.toml` only after all parsing, translation, and serialization succeeds. Write matching metadata only after the TOML rename succeeds:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GhosttyImportMetadata {
    pub source_files: Vec<PathBuf>,
    pub theme: Option<String>,
}

pub struct GhosttyImportResult {
    pub config_path: PathBuf,
    pub metadata: GhosttyImportMetadata,
}
```

- [ ] **Step 7: Implement managed reset**

`reset_managed_ratty_config` must remove only:

```text
<app-data>/ratty/ratty.toml
<app-data>/ratty/import.json
<app-data>/ratty/ratty.toml.tmp
```

Return `Ok(true)` if any managed file existed and was removed, `Ok(false)` if the application was already using Ratty defaults. Do not delete the parent directory if it contains any other file.

- [ ] **Step 8: Run all importer tests**

Run:

```bash
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui ghostty_import
```

Expected: discovery, layering, theme resolution, current Ghostty-style fixture, bindings, atomic failure, and reset tests pass.

- [ ] **Step 9: Commit the importer**

```bash
git add \
  src-tauri/crates/rchat-tui/src/ghostty_import.rs \
  src-tauri/crates/rchat-tui/src/lib.rs
git commit -m "feat(tui): import Ghostty appearance for Ratty"
```

---

### Task 4: Add Ratty controls to the existing Media settings section

**Files:**
- Modify: `src-tauri/crates/rchat-tui/src/state.rs` (`SettingsField`, `SettingsModalState`, focus tests)
- Modify: `src-tauri/crates/rchat-tui/src/app.rs` (`refresh_settings_modal`, `activate_settings_focus`, `settings_media_lines`, app tests)

**Interfaces:**
- Consumes: `ratty_host::{load_preferences, save_preferences, resolved_ratty, managed_ratty_config_path}`.
- Consumes: `ghostty_import::{import_from_ghostty, managed_import_status, reset_managed_ratty_config}`.
- Produces Settings fields: `RattyPath`, `RattyPathSave`, `RattyImportGhostty`, `RattyReset`.
- Produces restart-scoped user feedback; importing or changing the host never attempts to replace the currently running terminal process.
- Produces modal errors for discovery, preference-write, import, and reset failures; these actions must not terminate the interactive TUI loop.

- [ ] **Step 1: Add failing Media-section focus/state tests**

Extend `state.rs` tests:

```rust
#[test]
fn media_settings_expose_ratty_path_import_and_reset_controls() {
    let mut modal = SettingsModalState::default();
    modal.activate_section(SettingsSection::Media);

    assert_eq!(
        modal.content_fields(),
        vec![
            SettingsField::RattyPath,
            SettingsField::RattyPathSave,
            SettingsField::RattyImportGhostty,
            SettingsField::RattyReset,
        ]
    );
    assert_eq!(modal.focus, SettingsField::RattyPath);
}

#[test]
fn only_ratty_path_accepts_text_input() {
    let mut modal = SettingsModalState::default();
    modal.activate_section(SettingsSection::Media);
    modal.push_char('/');
    modal.push_char('x');
    assert_eq!(modal.ratty_path, "/x");

    modal.focus = SettingsField::RattyImportGhostty;
    modal.push_char('z');
    assert_eq!(modal.ratty_path, "/x");
}
```

- [ ] **Step 2: Run the state tests and confirm the fields are absent**

Run:

```bash
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui media_settings_expose_ratty_path_import_and_reset_controls
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui only_ratty_path_accepts_text_input
```

Expected: compilation failure until the fields/state are added.

- [ ] **Step 3: Add settings state and focus routing**

Add:

```rust
pub enum SettingsField {
    // existing variants...
    RattyPath,
    RattyPathSave,
    RattyImportGhostty,
    RattyReset,
}
```

Add to `SettingsModalState` and its default:

```rust
pub ratty_path: String,
pub ratty_resolved_path: Option<String>,
pub ratty_path_source: Option<String>,
pub ratty_config_source: String,
pub ratty_warning: Option<String>,
```

Return the four Ratty fields from `content_fields()` for `SettingsSection::Media`. Extend `push_char` and `pop_char` so only `SettingsField::RattyPath` edits `ratty_path`.

- [ ] **Step 4: Populate Ratty status whenever Settings opens or refreshes**

In `refresh_settings_modal`, read the application directory from `app_state.app_dir`, then populate:

```rust
let host_preferences = ratty_host::load_preferences(&app_state.app_dir)?;
let discovery = ratty_host::resolved_ratty(&app_state.app_dir)?;
let imported = ghostty_import::managed_import_status(&app_state.app_dir)?;

modal.ratty_path = host_preferences
    .executable_path
    .map(|path| path.display().to_string())
    .unwrap_or_default();
modal.ratty_resolved_path = discovery
    .resolved
    .as_ref()
    .map(|value| value.path.display().to_string());
modal.ratty_path_source = discovery
    .resolved
    .as_ref()
    .map(|value| value.source.label().to_string());
modal.ratty_warning = discovery.warning;
modal.ratty_config_source = if imported.is_some() {
    "Imported from Ghostty".to_string()
} else {
    "Ratty default/config discovery".to_string()
};
```

Add `RattyPathSource::label(self) -> &'static str` in `ratty_host.rs` with `configured`, `environment`, `sibling`, and `PATH` values.

- [ ] **Step 5: Wire save, import, and reset actions**

Generalize the existing modal helpers so both static and formatted messages are accepted without temporary borrowing:

```rust
fn set_settings_status(state: &mut UiState, message: impl Into<String>) {
    if let Some(modal) = state.app.settings.as_mut() {
        modal.status = Some(message.into());
        modal.error = None;
    }
}

fn set_settings_error(state: &mut UiState, message: impl Into<String>) {
    if let Some(modal) = state.app.settings.as_mut() {
        modal.error = Some(message.into());
        modal.status = None;
    }
}
```

Add match arms in `activate_settings_focus`. Catch each filesystem/import failure and retain it in the modal instead of propagating it out of the interactive loop:

```rust
SettingsField::RattyPathSave => {
    let value = state.app.settings.as_ref().unwrap().ratty_path.trim().to_string();
    let mut preferences = ratty_host::load_preferences(&app_state.app_dir)?;
    preferences.executable_path = if value.is_empty() {
        None
    } else {
        let path = PathBuf::from(&value);
        if !path.is_file() {
            set_settings_error(state, format!("Ratty executable not found: {}", path.display()));
            return Ok(());
        }
        Some(path)
    };
    if let Err(error) = ratty_host::save_preferences(&app_state.app_dir, &preferences) {
        set_settings_error(state, format!("failed to save Ratty path: {error}"));
        return Ok(());
    }
    refresh_settings_modal(app_state, state).await?;
    set_settings_status(state, "Ratty path saved; restart RChat to apply");
}
SettingsField::RattyImportGhostty => {
    let imported = match ghostty_import::import_from_ghostty(&app_state.app_dir) {
        Ok(imported) => imported,
        Err(error) => {
            set_settings_error(state, format!("Ghostty import failed: {error}"));
            return Ok(());
        }
    };
    refresh_settings_modal(app_state, state).await?;
    set_settings_status(
        state,
        format!("Ghostty settings imported to {}; restart RChat to apply", imported.config_path.display()),
    );
}
SettingsField::RattyReset => {
    let changed = match ghostty_import::reset_managed_ratty_config(&app_state.app_dir) {
        Ok(changed) => changed,
        Err(error) => {
            set_settings_error(state, format!("failed to reset Ratty settings: {error}"));
            return Ok(());
        }
    };
    refresh_settings_modal(app_state, state).await?;
    set_settings_status(
        state,
        if changed {
            "Ratty import reset; restart RChat to use Ratty defaults"
        } else {
            "Ratty is already using its defaults"
        },
    );
}
```

Do not restart Ratty from inside the active Ratty-hosted process. All three actions explicitly apply on the next RChat launch.

- [ ] **Step 6: Render Ratty controls and current source in Media settings**

Extend `settings_media_lines` above the existing media counters:

```rust
Line::from("Ratty terminal host"),
Line::from(format!(
    "Resolved: {} ({})",
    modal.ratty_resolved_path.as_deref().unwrap_or("not found"),
    modal.ratty_path_source.as_deref().unwrap_or("none")
)),
Line::from(format!("Configuration: {}", modal.ratty_config_source)),
Line::from(modal.ratty_warning.clone().unwrap_or_default()),
settings_input_line(modal, SettingsField::RattyPath, "Executable path", &modal.ratty_path, theme),
settings_button_line(modal, SettingsField::RattyPathSave, "Save Ratty path", theme),
settings_button_line(modal, SettingsField::RattyImportGhostty, "Import from Ghostty", theme),
settings_button_line(modal, SettingsField::RattyReset, "Reset imported Ratty settings", theme),
Line::from(""),
```

Retain the existing protocol/frame diagnostics below these controls.

- [ ] **Step 7: Run settings tests**

Run:

```bash
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui settings_
```

Expected: existing settings tests and the new Ratty focus/editing tests pass.

- [ ] **Step 8: Commit the Settings UI**

```bash
git add \
  src-tauri/crates/rchat-tui/src/state.rs \
  src-tauri/crates/rchat-tui/src/app.rs \
  src-tauri/crates/rchat-tui/src/ratty_host.rs
git commit -m "feat(tui): manage Ratty host settings"
```

---

### Task 5: Defensively enable Kitty previews whenever Ratty bitmap v1 is detected

**Files:**
- Modify: `src-tauri/crates/rchat-tui/src/app.rs` (`detect_media_backends`, `UiState`, diagnostics, tests)

**Interfaces:**
- Consumes: Ratty's corrected Kitty and `CSI 16t` replies from Task 1.
- Produces: `MediaBackendDetection { picker, ratty_bitmap, kitty_forced }`.
- Guarantees: A positive Ratty bitmap-v1 probe enables Kitty inline preview emission even if `ratatui-image` capability detection unexpectedly returns Halfblocks.
- Leaves unchanged: non-Ratty terminal selection, Kitty fallback, Ratty native static viewer, Ratty native video/screen surfaces.

- [ ] **Step 1: Add failing backend-selection tests**

Replace the tuple-only test seam with a concrete selection result and add:

```rust
#[test]
fn ratty_bitmap_support_forces_kitty_for_inline_previews() {
    let detection = finalize_media_detection(Picker::halfblocks(), true);

    assert_eq!(detection.picker.protocol_type(), ProtocolType::Kitty);
    assert!(detection.ratty_bitmap);
    assert!(detection.kitty_forced);
}

#[test]
fn non_ratty_terminal_keeps_detected_fallback_protocol() {
    let detection = finalize_media_detection(Picker::halfblocks(), false);

    assert_eq!(detection.picker.protocol_type(), ProtocolType::Halfblocks);
    assert!(!detection.ratty_bitmap);
    assert!(!detection.kitty_forced);
}

#[test]
fn native_ratty_kitty_detection_is_not_marked_forced() {
    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Kitty);

    let detection = finalize_media_detection(picker, true);

    assert_eq!(detection.picker.protocol_type(), ProtocolType::Kitty);
    assert!(!detection.kitty_forced);
}
```

- [ ] **Step 2: Run the backend-selection tests and confirm the result type is absent**

Run:

```bash
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui ratty_bitmap_support_forces_kitty
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui non_ratty_terminal_keeps
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui native_ratty_kitty
```

Expected: compilation failure until `MediaBackendDetection` and `finalize_media_detection` exist.

- [ ] **Step 3: Implement explicit media backend selection**

Add:

```rust
struct MediaBackendDetection {
    picker: ratatui_image::picker::Picker,
    ratty_bitmap: bool,
    kitty_forced: bool,
}

fn finalize_media_detection(
    mut picker: ratatui_image::picker::Picker,
    ratty_bitmap: bool,
) -> MediaBackendDetection {
    let kitty_forced = ratty_bitmap && picker.protocol_type() != ProtocolType::Kitty;
    if kitty_forced {
        picker.set_protocol_type(ProtocolType::Kitty);
    }
    MediaBackendDetection {
        picker,
        ratty_bitmap,
        kitty_forced,
    }
}
```

Keep probing order unchanged: Ratty support query first, then `Picker::from_query_stdio`, then `finalize_media_detection`. Update `run_interactive`, `run_media_smoke`, and `run_local_screen_smoke` to derive `kitty_available` from the finalized picker.

This guard is not the primary fix; Task 1 makes normal detection succeed with accurate Ratty cell metrics. The guard prevents previews from disappearing if a capability reply is lost or times out after Ratty bitmap support has already been proven.

- [ ] **Step 4: Retain detection diagnostics in `UiState`**

Add defaulted fields:

```rust
ratty_hosted: bool,
ratty_bitmap_available: bool,
kitty_detection_forced: bool,
```

Initialize all three fields to `false` in every `UiState` constructor/default path so ordinary tests and non-interactive commands do not inherit host state accidentally.

Set them immediately after `UiState::new`:

```rust
state.ratty_hosted = std::env::var("RATTY_SESSION").ok().as_deref() == Some("1");
state.ratty_bitmap_available = detection.ratty_bitmap;
state.kitty_detection_forced = detection.kitty_forced;
```

Display these values in `settings_media_lines`:

```text
Hosted by Ratty: yes/no
Ratty bitmap v1: yes/no
Kitty selection: detected/forced/unavailable
```

- [ ] **Step 5: Add an inline-preview enablement regression test**

Extract the worker gate into a pure helper:

```rust
fn kitty_media_enabled(protocol_type: ProtocolType) -> bool {
    protocol_type == ProtocolType::Kitty
}
```

Test the finalized Ratty detection through the same gate:

```rust
#[test]
fn ratty_detection_enables_inline_media_worker() {
    let detection = finalize_media_detection(Picker::halfblocks(), true);
    assert!(kitty_media_enabled(detection.picker.protocol_type()));
}
```

Use this helper at the three worker creation sites so the test covers the production decision rather than duplicated test-only logic.

- [ ] **Step 6: Run all focused RChat media tests**

Run:

```bash
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui media_detection
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui inline_media
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui ratty_detection_enables_inline_media_worker
```

Expected: Ratty-first probe ordering remains green, Ratty forces Kitty only as a defensive fallback, and inline preview loading is enabled.

- [ ] **Step 7: Commit backend selection and diagnostics**

```bash
git add src-tauri/crates/rchat-tui/src/app.rs
git commit -m "fix(tui): enable Kitty previews inside Ratty"
```

---

### Task 6: Verify normal launch, settings preference, reset, and media rendering end to end

**Files:**
- Modify only if verification exposes an issue: files already listed in Tasks 1–5
- Include in the final RChat commit if still uncommitted: `docs/superpowers/plans/2026-07-15-rchat-ratty-host-kitty-and-settings.md`

**Interfaces:**
- Verifies both repositories independently before the manual cross-process check.
- Verifies the developer launch without `DYLD_LIBRARY_PATH`.
- Verifies Ratty-native viewer/live media and Kitty inline media coexist in one Ratty session.

- [ ] **Step 1: Format and lint Ratty**

Run:

```bash
cd /Users/atasesli/Desktop/VsCode/ratty
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: both commands exit successfully. Fix only warnings introduced by this plan.

- [ ] **Step 2: Run the complete Ratty suite**

Run:

```bash
cd /Users/atasesli/Desktop/VsCode/ratty
cargo nextest run
```

Expected: the entire Ratty suite passes, including bitmap-surface, Kitty, runtime, and terminal tests.

- [ ] **Step 3: Format and lint only the touched RChat package**

Run:

```bash
cd /Users/atasesli/Desktop/VsCode/rchat
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml -p rchat-tui --all-targets -- -D warnings
```

Expected: touched code is formatted and `rchat-tui` has no warnings. If the workspace-wide format check reports pre-existing drift in unrelated dirty files, run `rustfmt --edition 2021 --check` on the touched RChat Rust files and record the unrelated paths instead of rewriting them.

- [ ] **Step 4: Run the complete RChat TUI suite and repository parity check**

Run:

```bash
cargo nextest run --manifest-path src-tauri/Cargo.toml -p rchat-tui
node scripts/check-command-parity.mjs
```

Expected: all `rchat-tui` tests pass and command parity reports success. No command parity allowlist change should be necessary because this plan adds no Tauri command.

- [ ] **Step 5: Build both current binaries without a Swift environment override**

Run:

```bash
cd /Users/atasesli/Desktop/VsCode/ratty
cargo build

cd /Users/atasesli/Desktop/VsCode/rchat
env -u DYLD_LIBRARY_PATH cargo build --manifest-path src-tauri/Cargo.toml -p rchat-tui
```

Expected: both builds succeed and the RChat build embeds the existing `/usr/lib/swift` debug rpath without duplicate `SwiftNativeNSObject` warnings.

- [ ] **Step 6: Verify automatic Ratty launch from the ordinary RChat command**

Run from Ghostty without `DYLD_LIBRARY_PATH`:

```bash
cd /Users/atasesli/Desktop/VsCode/rchat
env -u DYLD_LIBRARY_PATH cargo run --manifest-path src-tauri/Cargo.toml -p rchat-tui
```

Expected:

- Cargo starts `rchat-tui` once in Ghostty.
- The bootstrap resolves the sibling Ratty binary in the shared Cargo target directory or the saved path.
- A new window titled `RChat` opens.
- The child RChat session reports `Hosted by Ratty: yes`, `Ratty bitmap v1: yes`, and `Kitty selection: detected` in Media settings.
- No second Ratty window opens.
- Closing RChat closes the Ratty-hosted process and returns control to the original shell.

- [ ] **Step 7: Verify Ratty path preference and fallback**

In TUI Settings → Media:

1. Set the exact current Ratty binary path and activate **Save Ratty path**.
2. Restart RChat and confirm the resolved source says `configured`.
3. Set a nonexistent path, save must reject it without replacing the valid preference.
4. Launch once with `--no-ratty`; RChat must remain in Ghostty and use its detected Kitty fallback.
5. Remove the explicit path, restart, and confirm sibling/`PATH` discovery resumes.

- [ ] **Step 8: Verify Ghostty import and reset**

In TUI Settings → Media:

1. Record the current default Ratty appearance.
2. Activate **Import from Ghostty**.
3. Confirm Settings reports `Imported from Ghostty` and names the managed config path.
4. Restart RChat; confirm the imported `FiraCode Nerd Font Mono`/`Adventure` values appear when present in the live Ghostty config.
5. Activate **Reset imported Ratty settings**.
6. Restart RChat; confirm Ratty returns to its normal default/config-discovery appearance.
7. Confirm the live Ghostty config and standalone Ratty config have not changed.

- [ ] **Step 9: Verify Kitty previews and Ratty-native surfaces in the same session**

Inside the automatically opened Ratty window:

1. Open a chat containing an image attachment and confirm its inline preview appears.
2. Open the sticker picker and confirm the selected sticker preview appears.
3. Open the New Person QR flow and confirm the QR preview appears.
4. Open the static image viewer and confirm Media diagnostics still report Ratty bitmap v1; pan and zoom must use the Ratty bitmap surface.
5. Run the existing screen/video smoke path and confirm live frames use the Ratty bitmap surface rather than the Kitty worker.
6. Check the launching Ghostty console: `CSI 16t` must no longer be logged as unhandled, and Ratty must not log malformed Kitty probe data.

- [ ] **Step 10: Inspect final diffs and commit the plan if needed**

Run:

```bash
git diff --check
git status --short
git -C ../ratty diff --check
git -C ../ratty status --short
```

Expected: no whitespace errors; all unrelated dirty files remain present and unmodified by this plan.

If the plan file is not already committed with one of the RChat tasks:

```bash
git add docs/superpowers/plans/2026-07-15-rchat-ratty-host-kitty-and-settings.md
git commit -m "docs: plan Ratty-hosted RChat launch"
```

Do not stage or commit any unrelated user changes.
