// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(dead_code)]
use {
    crate::ui::terminal_views::{GridView, ScrollBar},
    carnelian::{
        color::Color,
        input::{self},
        render::{Composition, Context as RenderContext, Order, PreClear, RenderExt},
        Coord, Rect, Size, ViewAssistantContext,
    },
    fuchsia_trace as ftrace,
    std::convert::TryFrom,
    term_model::term::RenderableCellsIter,
};

pub struct TerminalScene {
    composition: Composition,
    background_color: Color,
    grid_view: GridView,
    scroll_bar: ScrollBar,
    size: Size,
    scroll_context: ScrollContext,
}

const SCROLL_BAR_WIDTH: f32 = 16.0;

pub enum PointerEventResponse {
    /// Indicates that the pointer event resulted in the user wanting to scroll the grid
    ScrollLines(isize),

    /// Indicates that there are no changes in the grid but the view needs to be updated.
    /// This is usually in response to the scroll bar scrolling but not changing the lines.
    ViewDirty,
}

impl TerminalScene {
    pub fn new(background_color: Color) -> TerminalScene {
        let composition = Composition::new(background_color);
        TerminalScene { background_color, composition, ..TerminalScene::default() }
    }

    pub fn update_background_color(&mut self, new_color: Color) {
        self.background_color = new_color;
    }

    pub fn calculate_term_size_from_size(size: &Size) -> Size {
        Size::new(size.width - SCROLL_BAR_WIDTH, size.height)
    }

    pub fn render<'a, C>(
        &mut self,
        render_context: &mut RenderContext,
        context: &ViewAssistantContext,
        cells: RenderableCellsIter<'a, C>,
    ) {
        ftrace::duration!("terminal", "Scene:TerminalScene:render2");
        let image = render_context.get_current_image(context);
        let ext = RenderExt {
            pre_clear: Some(PreClear { color: self.background_color }),
            ..Default::default()
        };

        let grid_layers = self.grid_view.render(render_context, cells);
        let scroll_bar_layers = self.scroll_bar.render(render_context);

        self.composition.clear();
        for (i, layer) in grid_layers.into_iter().chain(scroll_bar_layers).enumerate() {
            self.composition.insert(Order::try_from(i).unwrap_or_else(|e| panic!("{}", e)), layer);
        }

        render_context.render(&mut self.composition, None, image, &ext);
    }

    pub fn update_size(&mut self, new_size: Size, cell_size: Size) {
        ftrace::duration!("terminal", "Scene:TerminalScene:update_size");

        self.grid_view.cell_size = cell_size;
        self.size = new_size;

        self.grid_view.frame =
            Rect::from_size(TerminalScene::calculate_term_size_from_size(&new_size));

        let mut scroll_bar_frame = Rect::from_size(new_size);
        scroll_bar_frame.size.width = SCROLL_BAR_WIDTH;
        scroll_bar_frame.origin.x = new_size.width - SCROLL_BAR_WIDTH;
        self.scroll_bar.frame = scroll_bar_frame;

        self.update_scroll_metrics();
    }

    pub fn update_scroll_context(&mut self, scroll_context: ScrollContext) {
        if scroll_context != self.scroll_context {
            self.scroll_context = scroll_context;
            self.update_scroll_metrics();
        }
    }

    pub fn handle_pointer_event(
        &mut self,
        event: &input::pointer::Event,
        _ctx: &mut ViewAssistantContext,
    ) -> Option<PointerEventResponse> {
        if self.scroll_bar.is_tracking() {
            return self.handle_primary_pointer_event_for_scroll_bar(&event);
        } else {
            match event.phase {
                input::pointer::Phase::Down(point) => {
                    let point = point.to_f32();
                    if self.scroll_bar.frame.contains(point) {
                        return self.handle_primary_pointer_event_for_scroll_bar(&event);
                    }
                }
                _ => (),
            }
        }

        None
    }

    fn update_scroll_metrics(&mut self) {
        let total_lines = self.scroll_context.history + self.scroll_context.visible_lines;
        let content_height = self.grid_view.cell_size.height * (total_lines as Coord);

        let content_offset =
            self.grid_view.cell_size.height * (self.scroll_context.display_offset as Coord);

        self.scroll_bar.content_height = content_height;
        self.scroll_bar.content_offset = content_offset;
        self.scroll_bar.invalidate_thumb_frame();
    }

    fn handle_primary_pointer_event_for_scroll_bar(
        &mut self,
        event: &input::pointer::Event,
    ) -> Option<PointerEventResponse> {
        // The following logic assumes that we are only tracking one pointer at a time.
        let prev_offset = self.scroll_bar.content_offset;
        match event.phase {
            input::pointer::Phase::Down(location) => {
                self.scroll_bar.begin_tracking_pointer_event(location.to_f32());
            }
            input::pointer::Phase::Moved(location) => {
                self.scroll_bar.handle_pointer_move(location.to_f32())
            }
            input::pointer::Phase::Up
            | input::pointer::Phase::Remove
            | input::pointer::Phase::Cancel => self.scroll_bar.cancel_pointer_event(),
        };

        // do not rely on the ScrollContext being udpated at this point. Rely on the
        // values stored in the scroll bar to determine how many rows we have changed.
        let offset_change = Self::calculate_change_in_scroll_offset(
            prev_offset,
            self.scroll_bar.content_offset,
            self.grid_view.cell_size.height,
        );

        match offset_change {
            0 => Some(PointerEventResponse::ViewDirty),
            _ => Some(PointerEventResponse::ScrollLines(offset_change)),
        }
    }

    fn calculate_change_in_scroll_offset(
        previous_content_offset: f32,
        content_offset: f32,
        row_height: f32,
    ) -> isize {
        let new_row = f32::floor(content_offset / row_height);
        let old_row = f32::floor(previous_content_offset / row_height);
        (new_row - old_row) as isize
    }
}

