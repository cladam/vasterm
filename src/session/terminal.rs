use std::cell::RefCell;
use std::rc::Rc;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Osc52};
use alacritty_terminal::vte::ansi::{Processor, Rgb};
use anyhow::{Result, ensure};

use super::MAX_INPUT_BYTES;

pub const SCROLLBACK_LINES: usize = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    columns: u16,
    rows: u16,
}

/// Grid and physical cell dimensions shared by the emulator and PTY.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalSize {
    pub grid: GridSize,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl TerminalSize {
    pub(super) fn pty_size(self) -> Result<portable_pty::PtySize> {
        Ok(portable_pty::PtySize {
            rows: self.grid.rows(),
            cols: self.grid.columns(),
            pixel_width: self
                .grid
                .columns()
                .checked_mul(self.cell_width)
                .ok_or_else(|| anyhow::anyhow!("terminal pixel width exceeds PTY limit"))?,
            pixel_height: self
                .grid
                .rows()
                .checked_mul(self.cell_height)
                .ok_or_else(|| anyhow::anyhow!("terminal pixel height exceeds PTY limit"))?,
        })
    }
}

impl GridSize {
    pub fn new(columns: u16, rows: u16) -> Result<Self> {
        ensure!(columns >= 2, "a terminal needs at least two columns");
        ensure!(rows >= 1, "a terminal needs at least one row");
        Ok(Self { columns, rows })
    }

    pub fn rows(self) -> u16 {
        self.rows
    }

    pub fn columns(self) -> u16 {
        self.columns
    }
}

impl Default for GridSize {
    fn default() -> Self {
        Self {
            columns: 80,
            rows: 24,
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        usize::from(self.rows)
    }

    fn columns(&self) -> usize {
        usize::from(self.columns)
    }
}

#[derive(Clone, Default)]
pub struct TerminalEvents(Rc<RefCell<Vec<Event>>>);

impl EventListener for TerminalEvents {
    fn send_event(&self, event: Event) {
        if matches!(
            event,
            Event::PtyWrite(_) | Event::ColorRequest(..) | Event::TextAreaSizeRequest(_)
        ) {
            self.0.borrow_mut().push(event);
        }
    }
}

pub(super) struct TerminalState {
    pub core: Term<TerminalEvents>,
    pub size: TerminalSize,
    parser: Processor,
    events: TerminalEvents,
}

impl TerminalState {
    pub fn new(size: GridSize) -> Self {
        let events = TerminalEvents::default();
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            osc52: Osc52::Disabled,
            kitty_keyboard: false,
            ..Config::default()
        };
        Self {
            core: Term::new(config, &size, events.clone()),
            size: TerminalSize {
                grid: size,
                ..TerminalSize::default()
            },
            parser: Processor::new(),
            events,
        }
    }

    pub fn advance(
        &mut self,
        bytes: &[u8],
        mut respond: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut replies = Vec::new();
        for byte in bytes {
            // Handle queries before subsequent bytes can change their referenced state.
            self.parser.advance(&mut self.core, &[*byte]);
            let events = std::mem::take(&mut *self.events.0.borrow_mut());
            for event in events {
                let response = match event {
                    Event::PtyWrite(response) => response,
                    Event::ColorRequest(index, format) => {
                        let color = self.core.colors()[index]
                            .or_else(|| default_color(index))
                            .ok_or_else(|| anyhow::anyhow!("unsupported terminal color {index}"))?;
                        format(color)
                    }
                    Event::TextAreaSizeRequest(format) => format(WindowSize {
                        num_lines: self.core.screen_lines() as u16,
                        num_cols: self.core.columns() as u16,
                        cell_width: self.size.cell_width,
                        cell_height: self.size.cell_height,
                    }),
                    _ => unreachable!("only protocol response events are queued"),
                };
                ensure!(
                    response.len() <= MAX_INPUT_BYTES,
                    "terminal response exceeds input limit"
                );
                if replies.len() + response.len() > MAX_INPUT_BYTES {
                    respond(&replies)?;
                    replies.clear();
                }
                replies.extend_from_slice(response.as_bytes());
            }
        }
        if !replies.is_empty() {
            respond(&replies)?;
        }
        Ok(())
    }
}

