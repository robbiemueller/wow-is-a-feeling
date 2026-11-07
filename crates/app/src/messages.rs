//! Red and yellow lines at top center. They hold, then fade, and while one's alive the
//! app only wakes up for the fade.

use crate::skin::shadow_text;
use crate::theme;
use eframe::egui::{
    self, Align2, Color32, CursorIcon, Id, LayerId, Order, Rect, Sense, Stroke, pos2, vec2,
};
use std::time::{Duration, Instant};

const HOLD: Duration = Duration::from_secs(5);
const FADE: Duration = Duration::from_secs(1);
const MAX_SHOWN: usize = 3;
/// About 30 frames a second is plenty for a fade.
const FADE_STEP: Duration = Duration::from_millis(33);
const LINE_HEIGHT: f32 = 24.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Error,
    System,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub kind: Kind,
    pub text: String,
    /// A clickable label and where it goes.
    pub link: Option<(String, String)>,
    born: Instant,
}

#[derive(Default)]
pub struct Messages {
    list: Vec<Message>,
}

impl Messages {
    pub fn push(&mut self, kind: Kind, text: impl Into<String>, now: Instant) {
        self.add(kind, text.into(), None, now);
    }

    pub fn push_link(
        &mut self,
        kind: Kind,
        text: impl Into<String>,
        link_text: &str,
        url: &str,
        now: Instant,
    ) {
        self.add(
            kind,
            text.into(),
            Some((link_text.to_owned(), url.to_owned())),
            now,
        );
    }

    fn add(&mut self, kind: Kind, text: String, link: Option<(String, String)>, now: Instant) {
        // The same line again just starts its clock over.
        if let Some(old) = self
            .list
            .iter()
            .position(|m| m.kind == kind && m.text == text)
        {
            self.list.remove(old);
        }
        self.list.push(Message {
            kind,
            text,
            link,
            born: now,
        });
        if self.list.len() > MAX_SHOWN {
            let extra = self.list.len() - MAX_SHOWN;
            self.list.drain(..extra);
        }
    }

    /// Forget anything that has finished fading.
    pub fn tick(&mut self, now: Instant) {
        self.list
            .retain(|m| now.saturating_duration_since(m.born) < HOLD + FADE);
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    #[cfg(test)]
    pub fn texts(&self) -> Vec<&str> {
        self.list.iter().map(|m| m.text.as_str()).collect()
    }

    /// When this needs another frame. Nothing while there's nothing to show.
    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        self.list
            .iter()
            .map(|m| {
                let age = now.saturating_duration_since(m.born);
                if age < HOLD {
                    HOLD - age
                } else {
                    FADE_STEP
                }
            })
            .min()
    }

    /// Paint over everything else and hand back where a link was clicked.
    pub fn show(&self, ctx: &egui::Context, full: Rect, now: Instant) {
        if self.list.is_empty() {
            return;
        }
        let layer = LayerId::new(Order::Foreground, Id::unique("messages"));
        let painter = ctx.layer_painter(layer);
        let font = theme::strong(16.0);
        let mut y = full.min.y + 100.0;
        for (i, message) in self.list.iter().enumerate() {
            let alpha = alpha_at(now.saturating_duration_since(message.born));
            let base = match message.kind {
                Kind::Error => theme::ERROR_RED,
                Kind::System => theme::SYSTEM_YELLOW,
            };
            let color = base.gamma_multiply(alpha);
            let text = message
                .link
                .as_ref()
                .map_or(message.text.clone(), |(label, _)| {
                    format!("{} {label}", message.text)
                });
            let width_limit = (full.width() - 60.0).max(100.0);
            let galley = painter.layout(text, font.clone(), color, width_limit);
            let size = galley.size();
            let rect =
                Rect::from_center_size(pos2(full.center().x, y + size.y / 2.0), size + vec2(24.0, 6.0));
            // A band of dark behind the text, because half of it sits on the parchment.
            painter.rect_filled(rect, 3.0, Color32::from_black_alpha((190.0 * alpha) as u8));
            let at = pos2(rect.min.x + 12.0, rect.min.y + 3.0);
            painter.galley(
                at + vec2(1.0, 1.0),
                painter.layout(
                    galley.job.text.clone(),
                    font.clone(),
                    theme::SHADOW.gamma_multiply(alpha),
                    width_limit,
                ),
                theme::SHADOW,
            );
            painter.galley(at, galley, color);

            if let Some((_, url)) = &message.link {
                self.link_area(ctx, rect, i, url);
            }
            y = rect.max.y + (LINE_HEIGHT - 22.0);
        }
    }

