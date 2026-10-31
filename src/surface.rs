use alacritty_terminal::Term;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use anyhow::Result;
use eframe::egui::{self, Color32, FontId, Pos2, Rect, Stroke, Vec2};

use crate::input::TerminalInput;
use crate::session::{
    GridSize, Session, SessionState, TerminalEvents, TerminalSize, default_color,
};

#[derive(Default)]
pub(crate) struct TerminalSurface {
    input: TerminalInput,
    initialized: bool,
}

#[derive(Clone, Copy)]
struct Metrics {
    cell: Vec2,
    pixels_per_point: f32,
    width: u16,
    height: u16,
}

impl Metrics {
    fn new(cell: Vec2, pixels_per_point: f32) -> Self {
        let width = (cell.x * pixels_per_point).ceil().clamp(1.0, 32767.0) as u16;
        let height = (cell.y * pixels_per_point).ceil().clamp(1.0, 65535.0) as u16;
        Self {
            cell: egui::vec2(f32::from(width), f32::from(height)) / pixels_per_point,
            pixels_per_point,
            width,
            height,
        }
    }

    fn size(self, available: Vec2) -> Result<TerminalSize> {
        Ok(TerminalSize {
            grid: GridSize::new(
                (available.x / self.cell.x)
                    .floor()
                    .clamp(2.0, f32::from(u16::MAX / self.width)) as u16,
                (available.y / self.cell.y)
                    .floor()
                    .clamp(1.0, f32::from(u16::MAX / self.height)) as u16,
            )?,
            cell_width: self.width,
            cell_height: self.height,
        })
    }

    fn rect(self, origin: Pos2, column: usize, row: i32, wide: bool) -> Rect {
        Rect::from_min_size(
            origin + egui::vec2(column as f32 * self.cell.x, row as f32 * self.cell.y),
            egui::vec2(self.cell.x * if wide { 2.0 } else { 1.0 }, self.cell.y),
        )
    }
}

impl TerminalSurface {
    pub fn show(&mut self, ui: &mut egui::Ui, session: &mut Session) -> Result<()> {
        let font = FontId::monospace(14.0);
        let cell = ui
            .fonts_mut(|fonts| egui::vec2(fonts.glyph_width(&font, 'M'), fonts.row_height(&font)));
        let metrics = Metrics::new(cell, ui.ctx().pixels_per_point());
        let (_, allocated) = ui.allocate_space(ui.available_size());
        let response = ui.interact(
            allocated,
            ui.make_persistent_id("terminal-surface"),
            egui::Sense::click(),
        );
        let scale = metrics.pixels_per_point;
        let min = egui::pos2(
            (allocated.left() * scale).ceil() / scale,
            (allocated.top() * scale).ceil() / scale,
        );
        let max = egui::pos2(
            (allocated.right() * scale).floor() / scale,
            (allocated.bottom() * scale).floor() / scale,
        );
        let rect = Rect::from_min_max(min, max.max(min));
        if !self.initialized || response.clicked() {
            response.request_focus();
            self.initialized = true;
            ui.ctx().request_repaint();
        }
        let focused = response.has_focus() && ui.input(|input| input.focused);
        if ui.memory(|memory| memory.has_focus(response.id)) {
            ui.memory_mut(|memory| {
                memory.set_focus_lock_filter(
                    response.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }
        if matches!(
            session.state(),
            SessionState::Starting | SessionState::Running { .. }
        ) {
            session.resize(metrics.size(rect.size())?)?;
        }
        let mut input_error = None;
        if focused && matches!(session.state(), SessionState::Running { .. }) {
            let (events, modifiers) = ui.input(|input| (input.events.clone(), input.modifiers));
            for bytes in self
                .input
                .encode(&events, modifiers, *session.terminal().mode())
            {
                if let Err(error) = session.send_input(&bytes) {
                    input_error = Some(error);
                    break;
                }
            }
        } else {
            self.input.preedit.clear();
        }
        let painter = ui.painter_at(rect);
        let cursor = paint(&painter, session.terminal(), rect, metrics, &font, focused);
        if focused {
            ui.output_mut(|output| {
                output.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Normal,
                    rect,
                    cursor_rect: cursor,
                    should_interrupt_composition: false,
                })
            });
            if !self.input.preedit.is_empty() {
                let galley =
                    painter.layout_no_wrap(self.input.preedit.clone(), font, Color32::WHITE);
                let pos = egui::pos2(
                    cursor
                        .left()
                        .min((rect.right() - galley.size().x).max(rect.left())),
                    cursor
                        .top()
                        .min((rect.bottom() - galley.size().y).max(rect.top())),
                );
                painter.rect_filled(Rect::from_min_size(pos, galley.size()), 0.0, Color32::BLACK);
                painter.galley(pos, galley, Color32::WHITE);
            }
        }
        if let Some(error) = input_error {
            return Err(error);
        }
        Ok(())
    }
}

fn resolve(term: &Term<TerminalEvents>, color: Color) -> Color32 {
    let rgb = match color {
        Color::Spec(rgb) => Some(rgb),
        Color::Indexed(index) => {
            term.colors()[usize::from(index)].or_else(|| default_color(usize::from(index)))
        }
        Color::Named(color) => {
            term.colors()[color as usize].or_else(|| default_color(color as usize))
        }
    };
    let Some(rgb) = rgb else {
        unreachable!("all indexed and named ANSI colors have palette defaults");
    };
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

fn cell_colors(term: &Term<TerminalEvents>, cell: &Cell) -> (Color32, Color32) {
    let mut foreground = resolve(term, cell.fg);
    if cell.flags.contains(Flags::DIM) {
        foreground = Color32::from_rgb(
            (f32::from(foreground.r()) * 0.66) as u8,
            (f32::from(foreground.g()) * 0.66) as u8,
            (f32::from(foreground.b()) * 0.66) as u8,
        );
    }
    let mut background = resolve(term, cell.bg);
    if cell.flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut foreground, &mut background);
    }
    (foreground, background)
}

