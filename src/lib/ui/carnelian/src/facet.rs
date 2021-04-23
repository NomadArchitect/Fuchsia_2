// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{
    color::Color,
    drawing::{
        linebreak_text, path_for_corner_knockouts, path_for_cursor, path_for_rectangle, FontFace,
        GlyphMap, Text,
    },
    geometry::IntPoint,
    render::{
        rive::RenderCache as RiveRenderCache, BlendMode, Composition, Context as RenderContext,
        Fill, FillRule, Layer, PreClear, Raster, RenderExt, Shed, Style,
    },
    Coord, Point, Rect, Size, ViewAssistantContext,
};
use anyhow::{bail, Error};
use euclid::{default::Transform2D, point2, size2, vec2};
use fuchsia_zircon::{self as zx, AsHandleRef, Event, Signals, Time};
use rive_rs::{
    self as rive,
    animation::{LinearAnimation, LinearAnimationInstance},
    layout::{self, Alignment, Fit},
    math::Aabb,
};
use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Debug)]
pub struct SetColorMessage {
    pub color: Color,
}

pub struct SetTextMessage {
    pub text: String,
}

pub struct SetLocationMessage {
    pub location: Point,
}

pub struct SetSizeMessage {
    pub size: Size,
}

pub trait Facet {
    fn update_layers(
        &mut self,
        size: Size,
        layer_group: &mut LayerGroup,
        render_context: &mut RenderContext,
    ) -> Result<(), Error>;

    fn handle_message(&mut self, msg: Box<dyn Any>) {
        println!("Unhandled message {:#?}", msg);
    }
}

pub type FacetPtr = Box<dyn Facet>;
pub type LayerIterator = Box<dyn Iterator<Item = Layer>>;

pub struct RectangleFacet {
    bounds: Rect,
    color: Color,
    raster: Option<Raster>,
}

impl RectangleFacet {
    pub fn new(bounds: Rect, color: Color) -> FacetPtr {
        Box::new(Self { bounds, color, raster: None })
    }

    pub fn h_line(
        y: Coord,
        x_start: Coord,
        x_end: Coord,
        thickness: Coord,
        color: Color,
    ) -> FacetPtr {
        let x = x_start.min(x_end);
        let half_thickness = thickness / 2.0;
        let top_left = point2(x, y - half_thickness);
        let line_bounds = Rect::new(top_left, size2((x_end - x_start).abs(), thickness));
        Self::new(line_bounds, color)
    }

    pub fn v_line(
        x: Coord,
        y_start: Coord,
        y_end: Coord,
        thickness: Coord,
        color: Color,
    ) -> FacetPtr {
        let y = y_start.min(y_end);
        let height = (y_end - y_start).abs();
        let half_thickness = thickness / 2.0;
        let top_left = point2(x - half_thickness, y);
        let line_bounds = Rect::new(top_left, size2(thickness, height));
        Self::new(line_bounds, color)
    }
}

impl Facet for RectangleFacet {
    fn update_layers(
        &mut self,
        _size: Size,
        layer_group: &mut LayerGroup,
        render_context: &mut RenderContext,
    ) -> Result<(), Error> {
        let line_raster = self.raster.take().unwrap_or_else(|| {
            let line_path = path_for_rectangle(&self.bounds, render_context);
            let mut raster_builder = render_context.raster_builder().expect("raster_builder");
            raster_builder.add(&line_path, None);
            raster_builder.build()
        });
        let raster = line_raster.clone();
        self.raster = Some(line_raster);
        layer_group.replace_all(std::iter::once(Layer {
            raster: raster,
            style: Style {
                fill_rule: FillRule::NonZero,
                fill: Fill::Solid(self.color),
                blend_mode: BlendMode::Over,
            },
        }));
        Ok(())
    }

    fn handle_message(&mut self, msg: Box<dyn Any>) {
        if let Some(set_color) = msg.downcast_ref::<SetColorMessage>() {
            self.color = set_color.color;
        }
    }
}

pub enum TextHorizontalAlignment {
    Left,
    Right,
    Center,
}

impl Default for TextHorizontalAlignment {
    fn default() -> Self {
        Self::Left
    }
}

pub enum TextVerticalAlignment {
    Baseline,
    Top,
    Bottom,
    Center,
}

impl Default for TextVerticalAlignment {
    fn default() -> Self {
        Self::Baseline
    }
}

