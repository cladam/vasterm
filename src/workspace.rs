use std::sync::Arc;

use eframe::egui;

use crate::session::{GridSize, Session, SessionState, local_shell_command};
use crate::surface::TerminalSurface;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub usize);

impl std::fmt::Display for PaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0 + 1)
    }
}

pub struct Pane {
    pub id: PaneId,
    pub session: Option<Session>,
    pub startup_error: Option<String>,
    pub(crate) surface: TerminalSurface,
    pub interaction_error: Option<String>,
}

impl Pane {
    pub fn new(id: PaneId, context: &egui::Context) -> Self {
        let repaint_context = context.clone();
        let result = local_shell_command().and_then(|command| {
            Session::spawn(
                command,
                GridSize::default(),
                Arc::new(move || repaint_context.request_repaint()),
            )
        });
        match result {
            Ok(session) => Self {
                id,
                session: Some(session),
                startup_error: None,
                surface: TerminalSurface::default(),
                interaction_error: None,
            },
            Err(error) => Self {
                id,
                session: None,
                startup_error: Some(format!("{error:#}")),
                surface: TerminalSurface::default(),
                interaction_error: None,
            },
        }
    }

    pub fn with_session(id: PaneId, session: Session) -> Self {
        Self {
            id,
            session: Some(session),
            startup_error: None,
            surface: TerminalSurface::default(),
            interaction_error: None,
        }
    }

    pub fn with_error(id: PaneId, error: String) -> Self {
        Self {
            id,
            session: None,
            startup_error: Some(error),
            surface: TerminalSurface::default(),
            interaction_error: None,
        }
    }

    pub fn surface_id(&self, ui: &egui::Ui) -> egui::Id {
        ui.make_persistent_id(("terminal-surface", self.id.0))
    }

    pub fn poll(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.poll();
        }
    }

    pub fn request_shutdown(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.request_shutdown();
        }
    }

    pub fn cleanup_complete(&self) -> bool {
        self.session.as_ref().is_none_or(|s| s.cleanup_complete())
    }

    pub fn cleanup_failed(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.cleanup_failed())
    }

    pub fn show(&mut self, ui: &mut egui::Ui, default_focus: bool) {
        if let Some(error) = &self.startup_error {
            ui.colored_label(egui::Color32::LIGHT_RED, format!("Session failed: {error}"));
        }
        if let Some(session) = self.session.as_mut() {
            let color = if matches!(session.state(), SessionState::Failed(_)) {
                egui::Color32::LIGHT_RED
            } else {
                ui.visuals().text_color()
            };
            ui.horizontal(|ui| {
                ui.colored_label(color, session.state().to_string());
                ui.label(format!(
                    "{} x {} | {:.2} px/point",
                    session.size().columns(),
                    session.size().rows(),
                    ui.ctx().pixels_per_point()
                ));
                if matches!(
                    session.state(),
                    SessionState::Starting | SessionState::Running { .. } | SessionState::Draining
                ) && ui.button("Stop session").clicked()
                {
                    session.request_shutdown();
                }
                if session.cleanup_failed() && ui.button("Retry cleanup").clicked() {
                    session.request_shutdown();
                    self.interaction_error = None;
                }
            });
        }
        if let Some(error) = &self.interaction_error {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
            if ui.button("Dismiss error").clicked() {
                self.interaction_error = None;
            }
        }
        ui.separator();
        if let Some(session) = self.session.as_mut()
            && let Err(error) = self.surface.show(ui, session, self.id.0, default_focus)
        {
            self.interaction_error = Some(format!("Terminal interaction failed: {error:#}"));
            ui.ctx().request_repaint();
        }
    }
}

pub struct Workspace {
    pub panes: Vec<Pane>,
}

impl Workspace {
    pub fn two_panes(context: &egui::Context) -> Self {
        Self {
            panes: vec![Pane::new(PaneId(0), context), Pane::new(PaneId(1), context)],
        }
    }

    pub fn from_panes(panes: Vec<Pane>) -> Self {
        Self { panes }
    }

    pub fn poll(&mut self) {
        for pane in &mut self.panes {
            pane.poll();
        }
    }

    pub fn all_cleanups_complete(&self) -> bool {
        self.panes.iter().all(|pane| pane.cleanup_complete())
    }

    pub fn any_cleanup_failed(&self) -> Option<String> {
        for pane in &self.panes {
            if let Some(session) = pane.session.as_ref()
                && (session.cleanup_failed() || session.cleanup_complete())
                && let SessionState::Failed(error) = session.state()
            {
                return Some(if self.panes.len() > 1 {
                    format!("Pane {}: {error}", pane.id)
                } else {
                    error.clone()
                });
            }
        }
        None
    }

    pub fn request_shutdown_all(&mut self) {
        for pane in &mut self.panes {
            if !pane.cleanup_complete() {
                pane.request_shutdown();
            }
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let count = self.panes.len();
        if count == 0 {
            ui.label("No terminal panes.");
            return;
        }

        let full_rect = ui.available_rect_before_wrap();
        if count == 1 {
            let surface_id = self.panes[0].surface_id(ui);
            let is_focused = ui.memory(|m| m.has_focus(surface_id)) && ui.input(|i| i.focused);
            let stroke = if is_focused {
                egui::Stroke::new(1.5, ui.visuals().selection.bg_fill)
            } else {
                ui.visuals().widgets.noninteractive.bg_stroke
            };
            ui.painter()
                .rect_stroke(full_rect, 2.0, stroke, egui::StrokeKind::Inside);
            ui.scope_builder(
                egui::UiBuilder::new().max_rect(full_rect.shrink(3.0)),
                |ui| {
                    self.panes[0].show(ui, true);
                },
            );
            ui.allocate_rect(full_rect, egui::Sense::hover());
            return;
        }

        let spacing = 6.0;
        let total_spacing = spacing * (count - 1) as f32;
        let pane_width = ((full_rect.width() - total_spacing) / count as f32)
            .floor()
            .max(10.0);

        ui.allocate_rect(full_rect, egui::Sense::hover());

        for i in 0..count {
            let left = full_rect.min.x + i as f32 * (pane_width + spacing);
            let rect = egui::Rect::from_min_size(
                egui::pos2(left, full_rect.min.y),
                egui::vec2(pane_width, full_rect.height()),
            );

            let surface_id = self.panes[i].surface_id(ui);
            let is_focused = ui.memory(|m| m.has_focus(surface_id)) && ui.input(|i| i.focused);
            let stroke = if is_focused {
                egui::Stroke::new(1.5, ui.visuals().selection.bg_fill)
            } else {
                ui.visuals().widgets.noninteractive.bg_stroke
            };
            ui.painter()
                .rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);

            ui.scope_builder(egui::UiBuilder::new().max_rect(rect.shrink(3.0)), |ui| {
                self.panes[i].show(ui, i == 0);
            });
        }
    }
}
