// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    carnelian::{
        color::Color,
        drawing::{path_for_rectangle, FontFace, Glyph},
        render::{BlendMode, Context as RenderContext, Fill, FillRule, Layer, Raster, Style},
        Coord, Point, Rect, Size,
    },
    euclid::default::Vector2D,
    fuchsia_trace as ftrace,
    term_model::{
        ansi::CursorStyle,
        term::{CursorKey, RenderableCellContent, RenderableCellsIter},
    },
};

const UNDERLINE_CURSOR_CHAR: char = '\u{10a3e2}';
const BEAM_CURSOR_CHAR: char = '\u{10a3e3}';
const BOX_CURSOR_CHAR: char = '\u{10a3e4}';

const MAXIMUM_THUMB_RATIO: f32 = 0.8;
const MINIMUM_THUMB_RATIO: f32 = 0.05;

const SCROLL_BAR_MOVEMENT_THRESHOLD: f32 = 1.0;

static FONT_DATA: &'static [u8] =
    include_bytes!("../../../../../../prebuilt/third_party/fonts/robotomono/RobotoMono-Regular.ttf");

fn make_color(term_color: &term_model::term::color::Rgb) -> Color {
    Color { r: term_color.r, g: term_color.g, b: term_color.b, a: 0xFF }
}

fn raster_for_rectangle(bounds: &Rect, render_context: &mut RenderContext) -> Raster {
    let mut raster_builder = render_context.raster_builder().expect("raster_builder");
    raster_builder.add(&path_for_rectangle(bounds, render_context), None);
    raster_builder.build()
}

pub struct GridView {
    font: FontFace,
    background_color: Color,
    pub frame: Rect,
    pub cell_size: Size,
}

impl Default for GridView {
    fn default() -> Self {
        GridView::new(&Color::new())
    }
}

impl GridView {
    pub fn new(background_color: &Color) -> GridView {
        GridView {
            frame: Rect::zero(),
            background_color: *background_color,
            font: FontFace::new(FONT_DATA).expect("unable to load font data"),
            cell_size: Size::zero(),
        }
    }

    pub fn render<'a, C>(
        &self,
        render_context: &mut RenderContext,
        cells: RenderableCellsIter<'a, C>,
    ) -> Vec<Layer> {
        let size = self.cell_size;

        let font = &self.font;

        let font_size = size.height * 0.9;
        let baseline = font_size * 0.9;
        let background_color = self.background_color;
        let (mut layers, maybe_bg_layers): (Vec<_>, Vec<_>) = cells
            .filter_map(|cell| {
                if let Some(character) = maybe_char_for_renderable_cell_content(cell.inner) {
                    let cell_position = Point::new(
                        size.width * cell.column.0 as f32,
                        size.height * cell.line.0 as f32,
                    );
                    let char_position = cell_position + Vector2D::new(0.0, baseline);
                    let glyph_index = font.face.glyph_index(character);
                    let glyph = Glyph::new(render_context, &self.font, font_size, glyph_index);
                    let cell_bounds = Rect::new(cell_position, size);
                    let fg_raster = if glyph_index.is_none() {
                        raster_for_rectangle(&cell_bounds, render_context)
                    } else {
                        let pos_vec = char_position.to_vector().to_i32();
                        glyph.raster.translate(pos_vec)
                    };
                    let cell_background_color = make_color(&cell.bg);
                    let bg_layer = if cell_background_color != background_color {
                        let bg_raster = raster_for_rectangle(&cell_bounds, render_context);
                        Some(Layer {
                            raster: bg_raster,
                            clip: None,
                            style: Style {
                                fill_rule: FillRule::NonZero,
                                fill: Fill::Solid(cell_background_color),
                                blend_mode: BlendMode::Over,
                            },
                        })
                    } else {
                        None
                    };

                    Some((
                        Layer {
                            raster: fg_raster,
                            clip: None,
                            style: Style {
                                fill_rule: FillRule::NonZero,
                                fill: Fill::Solid(make_color(&cell.fg)),
                                blend_mode: BlendMode::Over,
                            },
                        },
                        bg_layer,
                    ))
                } else {
                    None
                }
            })
            .unzip();
        let bg_layers: Vec<Layer> = maybe_bg_layers.into_iter().filter_map(|a| a).collect();
        layers.extend(bg_layers);
        layers
    }
}