#[derive(Default)]
pub struct TextFacetOptions {
    pub horizontal_alignment: TextHorizontalAlignment,
    pub vertical_alignment: TextVerticalAlignment,
    pub color: Color,
    pub max_width: Option<f32>,
}

pub struct TextFacet {
    face: FontFace,
    lines: Vec<String>,
    size: f32,
    location: Point,
    options: TextFacetOptions,
    rendered_text: Option<Text>,
    glyphs: GlyphMap,
}

impl TextFacet {
    fn wrap_lines(face: &FontFace, size: f32, text: &str, max_width: &Option<f32>) -> Vec<String> {
        let lines: Vec<String> = text.lines().map(|line| String::from(line)).collect();
        if let Some(max_width) = max_width {
            let wrapped_lines = lines
                .iter()
                .map(|line| linebreak_text(face, size, line, *max_width))
                .flatten()
                .collect();
            wrapped_lines
        } else {
            lines
        }
    }

    pub fn new(face: FontFace, text: &str, size: f32, location: Point) -> FacetPtr {
        Self::with_options(face, text, size, location, TextFacetOptions::default())
    }

    pub fn with_options(
        face: FontFace,
        text: &str,
        size: f32,
        location: Point,
        options: TextFacetOptions,
    ) -> FacetPtr {
        let lines = Self::wrap_lines(&face, size, text, &options.max_width);

        Box::new(Self {
            face,
            lines,
            size,
            location,
            options,
            rendered_text: None,
            glyphs: GlyphMap::new(),
        })
    }
}

impl Facet for TextFacet {
    fn update_layers(
        &mut self,
        _size: Size,
        layer_group: &mut LayerGroup,
        render_context: &mut RenderContext,
    ) -> Result<(), Error> {
        let rendered_text = self.rendered_text.take().unwrap_or_else(|| {
            Text::new_with_lines(
                render_context,
                &self.lines,
                self.size,
                &self.face,
                &mut self.glyphs,
            )
        });
        let ascent = self.face.ascent(self.size);
        let descent = self.face.descent(self.size);
        let x = match self.options.horizontal_alignment {
            TextHorizontalAlignment::Left => self.location.x,
            TextHorizontalAlignment::Center => {
                self.location.x - rendered_text.bounding_box.size.width / 2.0
            }
            TextHorizontalAlignment::Right => {
                self.location.x - rendered_text.bounding_box.size.width
            }
        };
        let y = match self.options.vertical_alignment {
            TextVerticalAlignment::Baseline => self.location.y - ascent,
            TextVerticalAlignment::Top => self.location.y,
            TextVerticalAlignment::Bottom => self.location.y - ascent + descent,
            TextVerticalAlignment::Center => {
                let capital_height = self.face.capital_height(self.size).unwrap_or(self.size);
                self.location.y + capital_height / 2.0 - ascent
            }
        };
        let translation = vec2(x, y);
        let raster = rendered_text.raster.clone().translate(translation.to_i32());
        self.rendered_text = Some(rendered_text);

        layer_group.replace_all(std::iter::once(Layer {
            raster,
            style: Style {
                fill_rule: FillRule::NonZero,
                fill: Fill::Solid(self.options.color),
                blend_mode: BlendMode::Over,
            },
        }));
        Ok(())
    }

    fn handle_message(&mut self, msg: Box<dyn Any>) {
        if let Some(set_text) = msg.downcast_ref::<SetTextMessage>() {
            self.lines =
                Self::wrap_lines(&self.face, self.size, &set_text.text, &self.options.max_width);
            self.rendered_text = None;
        } else if let Some(set_color) = msg.downcast_ref::<SetColorMessage>() {
            self.options.color = set_color.color;
        }
    }
}

pub struct RasterFacet {
    raster: Raster,
    style: Style,
    location: Point,
}

impl RasterFacet {
    pub fn new(raster: Raster, style: Style, location: Point) -> Self {
        Self { raster, style, location }
    }
}

impl Facet for RasterFacet {
    fn update_layers(
        &mut self,
        _size: Size,
        layer_group: &mut LayerGroup,
        _render_context: &mut RenderContext,
    ) -> Result<(), Error> {
        layer_group.replace_all(std::iter::once(Layer {
            raster: self.raster.clone().translate(self.location.to_vector().to_i32()),
            style: self.style.clone(),
        }));
        Ok(())
    }

    fn handle_message(&mut self, msg: Box<dyn Any>) {
        if let Some(set_location) = msg.downcast_ref::<SetLocationMessage>() {
            self.location = set_location.location;
        }
    }
}

