//! Extension UI compositor (Phase 6 C8), integration-tested against the
//! REAL Bun sidecar running pi's actual loader/runner with UNMODIFIED
//! corpus extensions (`tests/fixtures/corpus/*`, vendored byte-identical
//! from `.references/pi/packages/coding-agent/examples/extensions/`),
//! asserting the FINAL VT SCREEN GRID (pi-vt) — never internal state.
//!
//! Covered contracts:
//! - widget frames mount above/below the editor with placement, survive a
//!   resize (width-change `ui/render` round trip), and never paint outside
//!   CSI 2026 synchronized updates;
//! - custom header/footer replace the built-ins and restore on dispose;
//! - `ctx.ui.setStatus` reaches the footer; the boot theme baseline is real
//!   JSON (the historical `event: "theme"` ExtensionError is gone);
//! - a custom editor (rainbow-editor) receives focus + forwarded keys, its
//!   frames echo through the wire, and Enter submits through the host;
//! - `ui.custom({overlay: true})` mounts a positioned, focused overlay
//!   whose component consumes arrows/text/Enter and resolves `ui/done`;
//! - `ctx.ui.select` renders the host-native selector and resolves the
//!   extension promise with the picked option;
//! - 100 rapid frames coalesce to the latest.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use pi_coding_agent::ExtensionErrorSink;
use pi_coding_agent::extensions::binding::{BindOptions, ExtensionBinding, SessionHostActions};
use pi_coding_agent::extensions::{BunEnvironment, LauncherSource, SidecarLauncher, resolve_bun};
use pi_coding_agent::modes::interactive::interactive_mode::{
    InteractiveMode, InteractiveModeOptions,
};
use pi_ext_protocol::{CommandExecuteParams, ExtensionError, Request};

use pi_coding_agent::extension_bridge::SessionStartReason;

use common::vt_terminal::{VtHandle, VtTerminal};
use common::{TestRuntime, TestRuntimeOptions, assistant_text_message, make_runtime};

fn sidecar_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sidecar")
        .canonicalize()
        .expect("sidecar package present")
}

fn corpus(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/corpus")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("corpus {name}: {error}"))
}

fn real_launcher() -> SidecarLauncher {
    let bun = resolve_bun(&BunEnvironment::from_env()).expect("bun installed (sidecar tests)");
    let dir = sidecar_dir();
    SidecarLauncher {
        bun,
        entry: dir.join("src/main.ts"),
        cwd: dir,
        envs: Vec::new(),
    }
}

fn assert_no_flicker(handle: &VtHandle) {
    handle.screen(|screen| {
        assert_eq!(
            screen.cells_mutated_outside_sync(),
            0,
            "frame mutated cells outside CSI 2026 synchronization:\n{}",
            screen.serialize()
        );
        assert!(
            screen.sync_frames_completed() > 0,
            "no synchronized frame completed"
        );
        assert!(
            !screen.in_sync_update(),
            "frame left synchronized update open"
        );
    });
}

struct Fixture {
    #[allow(dead_code)]
    runtime: TestRuntime,
    binding: Arc<ExtensionBinding>,
    mode: InteractiveMode,
    handle: VtHandle,
    errors: Arc<Mutex<Vec<ExtensionError>>>,
}

impl Fixture {
    async fn pump_until(&mut self, timeout: Duration, pred: impl Fn(&str) -> bool) -> String {
        self.pump_until_named("screen state", timeout, pred).await
    }