// The term-model library gives us zero-width characters in our array of chars. However,
// we do not support this at thsi point so we just pull out the first char for rendering.
fn maybe_char_for_renderable_cell_content(content: RenderableCellContent) -> Option<char> {
    match content {
        RenderableCellContent::Cursor(cursor_key) => chars_for_cursor(cursor_key),
        RenderableCellContent::Chars(chars) => Some(chars[0]),
    }
}

fn chars_for_cursor(cursor: CursorKey) -> Option<char> {
    match cursor.style {
        CursorStyle::Block => Some(BOX_CURSOR_CHAR),
        CursorStyle::Underline => Some(UNDERLINE_CURSOR_CHAR),
        CursorStyle::Beam => Some(BEAM_CURSOR_CHAR),
        //TODO add support for HollowBlock style
        CursorStyle::HollowBlock => Some(UNDERLINE_CURSOR_CHAR),
        CursorStyle::Hidden => None,
    }
}

pub struct ScrollBar {
    pub frame: Rect,

    /// The content size of the scrollable area.
    pub content_height: Coord,

    /// The vertical distance that the content is offset from the bottom
    pub content_offset: Coord,

    /// The frame to draw the scroll bar thumb
    thumb_frame: Option<Rect>,

    /// Indicates whether we are tracking a scroll or not. This will
    /// eventually need to track the device_id when we handle multiple
    /// input events.
    last_pointer_tracking_location: Option<Point>,
}

impl Default for ScrollBar {
    fn default() -> Self {
        ScrollBar {
            frame: Rect::zero(),
            content_height: 0.0,
            content_offset: 0.0,
            thumb_frame: None,
            last_pointer_tracking_location: None,
        }
    }
}

impl ScrollBar {
    pub fn render(&self, render_context: &mut RenderContext) -> Option<Layer> {
        ftrace::duration!("terminal", "Views:ScrollBar:render2");
        self.thumb_frame
            .and_then(|thumb_frame| Some(Self::render_thumb_pattern(render_context, &thumb_frame)))
    }

    /// This method must be called after the client has updated
    /// the frame, content_height or content_offset. We leave this
    /// up to the caller to allow for the optimization of batching
    /// these updates without needing to recalculate the frame.
    pub fn invalidate_thumb_frame(&mut self) {
        self.update_thumb_frame();
    }

    pub fn begin_tracking_pointer_event(&mut self, point: Point) {
        if let Some(frame) = &self.thumb_frame {
            if !frame.contains(point) {
                // jump the middle of the thumb to the middle of the point
                let thumb_height = frame.size.height;
                let conversion_factor = self.pixel_space_to_content_space_conversion_factor();

                let proposed_offset = conversion_factor
                    * (self.frame.size.height
                        - (point.y - self.frame.origin.y)
                        - (thumb_height / 2.0));

                self.propose_offset(proposed_offset, conversion_factor, thumb_height);
            }
            self.last_pointer_tracking_location = Some(point);
        }
    }

    pub fn handle_pointer_move(&mut self, point: Point) {
        if let (Some(last_point), Some(thumb_frame)) =
            (self.last_pointer_tracking_location, self.thumb_frame)
        {
            let dy = last_point.y - point.y;
            // We do not want to respond to every micro pixel change. Only
            // move if we are above the threshold
            if dy.abs() < SCROLL_BAR_MOVEMENT_THRESHOLD {
                return;
            }

            let conversion_factor = self.pixel_space_to_content_space_conversion_factor();
            let proposed_offset = self.content_offset + (conversion_factor * dy);
            self.propose_offset(proposed_offset, conversion_factor, thumb_frame.size.height);

            self.last_pointer_tracking_location = Some(point);
        }
    }

    pub fn cancel_pointer_event(&mut self) {
        self.last_pointer_tracking_location = None;
    }

    fn propose_offset(
        &mut self,
        proposed_offset: Coord,
        conversion_factor: f32,
        thumb_height: f32,
    ) {
        // we have some rounding errors which make us loose 2 pixels. We round our inputs
        // to get those pixels back when calculating the max_offset.
        let max_offset =
            f32::ceil((self.frame.size.height - f32::floor(thumb_height)) * conversion_factor);
        self.content_offset = Coord::min(Coord::max(proposed_offset, 0.0), max_offset);
        self.invalidate_thumb_frame();
    }