pub fn default_color(index: usize) -> Option<Rgb> {
    const ANSI: [u32; 16] = [
        0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5, 0x7f7f7f,
        0xff0000, 0x00ff00, 0xffff00, 0x5c5cff, 0xff00ff, 0x00ffff, 0xffffff,
    ];
    let rgb = match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let n = index - 16;
            let level = |n: usize| if n == 0 { 0 } else { 55 + n as u32 * 40 };
            (level(n / 36) << 16) | (level(n / 6 % 6) << 8) | level(n % 6)
        }
        232..=255 => {
            let level = 8 + (index as u32 - 232) * 10;
            (level << 16) | (level << 8) | level
        }
        256 | 258 => 0xe5e5e5,
        257 => 0x000000,
        259..=266 | 268 => {
            let base = if index == 268 {
                0xe5e5e5
            } else {
                ANSI[index - 259]
            };
            let dim = |shift: u32| (((base >> shift) & 0xff_u32) * 2 / 3) << shift;
            dim(16) | dim(8) | dim(0)
        }
        267 => 0xffffff,
        _ => return None,
    };
    Some(Rgb {
        r: (rgb >> 16) as u8,
        g: (rgb >> 8) as u8,
        b: rgb as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_rejects_dimensions_the_emulator_cannot_use() {
        assert!(GridSize::new(0, 24).is_err());
        assert!(GridSize::new(1, 24).is_err());
        assert!(GridSize::new(80, 0).is_err());
        assert!(GridSize::new(2, 1).is_ok());
    }

    #[test]
    fn queries_observe_state_at_the_query_not_the_end_of_the_chunk() {
        let mut terminal = TerminalState::new(GridSize::default());
        let mut responses = Vec::new();
        terminal
            .advance(
                b"\x1b[4;7H\x1b[6n\x1b[H\x1b]4;1;#123456\x07\x1b]4;1;?\x07\x1b]104;1\x07",
                |bytes| {
                    responses.extend_from_slice(bytes);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(responses, b"\x1b[4;7R\x1b]4;1;rgb:1212/3434/5656\x07");
    }

    #[test]
    fn query_bursts_are_batched_without_exceeding_the_input_byte_limit() {
        let mut terminal = TerminalState::new(GridSize::default());
        let mut responses = Vec::new();
        terminal
            .advance(&b"\x1b[6n".repeat(1_000), |bytes| {
                assert!(!bytes.is_empty());
                assert!(bytes.len() <= MAX_INPUT_BYTES);
                responses.extend_from_slice(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(responses, b"\x1b[1;1R".repeat(1_000));
    }

    #[test]
    fn response_transport_errors_are_not_ignored() {
        let mut terminal = TerminalState::new(GridSize::default());
        let error = terminal
            .advance(b"\x1b[6n", |_| anyhow::bail!("injected write failure"))
            .unwrap_err();
        assert!(error.to_string().contains("injected write failure"));
    }

    #[test]
    fn utf8_and_escapes_survive_split_output_chunks() {
        use alacritty_terminal::index::{Column, Line, Point};
        use alacritty_terminal::vte::ansi::{Color, NamedColor};

        let mut terminal = TerminalState::new(GridSize::default());
        for bytes in [&b"\x1b[3"[..], &b"1m\xc3"[..], &b"\xa9"[..]] {
            terminal.advance(bytes, |_| Ok(())).unwrap();
        }
        let cell = &terminal.core.grid()[Point::new(Line(0), Column(0))];
        assert_eq!(cell.c, '\u{e9}');
        assert_eq!(cell.fg, Color::Named(NamedColor::Red));
    }

    #[test]
    fn palette_covers_cube_and_grayscale_boundaries() {
        assert_eq!(default_color(16), Some(Rgb { r: 0, g: 0, b: 0 }));
        assert_eq!(
            default_color(231),
            Some(Rgb {
                r: 255,
                g: 255,
                b: 255
            })
        );
        assert_eq!(default_color(232), Some(Rgb { r: 8, g: 8, b: 8 }));
        assert_eq!(
            default_color(255),
            Some(Rgb {
                r: 238,
                g: 238,
                b: 238
            })
        );
        assert_eq!(default_color(269), None);
    }
}
