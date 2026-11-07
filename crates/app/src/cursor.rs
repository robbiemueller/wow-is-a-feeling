//! The gauntlet pointer. Decoded once per scale, and the same `Arc` is handed over every
//! frame, which is what stops egui-winit from uploading it again.

use eframe::egui::{self, CursorIcon, CustomCursorImage};
use std::sync::Arc;

const SIZE: u16 = 24;
const HOTSPOT: u16 = 1;

#[derive(Default)]
pub struct Cursors {
    normal: Option<CustomCursorImage>,
    doubled: Option<CustomCursorImage>,
}

impl Cursors {
    /// Call at the end of a frame: the gauntlet for the default and hand icons, and
    /// otherwise the system's, so text and resize cursors still work.
    pub fn apply(&mut self, ctx: &egui::Context) {
        let icon = ctx.output(|o| o.cursor_icon);
        if matches!(icon, CursorIcon::Default | CursorIcon::PointingHand) {
            let image = if ctx.pixels_per_point() >= 1.5 {
                self.doubled.get_or_insert_with(|| scaled(2))
            } else {
                self.normal.get_or_insert_with(|| scaled(1))
            };
            ctx.set_cursor_image(Some(image.clone()));
        } else {
            ctx.set_cursor_image(None);
        }
    }
}

fn scaled(factor: u16) -> CustomCursorImage {
    let source = image::load_from_memory_with_format(
        include_bytes!("../../../assets/skin/cursor.png"),
        image::ImageFormat::Png,
    )
    .expect("the embedded cursor is a valid PNG")
    .into_rgba8();
    let bytes = nearest(source.as_raw(), usize::from(SIZE), usize::from(factor));
    CustomCursorImage {
        rgba: Arc::from(bytes),
        size: [SIZE * factor, SIZE * factor],
        hotspot: [HOTSPOT * factor, HOTSPOT * factor],
    }
}

/// Blow up square RGBA art without smoothing, so the pixels stay crisp.
fn nearest(rgba: &[u8], side: usize, factor: usize) -> Vec<u8> {
    let out_side = side * factor;
    let mut out = Vec::with_capacity(out_side * out_side * 4);
    for y in 0..out_side {
        for x in 0..out_side {
            let at = (y / factor * side + x / factor) * 4;
            out.extend_from_slice(&rgba[at..at + 4]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_art_is_the_size_the_code_says() {
        let cursor = scaled(1);
        assert_eq!(cursor.rgba.len(), 24 * 24 * 4);
        assert_eq!(cursor.size, [24, 24]);
        assert_eq!(cursor.hotspot, [1, 1]);
    }

    #[test]
    fn doubling_keeps_pixels_square_and_moves_the_hot_spot() {
        let cursor = scaled(2);
        assert_eq!(cursor.rgba.len(), 48 * 48 * 4);
        assert_eq!(cursor.hotspot, [2, 2]);
        let one = nearest(&[1, 2, 3, 4], 1, 2);
        assert_eq!(one, [1, 2, 3, 4].repeat(4));
    }

    #[test]
    fn scaling_reads_each_source_pixel_in_place() {
        let two_by_two: Vec<u8> = (0..16).collect();
        let doubled = nearest(&two_by_two, 2, 2);
        assert_eq!(&doubled[0..4], &two_by_two[0..4]);
        assert_eq!(&doubled[4..8], &two_by_two[0..4]);
        assert_eq!(&doubled[8..12], &two_by_two[4..8]);
        assert_eq!(&doubled[60..64], &two_by_two[12..16]);
    }
}