    #[inline]
    pub fn is_tracking(&self) -> bool {
        self.last_pointer_tracking_location.is_some()
    }

    fn update_thumb_frame(&mut self) {
        if let Some(thumb_info) = self.calculate_thumb_render_info() {
            let thumb_frame = thumb_info.calculate_frame_in_rect(&self.frame);

            self.thumb_frame = Some(thumb_frame);
        } else {
            self.thumb_frame = None;
        }
    }

    fn render_thumb_pattern(render_context: &mut RenderContext, frame: &Rect) -> Layer {
        let white = Color::white();
        let raster = raster_for_rectangle(&frame, render_context);
        Layer {
            raster: raster,
            clip: None,
            style: Style {
                fill_rule: FillRule::NonZero,
                fill: Fill::Solid(white),
                blend_mode: BlendMode::Over,
            },
        }
    }

    fn calculate_thumb_render_info(&self) -> Option<ThumbRenderInfo> {
        if self.content_height <= self.frame.size.height {
            return None;
        }

        let height =
            Self::calculate_thumb_height_ratio(self.frame.size.height, self.content_height)
                * self.frame.size.height;

        let vertical_offset =
            Coord::floor(self.content_space_to_pixel_space_factor(&height) * self.content_offset);
        Some(ThumbRenderInfo { height, vertical_offset })
    }

    fn calculate_thumb_height_ratio(frame_height: Coord, content_height: Coord) -> Coord {
        let ratio = frame_height / content_height;
        Coord::min(Coord::max(MINIMUM_THUMB_RATIO, ratio), MAXIMUM_THUMB_RATIO)
    }

    #[inline]
    fn pixel_space_to_content_space_conversion_factor(&self) -> Coord {
        // this method is different from the thumb_height_ratio in that it will never round
        // so it can be used to calculate offsets from pixel positions.
        self.content_height / self.frame.size.height
    }

    #[inline]
    fn content_space_to_pixel_space_factor(&self, thumb_height: &Coord) -> Coord {
        (self.frame.size.height - thumb_height) / (self.content_height - self.frame.size.height)
    }
}

#[derive(PartialEq, Debug)]
struct ThumbRenderInfo {
    /// The height of the ScrollBarThumb
    height: Coord,

    /// The y position of the bottom of the ScrollBarThumb
    vertical_offset: Coord,
}

impl ThumbRenderInfo {
    fn calculate_frame_in_rect(&self, outer_rect: &Rect) -> Rect {
        let size = Size::new(outer_rect.size.width, self.height);
        let origin = Point::new(
            outer_rect.origin.x,
            outer_rect.origin.y + outer_rect.size.height - self.vertical_offset - self.height,
        );
        Rect::new(origin, size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect_with_height(height: Coord) -> Rect {
        Rect::new(Point::zero(), Size::new(10.0, height))
    }

    #[test]
    fn sroll_bar_is_tracking_flag() {
        let mut scroll_bar = ScrollBar::default();

        assert_eq!(scroll_bar.is_tracking(), false);

        scroll_bar.last_pointer_tracking_location = Some(Point::new(0.0, 0.0));
        assert_eq!(scroll_bar.is_tracking(), true);
    }

    #[test]
    fn scroll_bar_does_not_change_content_offset_if_not_tracking() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 400.0;

        scroll_bar.handle_pointer_move(Point::new(50.0, 50.0));
        assert_eq!(scroll_bar.content_offset, 0.0);
    }

    #[test]
    fn scroll_bar_does_not_change_content_offset_if_less_than_threshold() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 400.0;
        scroll_bar.invalidate_thumb_frame();

        scroll_bar.last_pointer_tracking_location = Some(Point::new(10.0, 90.0));

        scroll_bar.handle_pointer_move(Point::new(10.0, 89.9));
        assert_eq!(scroll_bar.content_offset, 0.0);
    }

    #[test]
    fn scroll_bar_updates_content_offset_on_move_when_tracking() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 400.0;
        scroll_bar.invalidate_thumb_frame();

        scroll_bar.last_pointer_tracking_location = Some(Point::new(10.0, 90.0));