pub struct ShedFacet {
    path: PathBuf,
    location: Point,
    size: Size,
    rasters: Option<Vec<(Raster, Style)>>,
}

impl ShedFacet {
    pub fn new(path: PathBuf, location: Point, size: Size) -> Self {
        Self { path, location, size, rasters: None }
    }
}

impl Facet for ShedFacet {
    fn update_layers(
        &mut self,
        _size: Size,
        layer_group: &mut LayerGroup,
        render_context: &mut RenderContext,
    ) -> Result<(), Error> {
        let rasters = self.rasters.take().unwrap_or_else(|| {
            if let Some(shed) = Shed::open(&self.path).ok() {
                let shed_size = shed.size();
                let scale_factor: Size =
                    size2(self.size.width / shed_size.width, self.size.height / shed_size.height);
                let transform =
                    Transform2D::translation(-shed_size.width / 2.0, -shed_size.height / 2.0)
                        .then_scale(scale_factor.width, scale_factor.height);

                shed.rasters(render_context, Some(&transform))
            } else {
                let placeholder_rect =
                    Rect::from_size(self.size).translate(self.size.to_vector() / -2.0);
                let rect_path = path_for_rectangle(&placeholder_rect, render_context);
                let mut raster_builder = render_context.raster_builder().expect("raster_builder");
                raster_builder.add(&rect_path, None);
                let raster = raster_builder.build();
                vec![(
                    raster,
                    Style {
                        fill_rule: FillRule::NonZero,
                        fill: Fill::Solid(Color::red()),
                        blend_mode: BlendMode::Over,
                    },
                )]
            }
        });
        let location = self.location;
        layer_group.replace_all(rasters.iter().map(|(raster, style)| Layer {
            raster: raster.clone().translate(location.to_vector().to_i32()),
            style: *style,
        }));
        self.rasters = Some(rasters);
        Ok(())
    }

    fn handle_message(&mut self, msg: Box<dyn Any>) {
        if let Some(set_location) = msg.downcast_ref::<SetLocationMessage>() {
            self.location = set_location.location;
        }
    }
}

pub struct ToggleAnimationMessage {
    pub index: usize,
}

pub struct RiveFacet {
    location: Point,
    size: Size,
    file: rive::File,
    animations: Vec<(LinearAnimationInstance, bool)>,
    last_presentation_time: Option<Time>,
    render_cache: RiveRenderCache,
}

impl RiveFacet {
    pub fn new(
        path: PathBuf,
        location: Point,
        size: Size,
        initial_animations: impl IntoIterator<Item = usize>,
    ) -> Self {
        let buffer = fs::read(path).expect("failed to open .riv file");
        let mut reader = rive::BinaryReader::new(&buffer);
        let file = rive::File::import(&mut reader).expect("failed to import .riv file");
        let artboard = file.artboard().unwrap();
        let artboard_ref = artboard.as_ref();
        artboard_ref.advance(0.0);
        let mut animations: Vec<(LinearAnimationInstance, bool)> = artboard_ref
            .animations::<LinearAnimation>()
            .map(|animation| (LinearAnimationInstance::new(animation), false))
            .collect();
        for index in initial_animations.into_iter() {
            if index < animations.len() {
                animations[index].1 = true;
            }
        }

        Self {
            location,
            size,
            file,
            animations,
            last_presentation_time: None,
            render_cache: RiveRenderCache::new(),
        }
    }
}

impl Facet for RiveFacet {
    fn update_layers(
        &mut self,
        _size: Size,
        layer_group: &mut LayerGroup,
        render_context: &mut RenderContext,
    ) -> Result<(), Error> {
        let presentation_time = zx::Time::get_monotonic();
        let elapsed = if let Some(last_presentation_time) = self.last_presentation_time {
            const NANOS_PER_SECOND: f32 = 1_000_000_000.0;
            (presentation_time - last_presentation_time).into_nanos() as f32 / NANOS_PER_SECOND
        } else {
            0.0
        };
        self.last_presentation_time = Some(presentation_time);

        let artboard = self.file.artboard().unwrap();
        let artboard_ref = artboard.as_ref();

        for (animation_instance, is_animating) in self.animations.iter_mut() {
            if *is_animating {
                animation_instance.advance(elapsed);
                animation_instance.apply(artboard.clone(), 1.0);
            }
            if animation_instance.is_done() {
                animation_instance.reset();
                *is_animating = false;
            }
        }

        let width = self.size.width as f32;
        let height = self.size.height as f32;
        self.render_cache.with_renderer(render_context, |renderer| {
            artboard_ref.advance(elapsed);
            artboard_ref.draw(
                renderer,
                layout::align(
                    Fit::Contain,
                    Alignment::center(),
                    Aabb::new(0.0, 0.0, width, height),
                    artboard.as_ref().bounds(),
                ),
            );
        });

        let location = self.location;
        layer_group.replace_all(self.render_cache.rasters.iter().rev().map(|(raster, style)| {
            Layer { raster: raster.clone().translate(location.to_vector().to_i32()), style: *style }
        }));

        Ok(())
    }

