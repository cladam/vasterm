# vasterm

An exploration of a native, zoomable canvas for multiple terminal sessions.

The implementation direction is Rust with egui/eframe, a custom terminal
surface, and `alacritty_terminal`.

## Development

The initial target is macOS. Install Rust 1.95 or newer, then run:

```sh
cargo run --locked
```

The current app starts your `$SHELL` with `-i` on an 80x24 PTY, processes its
output through the terminal emulator, and displays session status or errors.
Use **Stop session** to terminate the owned shell, or close the window.
Terminal text rendering and keyboard input are the next slice (M1.3), so this
is not an interactive terminal yet. `$SHELL` must be set; startup errors are
shown rather than silently choosing a different shell.

See the [development plan and milestone log](docs/development-plan.md) for
the plan-do-test-verify-reflect cycle, acceptance criteria, and progress.

Local checks (also configured in macOS CI):

```sh
cargo fmt --check
cargo check --locked --all-targets
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```