        // a movement of 1 pixel in view space should equate to a movement
        // of 4 points in content space
        scroll_bar.handle_pointer_move(Point::new(10.0, 89.0));
        assert_eq!(scroll_bar.content_offset, 4.0);
    }

    #[test]
    fn scroll_bar_updates_content_offset_on_move_when_tracking_nonzero_origin() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = Rect::new(Point::new(10.0, 10.0), Size::new(10.0, 100.0));
        scroll_bar.content_height = 400.0;
        scroll_bar.invalidate_thumb_frame();

        scroll_bar.last_pointer_tracking_location = Some(Point::new(10.0, 90.0));

        // a movement of 1 pixel in view space should equate to a movement
        // of 4 points in content space
        scroll_bar.handle_pointer_move(Point::new(10.0, 89.0));
        assert_eq!(scroll_bar.content_offset, 4.0);
    }

    #[test]
    fn scroll_bar_updates_content_offset_on_move_when_tracking_stays_above_zero() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 400.0;
        scroll_bar.invalidate_thumb_frame();

        scroll_bar.last_pointer_tracking_location = Some(Point::new(10.0, 90.0));

        scroll_bar.handle_pointer_move(Point::new(10.0, 91.0));
        assert_eq!(scroll_bar.content_offset, 0.0);
    }

    #[test]
    fn scroll_bar_updates_content_offset_on_move_when_tracking_does_not_exceed_maximum() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 400.0;
        scroll_bar.invalidate_thumb_frame();

        scroll_bar.last_pointer_tracking_location = Some(Point::new(10.0, 90.0));

        scroll_bar.handle_pointer_move(Point::new(10.0, 0.0));
        assert_eq!(scroll_bar.content_offset, 300.0);
    }

    #[test]
    fn scroll_bar_handle_pointer_move_updates_last_tracking_point() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 400.0;
        scroll_bar.invalidate_thumb_frame();

        scroll_bar.last_pointer_tracking_location = Some(Point::new(10.0, 10.0));
        scroll_bar.handle_pointer_move(Point::new(10.0, 11.0));
        assert_eq!(scroll_bar.last_pointer_tracking_location.unwrap(), Point::new(10.0, 11.0));
    }

    #[test]
    fn scroll_bar_begin_pointer_move_updates_last_tracking_point() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 400.0;
        scroll_bar.invalidate_thumb_frame();

        scroll_bar.begin_tracking_pointer_event(Point::new(5.0, 90.0));
        assert_eq!(scroll_bar.last_pointer_tracking_location.unwrap(), Point::new(5.0, 90.0));
    }

    #[test]
    fn scroll_bar_begin_pointer_move_jumps_if_initial_point_outside_thumb_min() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 400.0;
        scroll_bar.invalidate_thumb_frame();

        scroll_bar.begin_tracking_pointer_event(Point::new(5.0, 10.0));
        assert_eq!(scroll_bar.content_offset, 300.0);
    }

    #[test]
    fn scroll_bar_begin_pointer_move_jumps_if_initial_point_outside_thumb() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 500.0;
        scroll_bar.invalidate_thumb_frame();

        scroll_bar.begin_tracking_pointer_event(Point::new(5.0, 50.0));
        assert_eq!(scroll_bar.content_offset, 200.0);
    }

    #[test]
    fn scroll_bar_begin_pointer_move_jumps_if_initial_point_outside_thumb_nonzero_origin() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = Rect::new(Point::new(10.0, 10.0), Size::new(10.0, 100.0));
        scroll_bar.content_height = 500.0;
        scroll_bar.invalidate_thumb_frame();

        scroll_bar.begin_tracking_pointer_event(Point::new(5.0, 60.0));
        assert_eq!(scroll_bar.content_offset, 200.0);
    }

    #[test]
    fn scroll_bar_begin_pointer_move_jumps_if_initial_point_outside_thumb_max() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 400.0;
        scroll_bar.content_offset = 300.0;
        scroll_bar.invalidate_thumb_frame();

        scroll_bar.begin_tracking_pointer_event(Point::new(5.0, 99.0));
        assert_eq!(scroll_bar.content_offset, 0.0);
    }

    #[test]
    fn scroll_bar_cancel_pointer_event_drops_last_tracking_point() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 400.0;
        scroll_bar.invalidate_thumb_frame();

        scroll_bar.begin_tracking_pointer_event(Point::new(10.0, 10.0));
        scroll_bar.cancel_pointer_event();
        assert!(scroll_bar.last_pointer_tracking_location.is_none());
    }

    #[test]
    fn thumb_frame_updated_when_told_thumb_frame_is_invalidated() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.content_height = 10_000.0;
        scroll_bar.invalidate_thumb_frame();
        assert!(scroll_bar.thumb_frame.is_some());
    }

    #[test]
    fn thumb_render_info_none_same_content_size_and_frame() {
        let scroll_bar = ScrollBar::default();
        let thumb_info = scroll_bar.calculate_thumb_render_info();
        assert!(thumb_info.is_none());
    }

    #[test]
    fn thumb_render_info_none_not_scrollable() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(1000.0);
        scroll_bar.content_height = 900.0;

        let thumb_info = scroll_bar.calculate_thumb_render_info();
        assert!(thumb_info.is_none());
    }

    #[test]
    fn scroll_bar_thumb_render_info_returns_proper_height() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.content_height = 2_000.0;
        scroll_bar.frame.size.height = 1_000.0;

        let render_info = scroll_bar.calculate_thumb_render_info().unwrap();
        assert_eq!(render_info.height, 500.0,);
    }

    #[test]
    fn calculate_thumb_height_ratio_pins_to_min() {
        let ratio = ScrollBar::calculate_thumb_height_ratio(100.0, 10_100.0);
        assert_eq!(ratio, super::MINIMUM_THUMB_RATIO);
    }

    #[test]
    fn calculate_thumb_height_ratio_pins_to_max() {
        let ratio = ScrollBar::calculate_thumb_height_ratio(100.0, 101.0);
        assert_eq!(ratio, super::MAXIMUM_THUMB_RATIO);
    }

    #[test]
    fn calculate_thumb_height_ratio() {
        let ratio = ScrollBar::calculate_thumb_height_ratio(10.0, 40.0);
        assert_eq!(ratio, 0.25);
    }

    #[test]
    fn calculate_thumb_vertical_offset_top() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 400.0;
        scroll_bar.content_offset = 300.0;

        let render_info = scroll_bar.calculate_thumb_render_info().unwrap();
        assert_eq!(render_info.vertical_offset, 75.0);
    }

    #[test]
    fn calculate_thumb_vertical_offset_mid() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 400.0;
        scroll_bar.content_offset = 100.0;

        let render_info = scroll_bar.calculate_thumb_render_info().unwrap();

        assert_eq!(render_info.vertical_offset, 25.0);
    }

    #[test]
    fn calculate_thumb_vertical_offset_with_round() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 300.0;
        scroll_bar.content_offset = 100.0;

        let render_info = scroll_bar.calculate_thumb_render_info().unwrap();

        assert_eq!(render_info.vertical_offset, 33.0);
    }

    #[test]
    fn calculate_thumb_vertical_offset_bottom() {
        let mut scroll_bar = ScrollBar::default();
        scroll_bar.frame = rect_with_height(100.0);
        scroll_bar.content_height = 302.0;
        scroll_bar.content_offset = 0.0;

        let render_info = scroll_bar.calculate_thumb_render_info().unwrap();

        assert_eq!(render_info.vertical_offset, 0.0);
    }

    #[test]
    fn scroll_context_thumb_render_info_equality() {
        let first = ThumbRenderInfo { height: 100.0, vertical_offset: 100.0 };
        let second = ThumbRenderInfo { height: 100.0, vertical_offset: 100.0 };
        assert_eq!(first, second);
    }

    #[test]
    fn scroll_context_thumb_render_info_not_equal_diff_offset() {
        let first = ThumbRenderInfo { height: 100.0, vertical_offset: 100.0 };
        let second = ThumbRenderInfo { height: 100.0, vertical_offset: 0.0 };
        assert_ne!(first, second);
    }

    #[test]
    fn scroll_context_thumb_render_info_equality_not_equal_diff_height() {
        let first = ThumbRenderInfo { height: 100.0, vertical_offset: 100.0 };
        let second = ThumbRenderInfo { height: 10.0, vertical_offset: 100.0 };
        assert_ne!(first, second);
    }

    #[test]
    fn thumb_render_info_calculate_frame_in_rect() {
        let thumb_info = ThumbRenderInfo { height: 10.0, vertical_offset: 10.0 };
        let outer = Rect::new(Point::new(10.0, 10.0), Size::new(10.0, 1000.0));
        let rect = thumb_info.calculate_frame_in_rect(&outer);

        assert_eq!(rect, Rect::new(Point::new(10.0, 990.0), Size::new(10.0, 10.0)));
    }
}
