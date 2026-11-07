//! The wiring for `easter.rs`: dances, dings, the cleaning gag, the platypus and the block
//! letters. All of it sits still until someone goes looking.

use crate::easter::{self, MonitorGag};
use crate::skin::Skin;
use crate::theme;
use eframe::egui::{
    self, Color32, CursorIcon, Id, LayerId, Order, Rect, Response, Sense, Ui, pos2, vec2,
};
use std::time::{Duration, Instant};

const DANCE_FOR: Duration = Duration::from_secs(3);
const DING_FOR: Duration = Duration::from_secs(1);
const PWNAGE_FOR: Duration = Duration::from_secs(3);

const PLATYPUS_NOTE: &str = "A pink plush platypus. It has seen things.";

/// Original block letters, 5 wide and 7 tall.
const LETTERS: [[&str; 7]; 6] = [
    [
        "####.", "#...#", "#...#", "####.", "#....", "#....", "#....",
    ],
    [
        "#...#", "#...#", "#...#", "#.#.#", "#.#.#", "##.##", "#...#",
    ],
    [
        "#...#", "##..#", "#.#.#", "#..##", "#...#", "#...#", "#...#",
    ],
    [
        ".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#",
    ],
    [
        ".###.", "#...#", "#....", "#.###", "#...#", "#...#", ".###.",
    ],
    [
        "#####", "#....", "#....", "####.", "#....", "#....", "#####",
    ],
];

pub struct Fun {
    pub gag: MonitorGag,
    dance_until: Option<Instant>,
    ding_until: Option<Instant>,
    pwnage_until: Option<Instant>,
    mom_at: Instant,
    mom_called: bool,
    pub platypus: bool,
    /// The monitor stays dark and the banners stay up, for screenshots.
    frozen: bool,
}

impl Fun {
    pub fn new(launches: u64, now: Instant) -> Self {
        Self {
            gag: MonitorGag::default(),
            dance_until: None,
            ding_until: None,
            pwnage_until: None,
            mom_at: now + easter::MOM_CALLS_AFTER,
            mom_called: false,
            platypus: easter::platypus_visits(launches),
            frozen: false,
        }
    }

    pub fn dance(&mut self, now: Instant) {
        self.dance_until = Some(now + DANCE_FOR);
    }

    pub fn ding(&mut self, now: Instant) {
        self.ding_until = Some(now + DING_FOR);
    }

    pub fn pwnage(&mut self, now: Instant) {
        self.pwnage_until = Some(now + PWNAGE_FOR);
    }

    pub fn is_dancing(&self, now: Instant) -> bool {
        self.dance_until.is_some_and(|until| now < until)
    }

    pub fn pwnage_showing(&self, now: Instant) -> bool {
        self.pwnage_until.is_some_and(|until| now < until)
    }

    /// Hold every timed effect for a screenshot, at a moment that looks right.
    pub fn freeze_for_screenshot(&mut self, now: Instant) {
        self.frozen = true;
        if self.ding_until.is_some() {
            self.ding_until = Some(now + DING_FOR);
        }
    }

    /// True once, when it's time for the phone to ring.
    pub fn mom_calling(&mut self, now: Instant) -> bool {
        if !self.mom_called && now >= self.mom_at {
            self.mom_called = true;
            return true;
        }
        false
    }

    /// When the next timed effect needs a frame. `ZERO` while something's in motion.
    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        let mut wake = (!self.mom_called).then(|| self.mom_at.saturating_duration_since(now));
        let mut sooner = |d: Duration| {
            wake = Some(wake.map_or(d, |w: Duration| w.min(d)));
        };
        if let Some(d) = self.gag.wake_in(now) {
            sooner(d);
        }
        if let Some(until) = self.pwnage_until
            && now < until
        {
            sooner(until - now);
        }
        if !self.frozen
            && (self.is_dancing(now) || self.ding_until.is_some_and(|until| now < until))
        {
            sooner(Duration::ZERO);
        }
        wake
    }

    /// How far the portrait has bobbed, in points.
    pub fn bob(&self, now: Instant) -> f32 {
        match self.dance_until {
            Some(until) if now < until => {
                let t = (DANCE_FOR - (until - now)).as_secs_f32();
                (t * std::f32::consts::TAU * 2.0).sin().abs() * -8.0
            }
            _ => 0.0,
        }
    }

    /// The clickable portrait. Returns its response for the gag to count.
    pub fn portrait(
        &mut self,
        ui: &mut Ui,
        skin: &Skin,
        center: egui::Pos2,
        radius: f32,
        now: Instant,
    ) -> Response {
        let screen_off = self.gag.is_off(now);
        let center = center + vec2(0.0, self.bob(now));
        skin.portrait(ui.painter(), center, radius, screen_off);
        let rect = Rect::from_center_size(center, vec2(radius * 2.0, radius * 2.0));
        let response = ui
            .interact(rect, ui.scope_id().with("portrait"), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand);
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, "Portrait"));
        if self.platypus {
            self.draw_platypus(ui, center, radius);
        }
        response
    }

    fn draw_platypus(&self, ui: &mut Ui, center: egui::Pos2, radius: f32) {
        const SCALE: f32 = 2.0;
        let (w, h) = (
            easter::PLATYPUS[0].len() as f32 * SCALE,
            easter::PLATYPUS.len() as f32 * SCALE,
        );
        let origin = pos2(center.x + radius * 0.1, center.y - radius + 1.0);
        let painter = ui.painter();
        for (row, line) in easter::PLATYPUS.iter().enumerate() {
            for (col, pixel) in line.bytes().enumerate() {
                let color = match pixel {
                    b'p' => Color32::from_rgb(240, 140, 180),
                    b'k' => Color32::BLACK,
                    b'o' => Color32::from_rgb(232, 138, 32),
                    b'w' => Color32::WHITE,
                    _ => continue,
                };
                let at = origin + vec2(col as f32 * SCALE, row as f32 * SCALE);
                painter.rect_filled(
                    Rect::from_min_size(at, vec2(SCALE, SCALE)),
                    0.0,
                    color,
                );
            }
        }
        let rect = Rect::from_min_size(origin, vec2(w, h));
        ui.interact(rect, ui.scope_id().with("platypus"), Sense::hover())
            .on_hover_ui(|ui| {
                ui.set_max_width(220.0);
                crate::widgets::tooltip_line(ui, PLATYPUS_NOTE);
            });
    }

    /// The full-window effects, painted over everything.
    pub fn overlays(&self, ctx: &egui::Context, full: Rect, now: Instant) {
        let painter = ctx.layer_painter(LayerId::new(Order::Foreground, Id::unique("fun")));
        if let Some(until) = self.ding_until
            && now < until
        {
            let left = (until - now).as_secs_f32() / DING_FOR.as_secs_f32();
            painter.rect_filled(
                full,
                0.0,
                Color32::from_rgba_unmultiplied(255, 209, 0, (left * 120.0) as u8),
            );
        }
        if self.pwnage_showing(now) {
            paint_pwnage(&painter, full);
        }
    }
}

