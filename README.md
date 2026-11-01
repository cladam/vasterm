# vasterm

An exploration of a native, zoomable canvas for multiple terminal sessions.

The implementation direction is Rust with egui/eframe, a custom terminal
surface, and `alacritty_terminal`.

## Development

The initial target is macOS. Install Rust 1.95 or newer, then run:

```sh
cargo run --locked
```

The current app starts your `$SHELL` with `-i`, draws its terminal cells, and
routes focused keyboard input and committed text/IME input to the shell.
The initial 80x24 grid resizes to the available cell dimensions; the status
line shows acknowledged columns/rows and display scale. Click the terminal
to focus it. Use **Stop session** to terminate the owned shell, or close the
window. `$SHELL` must be set; startup and interaction errors are shown rather
than silently choosing a different shell or discarding failed input.

M1.3 drawing/input/resize is complete following user-confirmed native
verification. M1.4 is in progress: its first bounds/measurement step is
implemented; responsive scheduling and lifecycle hardening remain open.
Rendering uses egui's bundled fonts with on-demand system-font
fallback for missing glyphs, including CJK. Coverage depends on installed
fonts; complex/combining-mark shaping remains a limitation. Bold is synthesized,
underline variants use a single line, and the cursor is steady rather than
blinking. On macOS, **left Option is Meta** (e.g. Option+B/F sends Escape+b/f
for shell word navigation); **right Option keeps native composed text** and
dead keys. Command shortcuts are not sent as shell keys. Selection, clipboard/paste, scrollback
navigation, and terminal mouse reporting remain deferred to M4. Pasting text
copied from another application is not supported yet, including Command+V.

Expand **Load diagnostics** for passive, cumulative counters: UI/logic passes,
worker wake requests, output backpressure retries, processed bytes, maximum
poll/chunk/UI durations, and user-input delivery latency. No timer refreshes
the panel, and no input or terminal contents are recorded. Delivery means the
complete input message was written to the PTY, not that the child consumed it;
protocol replies are excluded from user-input timing. UI passes count egui
submissions, not GPU frames. Maxima are since startup, and worker measurements
are sampled independently.

Input and output queues each hold up to 32 messages of at most 4 KiB;
in-flight chunks are additional. History is capped at 1,000 lines, and a
session update now consumes at most 64 KiB even with uneven chunk lengths.
The 4 ms elapsed-time budget is soft and checked between chunks: a slow
chunk can exceed it. Native load/idle acceptance is not yet verified.
For the idle exercise, open the panel first, let startup/layout/animations
settle, then leave the window untouched for 30 seconds. The numbers only
redraw on a subsequent UI pass; record any observation-triggering event
separately, or inspect counters through the debugger without requesting
repaint. Opening the panel changes available terminal height and may resize
the grid, so do not open/close it during the measurement interval.

See the [development plan and milestone log](docs/development-plan.md) for
the plan-do-test-verify-reflect cycle, acceptance criteria, and progress.

Local checks (also configured in macOS CI):

```sh
cargo fmt --check
cargo check --locked --all-targets
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```