    fn handle_message(&mut self, msg: Box<dyn Any>) {
        if let Some(set_location) = msg.downcast_ref::<SetLocationMessage>() {
            self.location = set_location.location;
        }
        if let Some(set_size) = msg.downcast_ref::<SetSizeMessage>() {
            self.size = set_size.size;
        }
        if let Some(toggle_animation) = msg.downcast_ref::<ToggleAnimationMessage>() {
            let i = toggle_animation.index;
            if i < self.animations.len() {
                self.animations[i].1 = !self.animations[i].1;
            }
        }
    }
}

struct Rendering {
    size: Size,
    previous_rasters: Vec<Raster>,
}

impl Rendering {
    fn new() -> Rendering {
        Rendering { previous_rasters: Vec::new(), size: Size::zero() }
    }
}

fn raster_for_corner_knockouts(
    bounds: &Rect,
    corner_radius: Coord,
    render_context: &mut RenderContext,
) -> Raster {
    let path = path_for_corner_knockouts(bounds, corner_radius, render_context);
    let mut raster_builder = render_context.raster_builder().expect("raster_builder");
    raster_builder.add(&path, None);
    raster_builder.build()
}

pub struct SceneOptions {
    pub background_color: Color,
    pub round_scene_corners: bool,
}

impl Default for SceneOptions {
    fn default() -> Self {
        Self { background_color: Color::new(), round_scene_corners: true }
    }
}

fn create_mouse_cursor_raster(render_context: &mut RenderContext) -> Raster {
    let path = path_for_cursor(Point::zero(), 20.0, render_context);
    let mut raster_builder = render_context.raster_builder().expect("raster_builder");
    raster_builder.add(&path, None);
    raster_builder.build()
}

fn cursor_layer(cursor_raster: &Raster, position: IntPoint, color: &Color) -> Layer {
    Layer {
        raster: cursor_raster.clone().translate(position.to_vector()),
        style: Style {
            fill_rule: FillRule::NonZero,
            fill: Fill::Solid(*color),
            blend_mode: BlendMode::Over,
        },
    }
}

fn cursor_layer_pair(cursor_raster: &Raster, position: IntPoint) -> Vec<Layer> {
    let black_pos = position + vec2(-1, -1);
    vec![
        cursor_layer(cursor_raster, position, &Color::fuchsia()),
        cursor_layer(cursor_raster, black_pos, &Color::new()),
    ]
}

pub type FacetId = usize;
pub type FacetMap = BTreeMap<FacetId, FacetPtr>;

pub struct LayerGroup(Vec<Layer>);

impl LayerGroup {
    pub fn replace_all(&mut self, new_layers: impl IntoIterator<Item = Layer>) {
        self.0 = new_layers.into_iter().collect();
    }
}

#[derive(Default)]
struct IdGenerator {}

impl Iterator for IdGenerator {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(100);
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        // fetch_add wraps on overflow, which we'll use as a signal
        // that this generator is out of ids.
        if id == 0 {
            None
        } else {
            Some(id)
        }
    }
}

type FacetIdGenerator = IdGenerator;
pub type LayerMap = BTreeMap<FacetId, LayerGroup>;

pub struct Scene {
    renderings: HashMap<u64, Rendering>,
    mouse_cursor_raster: Option<Raster>,
    facet_id_generator: FacetIdGenerator,
    facets: FacetMap,
    facet_order: Vec<FacetId>,
    layers: LayerMap,
    composition: Composition,
    options: SceneOptions,
}

impl Scene {
    fn new_from_builder(
        options: SceneOptions,
        facets: FacetMap,
        facet_id_generator: FacetIdGenerator,
    ) -> Self {
        let facet_order: Vec<FacetId> = facets.iter().map(|(facet_id, _)| *facet_id).collect();
        Self {
            renderings: HashMap::new(),
            mouse_cursor_raster: None,
            facet_id_generator,
            facets,
            facet_order,
            layers: LayerMap::new(),
            composition: Composition::new(options.background_color),
            options,
        }
    }