/// PWNAGE, drawn square by square so no font gets a say in it.
fn paint_pwnage(painter: &egui::Painter, full: Rect) {
    let columns = 6.0 * 5.0 + 5.0;
    let cell = (full.width() * 0.8 / columns).floor().clamp(4.0, 24.0);
    let word = vec2(columns * cell, 7.0 * cell);
    let top_left = pos2(full.center().x - word.x / 2.0, full.center().y - word.y / 2.0);
    let plate = Rect::from_min_size(top_left, word).expand(cell * 1.5);
    painter.rect_filled(plate, 4.0, Color32::from_black_alpha(225));
    painter.rect_stroke(
        plate,
        4.0,
        egui::Stroke::new(2.0, theme::GOLD_DIM),
        egui::StrokeKind::Inside,
    );
    let blocks = || {
        LETTERS.iter().enumerate().flat_map(move |(index, letter)| {
            letter.iter().enumerate().flat_map(move |(row, line)| {
                line.bytes().enumerate().filter_map(move |(col, block)| {
                    (block == b'#').then(|| {
                        let x = (index * 6 + col) as f32 * cell;
                        Rect::from_min_size(top_left + vec2(x, row as f32 * cell), vec2(cell, cell))
                    })
                })
            })
        })
    };
    // Shadows first, so neighboring blocks melt into one solid letter.
    let drop = (cell * 0.25).max(2.0);
    for rect in blocks() {
        painter.rect_filled(
            rect.translate(vec2(drop, drop)),
            0.0,
            Color32::from_rgb(120, 60, 0),
        );
    }
    for rect in blocks() {
        painter.rect_filled(rect, 0.0, theme::GOLD);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_does_nothing_until_asked() {
        let now = Instant::now();
        let fun = Fun::new(1, now);
        assert!(!fun.is_dancing(now));
        assert!(!fun.pwnage_showing(now));
        assert_eq!(fun.bob(now), 0.0);
        assert!(!fun.platypus);
    }

    #[test]
    fn idle_only_ever_waits_for_mom() {
        let now = Instant::now();
        let fun = Fun::new(1, now);
        assert_eq!(fun.next_wake(now), Some(easter::MOM_CALLS_AFTER));
    }

    #[test]
    fn mom_calls_once() {
        let now = Instant::now();
        let mut fun = Fun::new(1, now);
        assert!(!fun.mom_calling(now));
        let later = now + easter::MOM_CALLS_AFTER;
        assert!(fun.mom_calling(later));
        assert!(!fun.mom_calling(later));
        assert_eq!(fun.next_wake(later), None, "nothing left to wake up for");
    }

    #[test]
    fn a_dance_asks_for_frames_only_while_it_lasts() {
        let now = Instant::now();
        let mut fun = Fun::new(1, now);
        fun.dance(now);
        assert_eq!(fun.next_wake(now), Some(Duration::ZERO));
        assert!(fun.bob(now + Duration::from_millis(100)) < 0.0);
        let after = now + DANCE_FOR;
        assert!(!fun.is_dancing(after));
        assert_ne!(fun.next_wake(after), Some(Duration::ZERO));
    }

    #[test]
    fn pwnage_holds_without_asking_for_frames() {
        let now = Instant::now();
        let mut fun = Fun::new(1, now);
        fun.pwnage(now);
        assert!(fun.pwnage_showing(now));
        assert_eq!(fun.next_wake(now), Some(PWNAGE_FOR));
        assert!(!fun.pwnage_showing(now + PWNAGE_FOR));
    }

    #[test]
    fn the_platypus_keeps_its_schedule() {
        assert!(Fun::new(13, Instant::now()).platypus);
        assert!(!Fun::new(14, Instant::now()).platypus);
    }

    #[test]
    fn block_letters_are_all_the_same_size() {
        for letter in LETTERS {
            assert!(letter.iter().all(|row| row.len() == 5));
        }
    }
}