fn cell_text(cell: &Cell) -> String {
    let mut text = cell.c.to_string();
    if let Some(combining) = cell.zerowidth() {
        text.extend(combining);
    }
    text
}

fn paint(
    painter: &egui::Painter,
    term: &Term<TerminalEvents>,
    rect: Rect,
    metrics: Metrics,
    font: &FontId,
    focused: bool,
) -> Rect {
    let content = term.renderable_content();
    let origin = egui::pos2(
        (rect.left() * metrics.pixels_per_point).ceil() / metrics.pixels_per_point,
        (rect.top() * metrics.pixels_per_point).ceil() / metrics.pixels_per_point,
    );
    painter.rect_filled(
        rect,
        0.0,
        resolve(term, Color::Named(NamedColor::Background)),
    );
    let cursor = metrics.rect(
        origin,
        content.cursor.point.column.0,
        content.cursor.point.line.0 + content.display_offset as i32,
        false,
    );
    let cursor_color = resolve(term, Color::Named(NamedColor::Cursor));
    let mut run: Option<(Rect, Color32)> = None;
    for cell in term.grid().display_iter() {
        let cell_rect = metrics.rect(
            origin,
            cell.point.column.0,
            cell.point.line.0 + content.display_offset as i32,
            false,
        );
        let (_, background) = cell_colors(term, cell.cell);
        if let Some((run_rect, color)) = &mut run
            && *color == background
            && run_rect.top() == cell_rect.top()
        {
            run_rect.max.x = cell_rect.right();
        } else {
            if let Some((rect, color)) = run.take() {
                painter.rect_filled(rect, 0.0, color);
            }
            run = Some((cell_rect, background));
        }
    }
    if let Some((rect, color)) = run {
        painter.rect_filled(rect, 0.0, color);
    }
    for cell in content.display_iter {
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let cell_rect = metrics.rect(
            origin,
            cell.point.column.0,
            cell.point.line.0 + content.display_offset as i32,
            cell.flags.contains(Flags::WIDE_CHAR),
        );
        if !cell_rect.intersects(rect) {
            continue;
        }
        let cell_painter = painter.with_clip_rect(cell_rect.intersect(rect));
        let (mut foreground, background) = cell_colors(term, cell.cell);
        let is_cursor =
            cell.point == content.cursor.point && content.cursor.shape != CursorShape::Hidden;
        if is_cursor && focused && content.cursor.shape == CursorShape::Block {
            cell_painter.rect_filled(cell_rect, 0.0, cursor_color);
            if background != cursor_color {
                foreground = background;
            }
        }
        if !cell.flags.contains(Flags::HIDDEN) {
            let format = egui::TextFormat {
                font_id: font.clone(),
                color: foreground,
                italics: cell.flags.contains(Flags::ITALIC),
                ..Default::default()
            };
            let galley = cell_painter.layout_job(egui::text::LayoutJob::single_section(
                cell_text(cell.cell),
                format,
            ));
            let pos = cell_rect.min + egui::vec2(0.0, (metrics.cell.y - galley.size().y) / 2.0);
            cell_painter.galley(pos, galley.clone(), foreground);
            if cell.flags.contains(Flags::BOLD) {
                cell_painter.galley(
                    pos + egui::vec2(1.0 / metrics.pixels_per_point, 0.0),
                    galley,
                    foreground,
                );
            }
            let stroke = Stroke::new(1.0 / metrics.pixels_per_point, foreground);
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                let color = cell
                    .underline_color()
                    .map_or(foreground, |color| resolve(term, color));
                let y = cell_rect.bottom() - 2.0 / metrics.pixels_per_point;
                cell_painter.line_segment(
                    [
                        egui::pos2(cell_rect.left(), y),
                        egui::pos2(cell_rect.right(), y),
                    ],
                    Stroke { color, ..stroke },
                );
            }
            if cell.flags.contains(Flags::STRIKEOUT) {
                cell_painter
                    .line_segment([cell_rect.left_center(), cell_rect.right_center()], stroke);
            }
        }
        if is_cursor {
            let stroke = Stroke::new(1.0 / metrics.pixels_per_point, cursor_color);
            if !focused || content.cursor.shape == CursorShape::HollowBlock {
                cell_painter.rect_stroke(cell_rect, 0.0, stroke, egui::StrokeKind::Inside);
            } else {
                match content.cursor.shape {
                    CursorShape::Beam => {
                        cell_painter.line_segment(
                            [
                                cell_rect.left_top() + egui::vec2(stroke.width / 2.0, 0.0),
                                cell_rect.left_bottom() + egui::vec2(stroke.width / 2.0, 0.0),
                            ],
                            stroke,
                        );
                    }
                    CursorShape::Underline => {
                        cell_painter.line_segment(
                            [
                                cell_rect.left_bottom() - egui::vec2(0.0, stroke.width),
                                cell_rect.right_bottom() - egui::vec2(0.0, stroke.width),
                            ],
                            stroke,
                        );
                    }
                    _ => {}
                }
            }
        }
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    fn terminal(bytes: &[u8]) -> Term<TerminalEvents> {
        let mut term = Term::new(
            Config::default(),
            &GridSize::new(10, 3).unwrap(),
            TerminalEvents::default(),
        );
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        term
    }

    fn drawing(term: &Term<TerminalEvents>, scale: f32, focused: bool) -> (egui::FullOutput, Rect) {
        let context = egui::Context::default();
        context.set_pixels_per_point(scale);
        let metrics = Metrics::new(egui::vec2(8.0, 16.0), scale);
        let rect = Rect::from_min_size(egui::pos2(16.0, 32.0), egui::vec2(80.0, 48.0));
        let mut cursor = Rect::NOTHING;
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            cursor = paint(
                &ui.painter_at(rect),
                term,
                rect,
                metrics,
                &FontId::monospace(14.0),
                focused,
            );
        });
        output.textures_delta.clear();
        (output, cursor)
    }

    #[test]
    fn grid_sizing_is_scale_consistent_and_never_zero() {
        for scale in [1.0, 1.25, 2.0] {
            let metrics = Metrics::new(egui::vec2(8.0, 16.0), scale);
            let size = metrics.size(egui::vec2(640.0, 384.0)).unwrap();
            assert_eq!(size.grid, GridSize::default());
            assert_eq!(size.cell_width, (8.0 * scale) as u16);
            assert_eq!(size.cell_height, (16.0 * scale) as u16);
            assert_eq!(
                metrics.size(Vec2::ZERO).unwrap().grid,
                GridSize::new(2, 1).unwrap()
            );
            let large = metrics.size(egui::vec2(1e9, 1e9)).unwrap();
            assert!(large.grid.columns().checked_mul(large.cell_width).is_some());
            assert!(large.grid.rows().checked_mul(large.cell_height).is_some());
        }
    }

    #[test]
    fn cells_attributes_wide_spacers_combining_text_and_cursor_are_painted() {
        let term =
            terminal("A\x1b[31;44;1;4mB\x1b[0m界e\u{301}\x1b[7mI\x1b[0m\x1b[2;3H".as_bytes());
        for scale in [1.0, 1.25, 2.0] {
            let (output, cursor) = drawing(&term, scale, true);
            assert_eq!(
                cursor,
                Rect::from_min_size(egui::pos2(32.0, 48.0), egui::vec2(8.0, 16.0))
            );
            let texts: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| {
                    if let egui::epaint::Shape::Text(text) = &shape.shape {
                        Some((shape.clip_rect, text))
                    } else {
                        None
                    }
                })
                .collect();
            let (_, wide) = texts
                .iter()
                .find(|(_, text)| text.galley.text() == "界")
                .unwrap();
            assert_eq!(wide.pos.x, 32.0);
            let (clip, combining) = texts
                .iter()
                .find(|(_, text)| text.galley.text() == "e\u{301}")
                .unwrap();
            assert_eq!(
                combining.pos.x, 48.0,
                "wide spacer must not consume an extra glyph position"
            );
            assert_eq!(clip.width(), 8.0);
            assert_eq!(
                texts
                    .iter()
                    .filter(|(_, text)| text.galley.text() == "B")
                    .count(),
                2,
                "bold uses a second glyph draw"
            );
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::epaint::Shape::Rect(rect) if rect.fill == Color32::from_rgb(0, 0, 238)
                    && rect.rect.width() == 8.0)));
            assert!(
                output
                    .shapes
                    .iter()
                    .any(|shape| matches!(&shape.shape, egui::epaint::Shape::LineSegment { .. }))
            );
        }
        let inverse = &term.grid()[alacritty_terminal::index::Point::new(
            alacritty_terminal::index::Line(0),
            alacritty_terminal::index::Column(5),
        )];
        assert_eq!(
            cell_colors(&term, inverse),
            (Color32::BLACK, Color32::from_rgb(229, 229, 229))
        );
    }

    #[test]
    fn alternate_screen_is_drawn_and_main_screen_returns() {
        let mut term = terminal(b"MAIN");
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"\x1b[?1049h\x1b[HALT");
        let (output, _) = drawing(&term, 1.0, true);
        let text: String = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert!(text.starts_with("ALT"));
        assert!(!text.contains("MAIN"));
        parser.advance(&mut term, b"\x1b[?1049l");
        let (output, _) = drawing(&term, 1.0, true);
        let text: String = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert!(text.starts_with("MAIN"));
    }

    #[test]
    fn cursor_visibility_shapes_and_focus_are_respected() {
        for (sequence, filled) in [
            (&b"\x1b[?25l"[..], false),
            (&b"\x1b[2 q"[..], true),
            (&b"\x1b[4 q"[..], false),
            (&b"\x1b[6 q"[..], false),
        ] {
            let term = terminal(sequence);
            let (output, cursor) = drawing(&term, 1.0, true);
            assert_eq!(
                output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::epaint::Shape::Rect(rect) if rect.rect == cursor
                    && rect.fill == Color32::from_rgb(229, 229, 229))),
                filled
            );
        }
        let (output, cursor) = drawing(&terminal(b""), 1.0, false);
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::epaint::Shape::Rect(rect) if rect.rect == cursor && !rect.stroke.is_empty()
                && rect.fill == Color32::TRANSPARENT)));
    }

    #[test]
    fn every_ansi_palette_entry_and_dynamic_color_has_a_rendering() {
        let term = terminal(b"\x1b]4;1;#123456\x07");
        assert_eq!(
            resolve(&term, Color::Indexed(1)),
            Color32::from_rgb(18, 52, 86)
        );
        for index in 0..=268 {
            assert!(default_color(index).is_some());
        }
    }
}