impl Default for TerminalScene {
    fn default() -> Self {
        let background_color = Color::new();
        let composition = Composition::new(background_color);
        TerminalScene {
            composition,
            background_color,
            size: Size::zero(),
            grid_view: GridView::new(&background_color),
            scroll_bar: ScrollBar::default(),
            scroll_context: ScrollContext::default(),
        }
    }
}

#[derive(Eq, PartialEq)]
pub struct ScrollContext {
    pub history: usize,
    pub visible_lines: usize,
    pub display_offset: usize,
}

impl Default for ScrollContext {
    fn default() -> Self {
        ScrollContext { history: 0, visible_lines: 0, display_offset: 0 }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, carnelian::Point};

    #[test]
    fn calculate_difference_in_display_offset_no_change() {
        let row_height = 10.0;
        let content_offset = 10.1;
        let prev_offset = 10.0;

        // jump from row 1 to row 1
        let delta = TerminalScene::calculate_change_in_scroll_offset(
            prev_offset,
            content_offset,
            row_height,
        );
        assert_eq!(delta, 0);
    }

    #[test]
    fn calculate_difference_in_display_offset_single_line() {
        let row_height = 10.0;
        let content_offset = 20.1;
        let prev_offset = 10.0;

        // jump from row 1 to row 2
        let delta = TerminalScene::calculate_change_in_scroll_offset(
            prev_offset,
            content_offset,
            row_height,
        );
        assert_eq!(delta, 1);
    }

    #[test]
    fn calculate_difference_in_display_offset_down_single_line() {
        let row_height = 10.0;
        let content_offset = 19.0;
        let prev_offset = 20.0;

        // jump from row 2 to row 1
        let delta = TerminalScene::calculate_change_in_scroll_offset(
            prev_offset,
            content_offset,
            row_height,
        );
        assert_eq!(delta, -1);
    }

    #[test]
    fn calculate_difference_in_display_offset_multiple_lines() {
        let row_height = 10.0;
        let content_offset = 25.0;
        let prev_offset = 0.0;

        // jump from row 0 to row 2
        let delta = TerminalScene::calculate_change_in_scroll_offset(
            prev_offset,
            content_offset,
            row_height,
        );
        assert_eq!(delta, 2);
    }

    #[test]
    fn new_with_color_sets_background_color() {
        let scene = TerminalScene::new(Color { r: 255, ..Color::new() });
        assert_eq!(scene.background_color.r, 255);
    }

    #[test]
    fn update_background_color_sets_color_of_bg_view() {
        let mut scene = TerminalScene::default();
        scene.update_background_color(Color { g: 255, ..Color::new() });
        assert_eq!(scene.background_color.g, 255);
    }

    #[test]
    fn update_cell_size_updates_grid_view() {
        let mut scene = TerminalScene::default();
        scene.update_size(Size::zero(), Size::new(99.0, 99.0));
        assert_eq!(scene.grid_view.cell_size, Size::new(99.0, 99.0));
    }

    #[test]
    fn calculate_term_size_has_affordance_for_scroll_bar() {
        let size = Size::new(100.0, 100.0);
        let calculated = TerminalScene::calculate_term_size_from_size(&size);

        assert_eq!(calculated.width, 84.0);
        assert_eq!(calculated.height, 100.0);
    }

    #[test]
    fn update_size_sets_frames() {
        let mut scene = TerminalScene::default();
        let size = Size::new(100.0, 100.0);
        scene.update_size(size, Size::zero());

        assert_eq!(scene.grid_view.frame, Rect::new(Point::zero(), Size::new(84.0, 100.0)));
        assert_eq!(
            scene.scroll_bar.frame,
            Rect::new(Point::new(84.0, 0.0), Size::new(16.0, 100.0))
        );
    }

    #[test]
    fn update_scroll_context_updates_content_height() {
        let mut scene = TerminalScene::default();
        scene.update_size(Size::zero(), Size::new(10.0, 10.0));
        assert_eq!(scene.scroll_bar.content_height, 0.0);

        let scroll_context = ScrollContext { history: 300, visible_lines: 2, display_offset: 0 };

        scene.update_scroll_context(scroll_context);
        assert_eq!(scene.scroll_bar.content_height, 3020.0);
    }

    #[test]
    fn update_scroll_context_updates_content_offset() {
        let mut scene = TerminalScene::default();
        scene.update_size(Size::zero(), Size::new(10.0, 10.0));
        let scroll_context = ScrollContext { history: 300, visible_lines: 2, display_offset: 10 };

        scene.update_scroll_context(scroll_context);
        assert_eq!(scene.scroll_bar.content_offset, 100.0);
    }

    #[test]
    fn update_cell_size_updates_content_size() {
        let mut scene = TerminalScene::default();

        let scroll_context = ScrollContext { history: 300, visible_lines: 2, display_offset: 0 };

        scene.update_scroll_context(scroll_context);

        scene.update_size(Size::zero(), Size::new(10.0, 5.0));
        assert_eq!(scene.scroll_bar.content_height, 1510.0);
    }

    #[test]
    fn update_cell_size_updates_content_offset() {
        let mut scene = TerminalScene::default();

        // originally frame is much bigger than content so there is no offset
        scene.update_size(Size::zero(), Size::new(10.0, 10.0));
        let scroll_context = ScrollContext { history: 10, visible_lines: 10, display_offset: 10 };
        scene.update_scroll_context(scroll_context);

        assert_eq!(scene.scroll_bar.content_offset, 100.0);

        scene.update_size(Size::new(100.0, 1000.0), Size::new(10.0, 5.0));
        assert_eq!(scene.scroll_bar.content_offset, 50.0);
    }
}