    fn link_area(&self, ctx: &egui::Context, rect: Rect, index: usize, url: &str) {
        egui::Area::new(Id::unique(("message-link", index)))
            .order(Order::Foreground)
            .fixed_pos(rect.min)
            .show(ctx, |ui| {
                let (_, response) = ui.allocate_exact_size(rect.size(), Sense::click());
                let response = response.on_hover_cursor(CursorIcon::PointingHand);
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::Link, true, "Watch the video")
                });
                if response.hovered() {
                    ui.painter().rect_stroke(
                        rect,
                        3.0,
                        Stroke::new(1.0, theme::GOLD_DIM),
                        egui::StrokeKind::Inside,
                    );
                }
                if response.clicked() {
                    ctx.open_url(egui::OpenUrl::new_tab(url));
                }
            });
    }
}

/// Fully there for the hold, then down to nothing.
fn alpha_at(age: Duration) -> f32 {
    if age < HOLD {
        1.0
    } else {
        1.0 - ((age - HOLD).as_secs_f32() / FADE.as_secs_f32()).min(1.0)
    }
}

/// A single line with a shadow, for the casting strip.
pub fn strip_line(painter: &egui::Painter, center: egui::Pos2, text: &str, color: Color32) {
    shadow_text(
        painter,
        center,
        Align2::CENTER_CENTER,
        text,
        theme::strong(14.5),
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_most_three_and_the_oldest_goes() {
        let now = Instant::now();
        let mut m = Messages::default();
        for text in ["a", "b", "c", "d"] {
            m.push(Kind::System, text, now);
        }
        assert_eq!(m.texts(), vec!["b", "c", "d"]);
    }

    #[test]
    fn repeating_a_line_does_not_stack_it() {
        let now = Instant::now();
        let mut m = Messages::default();
        m.push(Kind::System, "a", now);
        m.push(Kind::System, "b", now);
        m.push(Kind::System, "a", now);
        assert_eq!(m.texts(), vec!["b", "a"]);
        m.push(Kind::Error, "a", now);
        assert_eq!(m.texts(), vec!["b", "a", "a"], "another color is another line");
    }

    #[test]
    fn nothing_alive_means_no_wakeups() {
        let now = Instant::now();
        let mut m = Messages::default();
        assert_eq!(m.next_wake(now), None);
        m.push(Kind::System, "a", now);
        assert!(!m.is_empty());
        m.tick(now + HOLD + FADE);
        assert!(m.is_empty());
        assert_eq!(m.next_wake(now + HOLD + FADE), None);
    }

    #[test]
    fn it_sleeps_through_the_hold_and_only_steps_through_the_fade() {
        let now = Instant::now();
        let mut m = Messages::default();
        m.push(Kind::Error, "a", now);
        assert_eq!(m.next_wake(now), Some(HOLD));
        assert_eq!(
            m.next_wake(now + Duration::from_secs(2)),
            Some(Duration::from_secs(3))
        );
        assert_eq!(m.next_wake(now + HOLD), Some(FADE_STEP));
    }

    #[test]
    fn alpha_holds_then_falls() {
        assert_eq!(alpha_at(Duration::ZERO), 1.0);
        assert_eq!(alpha_at(HOLD), 1.0);
        assert!((alpha_at(HOLD + FADE / 2) - 0.5).abs() < 0.01);
        assert_eq!(alpha_at(HOLD + FADE), 0.0);
    }
}
