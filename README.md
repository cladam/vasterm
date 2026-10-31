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

M1.3 is implemented but awaits native shell, `vim`, resize, and display-scale
verification. Rendering uses egui's bundled fonts: missing glyphs, CJK/emoji
coverage, and combining-mark shaping remain limitations. Bold is synthesized,
underline variants use a single line, and the cursor is steady rather than
blinking. Option-generated text follows the native keyboard layout; Command
shortcuts are not sent as shell keys. Selection, clipboard/paste, scrollback
navigation, and terminal mouse reporting remain deferred to M4.

See the [development plan and milestone log](docs/development-plan.md) for
the plan-do-test-verify-reflect cycle, acceptance criteria, and progress.

Local checks (also configured in macOS CI):

```sh
cargo fmt --check
cargo check --locked --all-targets
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```
