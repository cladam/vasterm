# vasterm

Vasterm is a native terminal application for macOS, written in Rust.
The aim is to put multiple terminal sessions on a canvas that you can pan
and zoom. You could keep an editor, a shell and logs alongside each other,
arrange them as you like, and zoom out to see the whole workspace.

The name combines *vast* and *terminal*. The project takes ideas from
terminal multiplexers, tiling window managers and zoomable interfaces.

**Vasterm is an early prototype.** It currently runs one local terminal;
multiple sessions and the canvas are not implemented yet.

## Current features

- An interactive session using your configured `$SHELL`.
- Terminal colours, text attributes, cursor movement and full-screen
  applications such as Vim.
- Keyboard shortcuts, Unicode text and input method (IME) support.
- System-font fallback for missing glyphs, including CJK where suitable
  fonts are installed.
- A terminal grid that resizes with the window.
- Session shutdown that runs separately from the interface, with visible
  errors and an option to retry if it fails.

Output buffering is bounded, and terminal processing is split across
updates to keep the interface responsive. The application does not request
periodic redraws while idle. The optional **Load diagnostics** panel shows
activity and timing counters without recording terminal contents or input.

## Build and run

You need **macOS**, **Rust 1.95 or newer**, and a configured `$SHELL`.
Build and run from source:

```sh
git clone https://github.com/cladam/vasterm.git
cd vasterm
cargo run --locked
```

Click the terminal to give it keyboard focus. **Left Option acts as Meta**
for shortcuts such as Option+B/F; **right Option retains composed text and
dead keys**.

Use **Stop session** or close the window to end the session. Vasterm attempts
to terminate the shell it started and its foreground job. This does not
cover detached processes or unrelated background jobs. If cleanup fails,
the window stays open and offers **Retry cleanup**.

## Planned features

- Independent terminals sharing one workspace.
- Moveable panes that you can resize, with canvas panning and zooming.
- Zooming without changing a terminal's rows and columns.
- Selection, copy/paste, scrollback navigation, and terminal mouse support.
- Saved workspace layouts. Restoring a layout will not restore running
  shell processes.

## Limitations

Vasterm is not ready to replace your everyday terminal. **Text selection,
clipboard paste (including Command+V), scrollback navigation and terminal
mouse reporting are not implemented.**

Font coverage depends on installed fonts, and support for complex text
shaping is limited. Other platforms are not yet supported.

## Development

Vasterm is written in Rust, using [egui/eframe](https://github.com/emilk/egui)
for the native interface, a custom cell-rendered terminal surface,
[`alacritty_terminal`](https://github.com/alacritty/alacritty) for terminal
emulation, and [`portable-pty`](https://github.com/wezterm/wezterm/tree/main/pty)
for PTY integration.

The [development plan](docs/development-plan.md) covers the architecture,
milestones, verification exercises and development history.

Local development checks:

```sh
cargo fmt --check
cargo check --locked --all-targets
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

## Licence

[MIT](LICENSE).