    pub fn round_scene_corners(&mut self, round_scene_corners: bool) {
        self.options.round_scene_corners = round_scene_corners;
    }

    pub fn add_facet(&mut self, facet: FacetPtr) -> FacetId {
        let facet_id = self.facet_id_generator.next().expect("facet ID");
        self.facets.insert(facet_id, facet);
        self.facet_order.push(facet_id);
        facet_id
    }

    pub fn remove_facet(&mut self, facet_id: FacetId) -> Result<(), Error> {
        if let Some(_) = self.facets.remove(&facet_id).as_mut() {
            self.layers.remove(&facet_id);
            self.facet_order.retain(|fid| facet_id != *fid);
            Ok(())
        } else {
            bail!("Tried to remove non-existant facet")
        }
    }

    pub fn move_facet_forward(&mut self, facet_id: FacetId) -> Result<(), Error> {
        if let Some(index) = self.facet_order.iter().position(|fid| *fid == facet_id) {
            if index > 0 {
                let new_index = index - 1;
                self.facet_order.swap(new_index, index)
            }
            Ok(())
        } else {
            bail!("Tried to move_facet_forward non-existant facet")
        }
    }

    pub fn move_facet_backward(&mut self, facet_id: FacetId) -> Result<(), Error> {
        if let Some(index) = self.facet_order.iter().position(|fid| *fid == facet_id) {
            if index < self.facet_order.len() - 1 {
                let new_index = index + 1;
                self.facet_order.swap(new_index, index)
            }
            Ok(())
        } else {
            bail!("Tried to move_facet_backward non-existant facet")
        }
    }

    pub fn layers(&mut self, size: Size, render_context: &mut RenderContext) -> Vec<Layer> {
        let mut layers = Vec::new();

        for facet_id in &self.facet_order {
            let facet = self.facets.get_mut(facet_id).expect("facet");
            let facet_layers = if let Some(facet_layers) = self.layers.get(facet_id) {
                facet_layers.0.clone()
            } else {
                Vec::new()
            };
            let mut layer_group = LayerGroup(facet_layers);
            facet.update_layers(size, &mut layer_group, render_context).expect("update_layers");
            layers.append(&mut layer_group.0.clone());
            self.layers.insert(*facet_id, layer_group);
        }
        layers
    }

    fn create_or_update_rendering(
        renderings: &mut HashMap<u64, Rendering>,
        background_color: Color,
        context: &ViewAssistantContext,
    ) -> Option<PreClear> {
        let image_id = context.image_id;
        let size_rendering = renderings.entry(image_id).or_insert_with(|| Rendering::new());
        let size = context.size;
        if size != size_rendering.size {
            size_rendering.size = context.size;
            size_rendering.previous_rasters.clear();
            Some(PreClear { color: background_color })
        } else {
            None
        }
    }

    fn update_composition(
        image_id: u64,
        layers: Vec<Layer>,
        mouse_position: &Option<IntPoint>,
        mouse_cursor_raster: &Option<Raster>,
        corner_knockouts: &Option<Raster>,
        renderings: &mut HashMap<u64, Rendering>,
        background_color: Color,
        composition: &mut Composition,
    ) -> Vec<Layer> {
        let corner_knockouts_layer = corner_knockouts.as_ref().and_then(|raster| {
            Some(Layer {
                raster: raster.clone(),
                style: Style {
                    fill_rule: FillRule::NonZero,
                    fill: Fill::Solid(Color::new()),
                    blend_mode: BlendMode::Over,
                },
            })
        });

        let cursor_layers: Vec<Layer> = mouse_position
            .and_then(|position| {
                let mouse_cursor_raster =
                    mouse_cursor_raster.as_ref().expect("mouse_cursor_raster");
                Some(cursor_layer_pair(mouse_cursor_raster, position))
            })
            .into_iter()
            .flatten()
            .collect();

        let clear_rendering = renderings.get_mut(&image_id).expect("rendering");

        composition.replace(
            ..,
            cursor_layers
                .clone()
                .into_iter()
                .chain(corner_knockouts_layer.into_iter())
                .chain(layers.into_iter())
                .chain(clear_rendering.previous_rasters.drain(..).map(|raster| Layer {
                    raster,
                    style: Style {
                        fill_rule: FillRule::WholeTile,
                        fill: Fill::Solid(background_color),
                        blend_mode: BlendMode::Over,
                    },
                })),
        );

        cursor_layers
    }

