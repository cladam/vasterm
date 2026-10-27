# vasterm

An exploration of a native, zoomable canvas for multiple terminal sessions.

The implementation direction is Rust with egui/eframe, a custom terminal
surface, and `alacritty_terminal`.

## Development

The initial target is macOS. Install Rust 1.95 or newer, then run:

```sh
cargo run --locked
```

The current bootstrap opens a native window, but does not start a shell yet.
See the [development plan and milestone log](docs/development-plan.md) for
the plan-do-test-verify-reflect cycle, acceptance criteria, and progress.

Local checks (also configured in macOS CI):

```sh
cargo fmt --check
cargo check --locked --all-targets
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```
