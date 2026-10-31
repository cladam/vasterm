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
verification; M1.4 load/lifecycle is next. Rendering uses egui's bundled fonts with on-demand system-font
fallback for missing glyphs, including CJK. Coverage depends on installed
fonts; complex/combining-mark shaping remains a limitation. Bold is synthesized,
underline variants use a single line, and the cursor is steady rather than
blinking. On macOS, **left Option is Meta** (e.g. Option+B/F sends Escape+b/f
for shell word navigation); **right Option keeps native composed text** and
dead keys. Command shortcuts are not sent as shell keys. Selection, clipboard/paste, scrollback
navigation, and terminal mouse reporting remain deferred to M4. Pasting text
copied from another application is not supported yet, including Command+V.

See the [development plan and milestone log](docs/development-plan.md) for
the plan-do-test-verify-reflect cycle, acceptance criteria, and progress.

Local checks (also configured in macOS CI):

```sh
cargo fmt --check
cargo check --locked --all-targets
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```