    pub fn render(
        &mut self,
        render_context: &mut RenderContext,
        ready_event: Event,
        context: &ViewAssistantContext,
    ) -> Result<(), Error> {
        let image = render_context.get_current_image(context);
        let image_id = context.image_id;
        let background_color = self.options.background_color;
        let pre_clear =
            Self::create_or_update_rendering(&mut self.renderings, background_color, context);
        let size = context.size;

        let ext = RenderExt { pre_clear, ..Default::default() };

        let corner_knockouts = if self.options.round_scene_corners {
            Some(raster_for_corner_knockouts(&Rect::from_size(size), 10.0, render_context))
        } else {
            None
        };

        if context.mouse_cursor_position.is_some() && self.mouse_cursor_raster.is_none() {
            self.mouse_cursor_raster = Some(create_mouse_cursor_raster(render_context));
        }

        let layers: Vec<Layer> = self.layers(size, render_context);
        let cursor_layer = Self::update_composition(
            image_id,
            layers.clone(),
            &context.mouse_cursor_position,
            &self.mouse_cursor_raster,
            &corner_knockouts,
            &mut self.renderings,
            background_color,
            &mut self.composition,
        );
        render_context.render(&self.composition, None, image, &ext);
        ready_event.as_handle_ref().signal(Signals::NONE, Signals::EVENT_SIGNALED)?;

        let update_rendering = self.renderings.entry(image_id).or_insert_with(|| Rendering::new());

        let previous_rasters: Vec<Raster> = layers
            .iter()
            .chain(cursor_layer.iter())
            .map(|layer| layer.raster.clone())
            .chain(corner_knockouts.into_iter())
            .collect();
        update_rendering.previous_rasters = previous_rasters;
        Ok(())
    }

    pub fn send_message(&mut self, target: &FacetId, msg: Box<dyn Any>) {
        if let Some(facet) = self.facets.get_mut(target) {
            facet.handle_message(msg);
        }
    }
}

pub struct SceneBuilder {
    background_color: Color,
    round_scene_corners: bool,
    facet_id_generator: FacetIdGenerator,
    facets: FacetMap,
}

impl SceneBuilder {
    pub fn new(background_color: Color) -> Self {
        Self {
            background_color,
            round_scene_corners: false,
            facet_id_generator: FacetIdGenerator::default(),
            facets: FacetMap::new(),
        }
    }

    pub fn round_scene_corners(&mut self, round: bool) {
        self.round_scene_corners = round;
    }

    fn allocate_facet_id(&mut self) -> FacetId {
        self.facet_id_generator.next().expect("facet_id")
    }

    fn push_facet(&mut self, facet: FacetPtr) -> FacetId {
        let facet_id = self.allocate_facet_id();
        self.facets.insert(facet_id.clone(), facet);
        facet_id
    }

    pub fn rectangle(&mut self, bounds: Rect, color: Color) -> FacetId {
        self.push_facet(RectangleFacet::new(bounds, color))
    }

    pub fn h_line(
        &mut self,
        y: Coord,
        x_start: Coord,
        x_end: Coord,
        thickness: Coord,
        color: Color,
    ) -> FacetId {
        self.push_facet(RectangleFacet::h_line(y, x_start, x_end, thickness, color))
    }

    pub fn v_line(
        &mut self,
        x: Coord,
        y_start: Coord,
        y_end: Coord,
        thickness: Coord,
        color: Color,
    ) -> FacetId {
        self.push_facet(RectangleFacet::v_line(x, y_start, y_end, thickness, color))
    }

    pub fn text(
        &mut self,
        face: FontFace,
        text: &str,
        size: f32,
        location: Point,
        options: TextFacetOptions,
    ) -> FacetId {
        self.push_facet(TextFacet::with_options(face, text, size, location, options))
    }

    pub fn facet(&mut self, facet: FacetPtr) -> FacetId {
        self.push_facet(facet)
    }

    pub fn scene_options(&self) -> SceneOptions {
        SceneOptions {
            background_color: self.background_color,
            round_scene_corners: self.round_scene_corners,
        }
    }

    pub fn build(self) -> Scene {
        Scene::new_from_builder(self.scene_options(), self.facets, self.facet_id_generator)
    }
}