    async fn pump_until_named(
        &mut self,
        stage: &str,
        timeout: Duration,
        pred: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.mode.pump();
            let screen = self.handle.screen(|screen| screen.serialize());
            if pred(&screen) {
                return screen;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {stage}; errors={:?}\n{screen}",
                self.errors.lock()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn send(&mut self, data: &str) {
        self.handle.send_input(data);
        self.mode.pump();
    }

    /// Fire an extension slash command WITHOUT awaiting its completion (a
    /// `ui.custom`/dialog handler resolves only after the host interacts).
    async fn spawn_command(
        &self,
        name: &str,
    ) -> tokio::task::JoinHandle<Result<serde_json::Value, String>> {
        let connection = self
            .binding
            .host()
            .current_connection()
            .await
            .expect("live sidecar");
        let name = name.to_string();
        tokio::spawn(async move {
            connection
                .request(Request::CommandExecute(CommandExecuteParams {
                    name,
                    args: String::new(),
                }))
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn theme_errors(&self) -> Vec<ExtensionError> {
        self.errors
            .lock()
            .iter()
            .filter(|error| error.event == "theme")
            .cloned()
            .collect()
    }
}

/// Real-sidecar interactive fixture: temp runtime, VT grid terminal,
/// InteractiveMode with attached extension UI, booted binding.
async fn fixture(extensions: &[(&str, &str)], options: TestRuntimeOptions) -> Fixture {
    fixture_sized(extensions, options, 100, 30).await
}

async fn fixture_sized(
    extensions: &[(&str, &str)],
    options: TestRuntimeOptions,
    cols: u16,
    rows: u16,
) -> Fixture {
    let runtime = make_runtime(TestRuntimeOptions {
        persisted: true,
        with_auth: true,
        ..options
    })
    .await;
    let ext_dir = runtime.tmp.path().join("extensions");
    std::fs::create_dir_all(&ext_dir).expect("ext dir");
    let mut paths = Vec::new();
    for (name, source) in extensions {
        let path = ext_dir.join(name);
        std::fs::write(&path, source).expect("fixture extension");
        paths.push(path);
    }

    let (terminal, handle) = VtTerminal::new(cols, rows);
    let mut mode = InteractiveMode::new(
        runtime.runtime.clone(),
        terminal,
        InteractiveModeOptions::default(),
    );
    mode.init();

    let errors: Arc<Mutex<Vec<ExtensionError>>> = Arc::default();
    let sink_errors = errors.clone();
    let error_sink: ExtensionErrorSink = Arc::new(move |error| sink_errors.lock().push(error));
    let actions = SessionHostActions::new();

    let bind_options = BindOptions::new(
        paths,
        LauncherSource::Resolved(real_launcher()),
        runtime.tmp.path().join("project"),
        runtime.tmp.path().join("agent"),
        runtime.tmp.path().join("sessions"),
        error_sink,
        actions.clone(),
    );

    let binding = mode
        .bind_extensions(bind_options)
        .expect("canonical extension paths")
        .expect("extensions discovered");
    actions.attach(&binding);
    actions.attach_runtime(runtime.runtime.clone());

    binding
        .start(SessionStartReason::Startup)
        .await
        .expect("sidecar boots");

    Fixture {
        runtime,
        binding,
        mode,
        handle,
        errors,
    }
}

// ============================================================================
// Widgets: mount, placement, resize, coalescing
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn widget_frames_mount_with_placement_and_survive_resize() {
    let mut fx = fixture(
        &[("widget-placement.ts", &corpus("widget-placement.ts"))],
        TestRuntimeOptions::default(),
    )
    .await;

    let screen = fx
        .pump_until(Duration::from_secs(20), |screen| {
            screen.contains("Above editor widget") && screen.contains("Below editor widget")
        })
        .await;

    // Placement: above < editor area < below (grid row order).
    let (above_row, below_row) = fx.handle.screen(|s| {
        (
            s.find_row("Above editor widget").expect("above row"),
            s.find_row("Below editor widget").expect("below row"),
        )
    });
    assert!(
        above_row < below_row,
        "above widget must render before below widget:\n{screen}"
    );

    // Resize: leaves request re-renders at the new width; content survives.
    fx.handle.resize(70, 24);
    fx.pump_until(Duration::from_secs(10), |screen| {
        screen.contains("Above editor widget") && screen.contains("Below editor widget")
    })
    .await;

    assert!(fx.theme_errors().is_empty(), "boot theme must be real JSON");
    assert_no_flicker(&fx.handle);
}

#[tokio::test(flavor = "current_thread")]
async fn hundred_rapid_frames_coalesce_to_the_latest() {
    // Synthetic (non-corpus) extension: 100 immediate widget updates.
    let rapid = r#"
export default function (pi) {
    pi.on("session_start", (_event, ctx) => {
        if (!ctx.hasUI) return;
        for (let i = 1; i <= 100; i++) {
            ctx.ui.setWidget("rapid", [`rapid frame ${i}`]);
        }
    });
}
"#;
    let mut fx = fixture(&[("rapid.ts", rapid)], TestRuntimeOptions::default()).await;

    let screen = fx
        .pump_until(Duration::from_secs(20), |screen| {
            screen.contains("rapid frame 100")
        })
        .await;
    assert!(
        !screen.contains("rapid frame 99"),
        "only the latest frame may remain:\n{screen}"
    );
    assert_no_flicker(&fx.handle);
}

// ============================================================================
// Header / footer replacement + statuses
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn custom_header_replaces_and_command_restores() {
    let mut fx = fixture(
        &[("custom-header.ts", &corpus("custom-header.ts"))],
        TestRuntimeOptions::default(),
    )
    .await;

    // Mascot subtitle proves the header frame mounted at the top.
    let screen = fx
        .pump_until(Duration::from_secs(20), |screen| {
            screen.contains("shitty coding agent")
        })
        .await;
    let header_row = fx
        .handle
        .screen(|s| s.find_row("shitty coding agent").expect("header row"));
    assert!(header_row < 12, "header renders at the top:\n{screen}");

    // The restore command disposes the slot and notifies.
    let task = fx.spawn_command("builtin-header").await;
    fx.pump_until(Duration::from_secs(10), |screen| {
        screen.contains("Built-in header restored") && !screen.contains("shitty coding agent")
    })
    .await;
    task.await
        .expect("command task completes")
        .expect("extension command succeeds");
    assert_no_flicker(&fx.handle);
}

#[tokio::test(flavor = "current_thread")]
async fn custom_footer_toggles_and_statuses_reach_the_footer() {
    let mut fx = fixture(
        &[
            ("custom-footer.ts", &corpus("custom-footer.ts")),
            ("status-line.ts", &corpus("status-line.ts")),
        ],
        TestRuntimeOptions::default(),
    )
    .await;

    // status-line.ts: session_start sets the "Ready" status (built-in
    // footer renders extension statuses).
    fx.pump_until(Duration::from_secs(20), |screen| screen.contains("Ready"))
        .await;

    // custom-footer.ts /footer: replaces the footer with the token line.
    let task = fx.spawn_command("footer").await;
    let screen = fx
        .pump_until(Duration::from_secs(10), |screen| {
            screen.contains("Custom footer enabled") && screen.contains("$0.000")
        })
        .await;
    assert!(
        screen.contains("\u{2191}0"),
        "custom footer tokens:\n{screen}"
    );
    task.await
        .expect("command task completes")
        .expect("extension command succeeds");

    // Toggle back: built-in footer returns (statuses visible again).
    let task = fx.spawn_command("footer").await;
    fx.pump_until(Duration::from_secs(10), |screen| {
        screen.contains("Default footer restored") && !screen.contains("$0.000")
    })
    .await;
    task.await
        .expect("command task completes")
        .expect("extension command succeeds");
    assert_no_flicker(&fx.handle);
}

// ============================================================================
// Custom editor: focus, forwarded input, submit
const INLINE_CUSTOM_EXTENSION: &str = r#"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  pi.registerCommand("inline-custom", {
    description: "Exercise non-overlay custom editor swap",
    handler: async (_args, ctx) => {
      await ctx.ui.custom(
        (_tui, _theme, _keybindings, done) => ({
          focused: false,
          render: () => ["INLINE CUSTOM"],
          handleInput: (data: string) => {
            if (data === "\r") done("closed");
          },
          invalidate: () => {},
        }),
      );
    },
  });

  pi.registerCommand("set-editor", {
    description: "Exercise extension-driven editor replacement",
    handler: async (_args, ctx) => {
      ctx.ui.setEditorText("extension draft");
    },
  });
}
"#;

// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn custom_editor_receives_input_and_submits_through_the_host() {
    let seed = make_runtime(TestRuntimeOptions {
        with_auth: true,
        ..Default::default()
    })
    .await;
    let response = assistant_text_message(&seed.model, "editor round trip done");
    drop(seed);
    let mut fx = fixture(
        &[
            ("rainbow-editor.ts", &corpus("rainbow-editor.ts")),
            ("inline-custom.ts", INLINE_CUSTOM_EXTENSION),
        ],
        TestRuntimeOptions {
            script: vec![response],
            ..Default::default()
        },
    )
    .await;

    // The bridged editor mounts on session_start; typed keys round-trip
    // through ui/input → sidecar render → ui/frame.
    fx.pump_until(Duration::from_secs(20), |_| true).await;
    fx.send("say ultrathink");
    let screen = fx
        .pump_until(Duration::from_secs(10), |screen| {
            screen.contains("say ultrathink")
        })
        .await;
    assert!(
        screen.contains("ultrathink"),
        "echo through frames:\n{screen}"
    );

    // A non-overlay custom component temporarily replaces the active editor;
    // resolving it must restore the bridged editor's draft, not the dormant
    // native Editor buffer.
    let custom = fx.spawn_command("inline-custom").await;
    fx.pump_until(Duration::from_secs(10), |screen| {
        screen.contains("INLINE CUSTOM")
    })
    .await;
    fx.send("\r");
    fx.pump_until(Duration::from_secs(10), |screen| {
        !screen.contains("INLINE CUSTOM") && screen.contains("say ultrathink")
    })
    .await;
    custom
        .await
        .expect("inline custom task completes")
        .expect("inline custom command succeeds");

    // Programmatic setEditorText follows the same mirror and therefore also
    // survives a custom swap.
    fx.spawn_command("set-editor")
        .await
        .await
        .expect("set-editor task completes")
        .expect("set-editor command succeeds");
    fx.pump_until(Duration::from_secs(10), |screen| {
        screen.contains("extension draft")
    })
    .await;
    let custom = fx.spawn_command("inline-custom").await;
    fx.pump_until(Duration::from_secs(10), |screen| {
        screen.contains("INLINE CUSTOM")
    })
    .await;
    fx.send("\r");
    fx.pump_until(Duration::from_secs(10), |screen| {
        !screen.contains("INLINE CUSTOM") && screen.contains("extension draft")
    })
    .await;
    custom
        .await
        .expect("second inline custom task completes")
        .expect("second inline custom command succeeds");

    // Enter → ui/editorSubmit → host submit path → scripted stream.
    fx.send("\r");
    fx.pump_until(Duration::from_secs(20), |screen| {
        screen.contains("editor round trip done")
    })
    .await;

    assert_no_flicker(&fx.handle);
}

// ============================================================================
// Overlays: ui.custom({overlay}) mount, bounds, focus, input, done
// ============================================================================
const RESPONSIVE_OVERLAY_EXTENSION: &str = r#"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  pi.registerCommand("responsive-overlay", {
    description: "Exercise responsive overlay visibility",
    handler: async (_args, ctx) => {
      let terminalWidth = () => 80;
      await ctx.ui.custom(
        (tui, _theme, _keybindings, done) => {
          terminalWidth = () => tui.terminal.columns;
          return {
            focused: false,
            render: () => ["RESPONSIVE OVERLAY"],
            handleInput: (data: string) => {
              if (data === "\r") done("closed");
            },
            invalidate: () => {},
          };
        },
        {
          overlay: true,
          overlayOptions: () => ({
            visible: (width: number) => width >= 90,
            width: terminalWidth() >= 110 ? 60 : 80,
          }),
        },
      );
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn responsive_overlay_tracks_host_resize() {
    let mut fx = fixture(
        &[("responsive-overlay.ts", RESPONSIVE_OVERLAY_EXTENSION)],
        TestRuntimeOptions::default(),
    )
    .await;
    fx.pump_until(Duration::from_secs(20), |_| true).await;
    assert!(
        fx.binding
            .registered_commands()
            .iter()
            .any(|command| command.invocation_name == "responsive-overlay"),
        "responsive overlay command registration missing; errors={:?}",
        fx.errors.lock()
    );
    let task = fx.spawn_command("responsive-overlay").await;
    let screen = fx
        .pump_until_named(
            "responsive overlay mount",
            Duration::from_secs(10),
            |screen| screen.contains("RESPONSIVE OVERLAY") || task.is_finished(),
        )
        .await;
    if !screen.contains("RESPONSIVE OVERLAY") {
        panic!(
            "responsive command completed before mount: {:?}",
            task.await.expect("command task joins")
        );
    }
    let initial_col = screen
        .lines()
        .find(|line| line.contains("RESPONSIVE OVERLAY"))
        .and_then(|line| line.find("RESPONSIVE OVERLAY"))
        .expect("responsive overlay column");
    assert_eq!(initial_col, 10, "80-column overlay must center at 10");

    fx.handle.resize(120, 30);
    let screen = fx
        .pump_until_named(
            "responsive overlay layout update",
            Duration::from_secs(10),
            |screen| {
                screen
                    .lines()
                    .find(|line| line.contains("RESPONSIVE OVERLAY"))
                    .and_then(|line| line.find("RESPONSIVE OVERLAY"))
                    == Some(30)
            },
        )
        .await;
    assert!(screen.contains("RESPONSIVE OVERLAY"));

    fx.handle.resize(80, 30);
    fx.pump_until_named(
        "responsive overlay hide",
        Duration::from_secs(10),
        |screen| !screen.contains("RESPONSIVE OVERLAY"),
    )
    .await;

    fx.handle.resize(100, 30);
    fx.pump_until_named(
        "responsive overlay show",
        Duration::from_secs(10),
        |screen| screen.contains("RESPONSIVE OVERLAY"),
    )
    .await;

    fx.send("\r");
    fx.pump_until_named(
        "responsive overlay done",
        Duration::from_secs(10),
        |screen| !screen.contains("RESPONSIVE OVERLAY"),
    )
    .await;
    task.await
        .expect("command task completes")
        .expect("extension command succeeds");
    assert_no_flicker(&fx.handle);
}

#[tokio::test(flavor = "current_thread")]
async fn custom_overlay_mounts_focused_takes_input_and_resolves_done() {
    let mut fx = fixture(
        &[("overlay-test.ts", &corpus("overlay-test.ts"))],
        TestRuntimeOptions::default(),
    )
    .await;
    fx.pump_until(Duration::from_secs(20), |_| true).await;

    assert!(
        fx.binding
            .registered_commands()
            .iter()
            .any(|command| command.invocation_name == "overlay-test"),
        "overlay-test command registration missing; errors={:?}",
        fx.errors.lock()
    );
    let task = fx.spawn_command("overlay-test").await;
    let screen = fx
        .pump_until(Duration::from_secs(10), |screen| {
            screen.contains("Search") && screen.contains("Cancel")
        })
        .await;

    // Overlay bounds: pi-tui's default is 80 columns (oracle tui.ts:920);
    // on a 100-column terminal the overlay rectangle starts at column 10.
    let (row, col) = fx.handle.screen(|s| {
        let row = s.find_row("Search").expect("overlay content row");
        let line = s.rows()[row].clone();
        let col = line.find('│').expect("overlay left edge on content row");
        (row, col)
    });
    assert!(
        col == 10,
        "80-column overlay is centered (edge at {row}:{col}):\n{screen}"
    );

    // Focused overlay consumes typed input (the Search item's inline text
    // input) and Enter resolves done() → notify through the host.
    for ch in ["g", "r", "i", "d"] {
        fx.send(ch);
    }
    fx.pump_until(Duration::from_secs(10), |screen| screen.contains("grid"))
        .await;
    fx.send("\r");
    fx.pump_until(Duration::from_secs(10), |screen| {
        screen.contains("Search: \"grid\"")
    })
    .await;
    task.await
        .expect("command task completes")
        .expect("extension command succeeds");

    // The overlay unmounted with ui/done.
    let screen = fx
        .pump_until(Duration::from_secs(10), |screen| !screen.contains("Cancel"))
        .await;
    assert!(
        !screen.contains("Run"),
        "overlay content removed:\n{screen}"
    );
    assert_no_flicker(&fx.handle);
}

// ============================================================================
// Host-native dialogs (ctx.ui.select)
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn select_dialog_renders_natively_and_resolves_the_extension() {
    let picker = r#"
export default function (pi) {
    pi.registerCommand("pick", {
        description: "pick a fruit",
        handler: async (_args, ctx) => {
            const choice = await ctx.ui.select("Pick a fruit", ["Apple", "Banana", "Cherry"]);
            ctx.ui.notify(`picked: ${choice ?? "nothing"}`, "info");
        },
    });
}
"#;
    let mut fx = fixture(&[("picker.ts", picker)], TestRuntimeOptions::default()).await;
    fx.pump_until(Duration::from_secs(20), |_| true).await;

    let task = fx.spawn_command("pick").await;
    fx.pump_until(Duration::from_secs(10), |screen| {
        screen.contains("Pick a fruit") && screen.contains("Banana")
    })
    .await;

    // Arrow down + Enter picks Banana in the HOST-native selector.
    fx.send("\u{1b}[B");
    fx.send("\r");
    fx.pump_until(Duration::from_secs(10), |screen| {
        screen.contains("picked: Banana")
    })
    .await;
    task.await
        .expect("command task completes")
        .expect("extension command succeeds");
    assert_no_flicker(&fx.handle);
}
