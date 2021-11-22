// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::buffer::Buffer,
    crate::client::{Client, TaskQueue},
    crate::display::Callback,
    crate::object::{NewObjectExt, ObjectRef, RequestReceiver},
    crate::subcompositor::Subsurface,
    crate::xdg_shell::XdgSurface,
    anyhow::{format_err, Error},
    fidl_fuchsia_math::{Rect, Size},
    fuchsia_async as fasync, fuchsia_trace as ftrace, fuchsia_wayland_core as wl,
    fuchsia_zircon::{self as zx, HandleBased},
    std::mem,
    wayland::{
        WlBufferEvent, WlCompositor, WlCompositorRequest, WlRegion, WlRegionRequest, WlSurface,
        WlSurfaceRequest,
    },
};

#[cfg(feature = "flatland")]
use {
    crate::buffer::ImageInstanceId,
    crate::scenic::Flatland,
    fidl_fuchsia_math::{RectF, SizeU},
    fidl_fuchsia_ui_composition::TransformId,
    std::{
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering},
    },
};

#[cfg(not(feature = "flatland"))]
use {crate::scenic::ScenicSession, fuchsia_scenic as scenic, futures::prelude::*};

#[cfg(feature = "flatland")]
static NEXT_IMAGE_INSTANCE_ID: AtomicUsize = AtomicUsize::new(1);

/// An implementation of the wl_compositor global.
pub struct Compositor;

impl Compositor {
    /// Creates a new `Compositor`.
    pub fn new() -> Self {
        Self
    }
}

impl RequestReceiver<WlCompositor> for Compositor {
    fn receive(
        _this: ObjectRef<Self>,
        request: WlCompositorRequest,
        client: &mut Client,
    ) -> Result<(), Error> {
        match request {
            WlCompositorRequest::CreateSurface { id } => {
                let surface_id = id.id();
                id.implement(client, Surface::new(surface_id))?;
            }
            WlCompositorRequest::CreateRegion { id } => {
                id.implement(client, Region)?;
            }
        }
        Ok(())
    }
}

/// A `SurfaceNode` manages the set of flatland resources associated with a
/// surface.
#[cfg(feature = "flatland")]
struct SurfaceNode {
    /// The flatland instance that can be used to create flatland entities.
    pub flatland: Flatland,
    /// The flatland transform that represents this surface. Views can present this
    /// surface by placeing this transform in their view hierarchy.
    pub transform_id: TransformId,
}

#[cfg(feature = "flatland")]
impl SurfaceNode {
    pub fn new(flatland: Flatland) -> Self {
        let transform_id = flatland.alloc_transform_id();
        flatland.proxy().create_transform(&mut transform_id.clone()).expect("fidl error");
        Self { flatland, transform_id }
    }
}

/// A `SurfaceNode` manages the set of scenic resources associated with a
/// surface.
#[cfg(not(feature = "flatland"))]
struct SurfaceNode {
    /// The scenic session that can be used to create scenic entities.
    pub scenic: ScenicSession,
    /// The scenic node that represents this surface. Views can present this
    /// surface by placeing this node in their view hierarchy.
    pub surface_node: scenic::ShapeNode,
    /// The clip node allows us to clip buffer contents to the surface bounds.
    pub clip_node: scenic::ShapeNode,
    /// The entity node is simply a parent node to hold both the clip and
    /// surface nodes.
    pub entity_node: scenic::EntityNode,
}

#[cfg(not(feature = "flatland"))]
impl SurfaceNode {
    pub fn new(session: ScenicSession) -> Self {
        // To support wp_viewport, we'll build an entity node that contains a
        // shape node for the surface texture, and a clip node to clip the
        // viewport.
        //
        // TODO(fxbug.dev/23396): it would be simpler if we could instead just crop
        // the source image instead of using a clip node.
        let inner = session.as_inner().clone();
        let surface_node = scenic::ShapeNode::new(inner.clone());
        let clip_node = scenic::ShapeNode::new(inner.clone());
        let entity_node = scenic::EntityNode::new(inner.clone());
        // TODO(64996): This is now illegal, we need to instead use the SetClipPlanes command.
        //let clip_material = scenic::Material::new(inner);
        //clip_material.set_texture(None);
        //clip_node.set_material(&clip_material);
        //clip_node.set_translation(0.0, 0.0, std::f32::INFINITY);
        //entity_node.add_part(&clip_node);
        entity_node.add_child(&surface_node);
        entity_node.set_clip(0, true);
        Self { scenic: session, surface_node, clip_node, entity_node }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ViewportCropParams {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ViewportScaleParams {
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum SurfaceRelation {
    Above,
    Below,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct PlaceSubsurfaceParams {
    pub subsurface: ObjectRef<Subsurface>,
    pub sibling: ObjectRef<Surface>,
    pub relation: SurfaceRelation,
}

pub enum SurfaceCommand {
    AttachBuffer(BufferAttachment),
    ClearBuffer,
    Frame(ObjectRef<Callback>),
    SetViewportCropParams(ViewportCropParams),
    ClearViewportCropParams,
    SetViewportScaleParams(ViewportScaleParams),
    ClearViewportScaleParams,
    SetWindowGeometry(Rect),
    SetPosition(i32, i32),
    AddSubsurface(ObjectRef<Surface>, ObjectRef<Subsurface>),
    PlaceSubsurface(PlaceSubsurfaceParams),
}

/// A Surface is the object backing wl_surface protocol objects.
///
/// A Surface alone is not of much use until it's been assigned a role. Surface
/// roles are assigned implicitly when binding the role object to the surface.
///
/// For example, the request wl_shell::get_shell_surface(new_id, object<wl_surface>)
/// will create a new wl_shell_surface object for the provided surface. When
/// this happens we assign the `wl_shell_surface` role to the underlying
/// `wl_surface` object. Once a surface has been assigned a role, it is an error
/// to attempt to assign it a different role.
pub struct Surface {
    /// The current size of this surface as determined by the currently attached
    /// buffer.
    ///
    /// If no buffer is currently associated with this surface the size will be
    /// (0, 0).
    size: Size,

    /// The position of this surface with its parent.
    ///
    /// If this surface has no parent, its position will be (0, 0).
    position: (i32, i32),

    /// The relative z-ordering of this surface relative to other subsurfaces.
    ///
    /// Surfaces with a higher z-order will be drawn over surfaces with a lower
    /// z-order.
    ///
    /// TODO: This is sufficient to implement a single wl_surface with an
    /// arbitrary number of wl_subsurfaces. We'll need to make this more
    /// intelligent to handle the case of nested subsurfaces (that is, a sub-
    /// surface that has child subsurfaces itself).
    z_order: usize,

    /// The assgned role for this surface. This is set to `None` on creation
    /// and is implicitly set when creating the role object.
    ///
    /// Ex:
    ///
    ///   xdg_shell::get_xdg_surface(new_id<xdg_surface>, object<wl_surface>)
    ///
    /// The above request in the xdg_shell interface creates a new xdg_surface
    /// object for the provided wl_surface. This request would assign the
    /// xdg_surface role to the wl_surface.
    role: Option<SurfaceRole>,

    /// The crop parameters are set by a wp_viweport::set_source request.
    ///
    /// If set, this determines how a surface should be cropped to the viewport.
    crop_params: Option<ViewportCropParams>,

    /// The scale parameters are set by a wp_viweport::set_destination request.
    ///
    /// If set, this determines how a surface should be scaled to the viewport.
    scale_params: Option<ViewportScaleParams>,

    /// Callbacks set by the client for redraw hints.
    frame_callbacks: Vec<ObjectRef<Callback>>,

    /// The set of scenic node resources that implement this surface. Initially
    /// this is `None` and becomes populated when a scenic session has been
    /// associated with the surface (see `Surface::set_session`).
    ///
    /// For a surface to be presented, it must have an assigned scenic session
    /// and this node must be included in a scenic resource tree that is mapped
    /// to a View (see XdgToplevel).
    node: Option<SurfaceNode>,

    /// The window geometry defines the portion of the surface that should be
    /// considered the primary window region. Portions of the surface outside
    /// of the window geometry may contain additional detail and/or client-side
    /// decorations.
    ///
    /// If unset then the entire surface bounds will be used as the window
    /// geometry.
    window_geometry: Option<Rect>,

    /// The scaling factor between the logical pixel space used by Scenic, and
    /// the physical pixels we expose to the client.
    #[cfg(not(feature = "flatland"))]
    pixel_scale: (f32, f32),

    /// The set of commands that have been queued up, pending the next commit.
    pending_commands: Vec<SurfaceCommand>,

    /// The set of active subsurfaces attached to this surface.
    ///
    /// The first element of the tuple is the wl_surface id for the surface. The
    /// second tuple element is the wl_subsurface id of the subsurface. The
    /// parent will be inserted into this vector with `None` for the subsurface
    /// ref, which enables this vector to track the current subsurface ordering
    /// of all subsurfaces and the parent.
    subsurfaces: Vec<(ObjectRef<Surface>, Option<ObjectRef<Subsurface>>)>,

    /// Parent and offset that can be set using aura shell interface.
    parent: Option<ObjectRef<Surface>>,
    offset: Option<(i32, i32)>,

    /// Queue of frame callbacks.
    ///
    /// The client can request multiple frame callbacks for each frame. The inner
    /// vector is the callbacks requested for a frame. The outer deque is the
    /// queue of frames.
    #[cfg(feature = "flatland")]
    callbacks: VecDeque<Vec<ObjectRef<Callback>>>,

    /// Present credits that determine if we are allowed to present.
    #[cfg(feature = "flatland")]
    present_credits: u32,

    /// Global identifier for image instances used by this surface.
    #[cfg(feature = "flatland")]
    image_instance_id: ImageInstanceId,

    /// The current content of this surface as determined by the currently attached
    /// buffer.
    #[cfg(feature = "flatland")]
    content: Option<BufferAttachment>,
}

impl Surface {
    /// Enqueues a command for this surface to take effect on the next call to
    /// wl_surface::commit.
    pub fn enqueue(&mut self, command: SurfaceCommand) {
        self.pending_commands.push(command);
    }

    pub fn detach_subsurface(&mut self, subsurface_ref: ObjectRef<Subsurface>) {
        if let Some(index) = self.subsurfaces.iter().position(|x| x.1 == Some(subsurface_ref)) {
            self.subsurfaces.remove(index);
        }
    }

    /// Assigns a role to this surface.
    ///
    /// The role can be updated as long as the type of role remains the same,
    /// it is an error to set a different type of role for that same surface.
    pub fn set_role(&mut self, role: SurfaceRole) -> Result<(), Error> {
        ftrace::duration!("wayland", "Surface::set_role");
        // The role is valid unless a different role has been assigned before.
        let valid_role = match &self.role {
            Some(SurfaceRole::XdgSurface(_)) => match role {
                SurfaceRole::XdgSurface(_) => true,
                _ => false,
            },
            Some(SurfaceRole::Subsurface(_)) => match role {
                SurfaceRole::Subsurface(_) => true,
                _ => false,
            },
            _ => true,
        };
        if valid_role {
            self.role = Some(role);
            Ok(())
        } else {
            Err(format_err!(
                "Attemping to reassign surface role from {:?} to {:?}",
                self.role,
                role
            ))
        }
    }

    pub fn set_parent_and_offset(&mut self, parent: Option<ObjectRef<Surface>>, x: i32, y: i32) {
        self.parent = parent;
        self.offset = Some((x, y));
    }

    pub fn window_geometry(&self) -> Rect {
        if let Some(window_geometry) = self.window_geometry.as_ref() {
            Rect { ..*window_geometry }
        } else {
            Rect { x: 0, y: 0, width: self.size.width, height: self.size.height }
        }
    }

    pub fn offset(&self) -> Option<(i32, i32)> {
        self.offset
    }

    // TODO: Determine correct error handling.
    fn commit_subsurfaces(
        client: &mut Client,
        callbacks: &mut Vec<ObjectRef<Callback>>,
        subsurfaces: &[(ObjectRef<Surface>, Option<ObjectRef<Subsurface>>)],
    ) -> Result<(), Error> {
        ftrace::duration!("wayland", "Surface::commit_subsurfaces");
        for (index, entry) in subsurfaces.iter().enumerate() {
            entry.0.get_mut(client)?.z_order = index;
            if let Some(subsurface_ref) = entry.1 {
                if subsurface_ref.get(client)?.is_sync() {
                    // Get pending commands from subsurface
                    let mut pending_state = subsurface_ref.get_mut(client)?.take_pending_state();
                    let task_queue = client.task_queue();
                    let surface_ref = subsurface_ref.get(client)?.surface();
                    let surface = surface_ref.get_mut(client)?;
                    surface.pending_commands.append(&mut pending_state.0);
                    callbacks.append(&mut pending_state.1);
                    surface.commit_self(task_queue, callbacks)?;
                }
            }
        }

        Ok(())
    }
}

#[cfg(feature = "flatland")]
impl Surface {
    /// Creates a new `Surface`.
    pub fn new(id: wl::ObjectId) -> Self {
        Surface {
            size: Size { width: 0, height: 0 },
            position: (0, 0),
            z_order: 0,
            role: None,
            crop_params: None,
            scale_params: None,
            frame_callbacks: vec![],
            node: None,
            window_geometry: None,
            parent: None,
            offset: None,
            pending_commands: Vec::new(),
            subsurfaces: vec![(id.into(), None)],
            callbacks: VecDeque::new(),
            present_credits: 1,
            image_instance_id: NEXT_IMAGE_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            content: None,
        }
    }

    /// Assigns the Flatland instance for this surface.
    ///
    /// When a surface is initially created, it has no Flatland instance. Since
    /// the instance is used to create the Flatland resources backing the surface,
    /// a wl_surface _must_ have an assigned an instance before it is committed.
    ///
    /// Ex: for xdg_toplevel surfaces, the a new instance will be created for
    /// each toplevel.
    ///
    /// It is an error to call `set_flatland` multiple times for the same
    /// surface.
    pub fn set_flatland(&mut self, flatland: Flatland) -> Result<(), Error> {
        ftrace::duration!("wayland", "Surface::set_flatland");
        if self.node.is_some() {
            Err(format_err!("Changing the Flatland instance for a surface is not supported"))
        } else {
            self.node = Some(SurfaceNode::new(flatland));
            Ok(())
        }
    }

    pub fn clear_flatland(&mut self) {
        self.node = None;
    }

    pub fn flatland(&self) -> Option<Flatland> {
        self.node.as_ref().map(|n| n.flatland.clone())
    }

    /// Returns a reference to the `TransformId` for this surface.
    pub fn transform(&self) -> Option<&TransformId> {
        self.node.as_ref().map(|n| &n.transform_id)
    }

    pub fn next_callbacks(&mut self) -> Option<Vec<ObjectRef<Callback>>> {
        self.callbacks.pop_front()
    }

    /// Updates the current surface state by applying a single `SurfaceCommand`.
    fn apply(&mut self, command: SurfaceCommand) -> Result<(), Error> {
        match command {
            SurfaceCommand::AttachBuffer(attachment) => {
                self.content = Some(attachment.clone());
            }
            SurfaceCommand::ClearBuffer => {}
            SurfaceCommand::Frame(callback) => {
                self.frame_callbacks.push(callback);
            }
            SurfaceCommand::SetViewportCropParams(params) => {
                self.crop_params = Some(params);
            }
            SurfaceCommand::ClearViewportCropParams => {
                self.crop_params = None;
            }
            SurfaceCommand::SetViewportScaleParams(params) => {
                self.scale_params = Some(params);
            }
            SurfaceCommand::ClearViewportScaleParams => {
                self.scale_params = None;
            }
            SurfaceCommand::SetWindowGeometry(geometry) => {
                self.window_geometry = Some(geometry);
            }
            SurfaceCommand::SetPosition(x, y) => {
                self.position = (x, y);
            }
            SurfaceCommand::AddSubsurface(surface_ref, subsurface_ref) => {
                self.subsurfaces.push((surface_ref, Some(subsurface_ref)));
            }
            SurfaceCommand::PlaceSubsurface(params) => {
                let sibling_index = if let Some(index) =
                    self.subsurfaces.iter().position(|x| x.0 == params.sibling)
                {
                    index
                } else {
                    return Err(format_err!("Invalid sibling id {}", params.sibling.id()));
                };
                let sibling_entry = self.subsurfaces.remove(sibling_index);
                let anchor_index = if let Some(index) =
                    self.subsurfaces.iter().position(|x| x.1 == Some(params.subsurface))
                {
                    index
                } else {
                    return Err(format_err!("Invalid subsurface id {}", params.subsurface.id()));
                };

                let new_index = match params.relation {
                    SurfaceRelation::Below => anchor_index,
                    SurfaceRelation::Above => anchor_index + 1,
                };
                self.subsurfaces.insert(new_index, sibling_entry);
            }
        };
        Ok(())
    }

    /// Performs the logic to commit the local state of this surface.
    ///
    /// This will update the scenic Node for this surface.
    fn commit_self(
        &mut self,
        task_queue: TaskQueue,
        callbacks: &mut Vec<ObjectRef<Callback>>,
    ) -> Result<(), Error> {
        ftrace::duration!("wayland", "Surface::commit_self");

        // Save the last buffer ID before applying updates.
        let last_buffer_id = self.content.as_ref().map(|content| content.id());

        let commands = mem::replace(&mut self.pending_commands, Vec::new());
        for command in commands {
            self.apply(command)?;
        }

        let node = match self.node.as_ref() {
            Some(node) => node,
            None => {
                // This is expected for some surfaces that aren't implemented
                // yet, like wl_pointer cursor surfaces.
                println!(
                    "No flatland instance associated with surface role {:?}; skipping commit",
                    self.role
                );
                return Ok(());
            }
        };

        if let Some(content) = &self.content {
            // Acquire image content. The instance ID ensures that usage of buffer by
            // another surface will not conflict with this surface.
            let image_content =
                content.buffer.image_content(self.image_instance_id, &node.flatland);
            self.size = content.buffer.image_size();

            // Set image as content for transform.
            node.flatland
                .proxy()
                .set_content(&mut node.transform_id.clone(), &mut image_content.id.clone())
                .expect("fidl error");

            // Set image sample region based on current crop params.
            let mut sample_region = self.crop_params.map_or(
                RectF {
                    x: 0.0,
                    y: 0.0,
                    width: self.size.width as f32,
                    height: self.size.height as f32,
                },
                |crop| RectF { x: crop.x, y: crop.y, width: crop.width, height: crop.height },
            );
            node.flatland
                .proxy()
                .set_image_sample_region(&mut image_content.id.clone(), &mut sample_region)
                .expect("fidl error");

            // Set destination size based on current scale params.
            let mut destination_size = self.scale_params.map_or(
                SizeU { width: self.size.width as u32, height: self.size.height as u32 },
                |scale| SizeU { width: scale.width as u32, height: scale.height as u32 },
            );
            node.flatland
                .proxy()
                .set_image_destination_size(&mut image_content.id.clone(), &mut destination_size)
                .expect("fidl error");
        }

        // Create and register a release fence to release the last buffer unless
        // it's the same as the current buffer.
        // TODO(fxbug.dev/85402): Track multiple usages of the same buffer and only
        // generate the release event when all usages drop to zero.
        let buffer_id = self.content.as_ref().map(|content| content.id());
        if last_buffer_id != buffer_id {
            if let Some(last_buffer_id) = last_buffer_id {
                let release_fence = zx::Event::create().unwrap();
                node.flatland.add_release_fence(
                    release_fence.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                );
                let task_queue = task_queue.clone();
                fasync::Task::local(async move {
                    let _signals =
                        fasync::OnSignals::new(&release_fence, zx::Signals::EVENT_SIGNALED)
                            .await
                            .unwrap();
                    // Safe to ignore result as EVENT_SIGNALED must have
                    // been observed if we reached this.
                    task_queue.post(move |client| {
                        client.event_queue().post(last_buffer_id, WlBufferEvent::Release)
                    });
                })
                .detach();
            }
        }

        callbacks.append(&mut self.frame_callbacks);

        Ok(())
    }

    pub fn present(
        this: ObjectRef<Self>,
        client: &mut Client,
        mut callbacks: Vec<ObjectRef<Callback>>,
    ) -> Result<(), Error> {
        ftrace::duration!("wayland", "Surface::present");
        if this.get(client)?.present_credits == 0 {
            // Drop frame by adding callbacks to previous frame. There must be at least
            // one set of pending callbacks when we enter this state.
            let surface = this.get_mut(client)?;
            surface.callbacks.back_mut().expect("no pending frame").append(&mut callbacks);
            println!("dropped frame, no present credits remaining");
            return Ok(());
        }
        let flatland = this
            .get(client)?
            .flatland()
            .ok_or(format_err!("Unable to present surface without a flatland instance."))?;
        // Wayland protocol doesn't provide a mechanism to control presentation time
        // so we ask Flatland to present contents immediately by specifying a presentation
        // time of 0.
        flatland.present(0);
        let surface = this.get_mut(client)?;
        surface.callbacks.push_back(callbacks);
        surface.present_credits -= 1;
        Ok(())
    }

    pub fn add_present_credits(&mut self, present_credits: u32) {
        self.present_credits += present_credits;
    }
}

#[cfg(not(feature = "flatland"))]
impl Surface {
    /// Creates a new `Surface`.
    pub fn new(id: wl::ObjectId) -> Self {
        Surface {
            size: Size { width: 0, height: 0 },
            position: (0, 0),
            z_order: 0,
            role: None,
            crop_params: None,
            scale_params: None,
            frame_callbacks: vec![],
            node: None,
            window_geometry: None,
            pixel_scale: (1.0, 1.0),
            parent: None,
            offset: None,
            pending_commands: Vec::new(),
            subsurfaces: vec![(id.into(), None)],
        }
    }

    pub fn set_pixel_scale(&mut self, scale_x: f32, scale_y: f32) {
        self.pixel_scale = (scale_x, scale_y);
        // Reset size to trigger a re-layout.
        self.size = Size { width: 0, height: 0 };
    }

    pub fn pixel_scale(&self) -> (f32, f32) {
        self.pixel_scale
    }

    /// Assigns the scenic session for this surface.
    ///
    /// When a surface is initially created, it has no scenic session. Since
    /// the session is used to create the scenic resources backing the surface,
    /// a wl_surface _must_ have an assigned session before it is committed.
    ///
    /// Ex: for xdg_toplevel surfaces, the a new session will be created for
    /// each toplevel.
    ///
    /// It is an error to call `set_session` multiple times for the same
    /// surface.
    pub fn set_session(&mut self, session: ScenicSession) -> Result<(), Error> {
        ftrace::duration!("wayland", "Surface::set_session");
        if self.node.is_some() {
            Err(format_err!("Chaning the scenic session for a surface is not supported"))
        } else {
            self.node = Some(SurfaceNode::new(session));
            Ok(())
        }
    }

    pub fn clear_session(&mut self) {
        self.node = None;
    }

    pub fn session(&self) -> Option<ScenicSession> {
        self.node.as_ref().map(|n| n.scenic.clone())
    }

    /// Returns a reference to the `scenic::ShapeNode` for this surface.
    pub fn node(&self) -> Option<&scenic::EntityNode> {
        self.node.as_ref().map(|n| &n.entity_node)
    }

    /// Updates the current surface state by applying a single `SurfaceCommand`.
    ///
    /// Returns `true` if the application of the command requires relayout of
    /// the surfaces view hierarchy. Ex: changing the size of the attached
    /// buffer or the associated viewport parameters requires updates to the
    /// scenic nodes, while just replacing the attached buffer requires no such
    /// update.
    fn apply(&mut self, command: SurfaceCommand, task_queue: &TaskQueue) -> Result<bool, Error> {
        let node = match self.node.as_ref() {
            Some(node) => node,
            None => {
                // This is expected for some surfaces that aren't implemented
                // yet, like wl_pointer cursor surfaces.
                println!(
                    "No scenic session associated with surface role {:?}; skipping commit",
                    self.role
                );
                return Ok(false);
            }
        };
        let needs_relayout = match command {
            SurfaceCommand::AttachBuffer(attachment) => {
                let buffer = attachment.buffer.clone();
                let material = scenic::Material::new(node.scenic.as_inner().clone());
                let image3 = buffer.image_resource(&node.scenic.as_inner());
                // Set translucent color to enable alpha blending if the buffer has
                // an alpha channel.
                let alpha = if buffer.has_alpha() { 254 } else { 255 };
                let color =
                    fidl_fuchsia_ui_gfx::ColorRgba { red: 255, green: 255, blue: 255, alpha };
                material.set_color(color);
                material.set_texture_resource(Some(&image3));
                node.surface_node.set_material(&material);
                let previous_size = mem::replace(&mut self.size, buffer.image_size());

                // Create and register a release fence to release this buffer when
                // scenic is done with it.
                let release_fence = zx::Event::create().unwrap();
                node.scenic.as_inner().lock().add_release_fence(
                    release_fence.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                );
                let task_queue = task_queue.clone();
                fasync::Task::local(async move {
                    let _signals =
                        fasync::OnSignals::new(&release_fence, zx::Signals::EVENT_SIGNALED)
                            .await
                            .unwrap();
                    // Safe to ignore result as EVENT_SIGNALED must have
                    // been observed if we reached this.
                    task_queue.post(move |client| {
                        client.event_queue().post(attachment.id(), WlBufferEvent::Release)
                    });
                })
                .detach();
                previous_size != self.size
            }
            SurfaceCommand::ClearBuffer => {
                let material = scenic::Material::new(node.scenic.as_inner().clone());
                material.set_texture(None);
                node.surface_node.set_material(&material);
                self.size = Size { width: 0, height: 0 };
                true
            }
            SurfaceCommand::Frame(callback) => {
                self.frame_callbacks.push(callback);
                false
            }
            SurfaceCommand::SetViewportCropParams(params) => {
                if self.crop_params != Some(params) {
                    self.crop_params = Some(params);
                    true
                } else {
                    false
                }
            }
            SurfaceCommand::ClearViewportCropParams => {
                if self.crop_params != None {
                    self.crop_params = None;
                    true
                } else {
                    false
                }
            }
            SurfaceCommand::SetViewportScaleParams(params) => {
                if self.scale_params != Some(params) {
                    self.scale_params = Some(params);
                    true
                } else {
                    false
                }
            }
            SurfaceCommand::ClearViewportScaleParams => {
                if self.scale_params != None {
                    self.scale_params = None;
                    true
                } else {
                    false
                }
            }
            SurfaceCommand::SetWindowGeometry(geometry) => {
                if self.window_geometry.as_ref() != Some(&geometry) {
                    self.window_geometry = Some(geometry);
                    true
                } else {
                    false
                }
            }
            SurfaceCommand::SetPosition(x, y) => {
                self.position = (x, y);
                true
            }
            SurfaceCommand::AddSubsurface(surface_ref, subsurface_ref) => {
                self.subsurfaces.push((surface_ref, Some(subsurface_ref)));
                false
            }
            SurfaceCommand::PlaceSubsurface(params) => {
                let sibling_index = if let Some(index) =
                    self.subsurfaces.iter().position(|x| x.0 == params.sibling)
                {
                    index
                } else {
                    return Err(format_err!("Invalid sibling id {}", params.sibling.id()));
                };
                let sibling_entry = self.subsurfaces.remove(sibling_index);
                let anchor_index = if let Some(index) =
                    self.subsurfaces.iter().position(|x| x.1 == Some(params.subsurface))
                {
                    index
                } else {
                    return Err(format_err!("Invalid subsurface id {}", params.subsurface.id()));
                };

                let new_index = match params.relation {
                    SurfaceRelation::Below => anchor_index,
                    SurfaceRelation::Above => anchor_index + 1,
                };
                self.subsurfaces.insert(new_index, sibling_entry);
                true
            }
        };
        Ok(needs_relayout)
    }

    /// Performs the logic to commit the local state of this surface.
    ///
    /// This will update the scenic Node for this surface.
    fn commit_self(
        &mut self,
        task_queue: TaskQueue,
        callbacks: &mut Vec<ObjectRef<Callback>>,
    ) -> Result<(), Error> {
        ftrace::duration!("wayland", "Surface::commit_self");
        let mut needs_relayout = false;
        let commands = mem::replace(&mut self.pending_commands, Vec::new());
        for command in commands {
            needs_relayout = self.apply(command, &task_queue)? || needs_relayout;
        }

        if needs_relayout {
            let node = self.node.as_ref().unwrap();
            let (image_width, image_height) = (self.size.width as f32, self.size.height as f32);

            // The size of the clip node. This defaults to the entire surface
            // unless the surface has a wp_viewport set.
            let (mut crop_w, mut crop_h) = (image_width, image_height);

            // The size and position of the surface.
            let (mut surface_x, mut surface_y) = (image_width * 0.5, image_height * 0.5);
            let (mut surface_w, mut surface_h) = (image_width, image_height);

            // Apply any crop parameters if they've been set by the viewport.
            if let Some(crop) = self.crop_params.as_ref() {
                surface_x -= crop.x;
                surface_y -= crop.y;
                crop_w = crop.width;
                crop_h = crop.height;
            }

            // Apply any scale parameters if they've been set by the viewport.
            if let Some(scale) = self.scale_params.as_ref() {
                let scale_x = scale.width as f32 / crop_w;
                let scale_y = scale.height as f32 / crop_h;
                surface_x *= scale_x;
                surface_y *= scale_y;
                surface_w *= scale_x;
                surface_h *= scale_y;
                crop_w = scale.width as f32;
                crop_h = scale.height as f32;
            }

            if let Some(window_geometry) = self.window_geometry.as_ref() {
                let (x, y, w, h) = (
                    window_geometry.x as f32,
                    window_geometry.y as f32,
                    window_geometry.width as f32,
                    window_geometry.height as f32,
                );
                surface_x -= x;
                surface_y -= y;
                if crop_w > w {
                    crop_w = w;
                }
                if crop_h > h {
                    crop_h = h;
                }
            }

            // The clip_node will be the viewport though which we'll view the
            // backing surface.
            node.clip_node.set_shape(&scenic::Rectangle::new(
                node.scenic.as_inner().clone(),
                crop_w,
                crop_h,
            ));
            // TODO(64996): This is now illegal, we need to instead use the SetClipPlanes command.
            //node.clip_node.set_translation(crop_w * 0.5, crop_h * 0.5, std::f32::INFINITY);

            // Position & scale the surface so that the viewport src rect aligns
            // with the clip node.
            node.surface_node.set_shape(&scenic::Rectangle::new(
                node.scenic.as_inner().clone(),
                surface_w,
                surface_h,
            ));
            node.surface_node.set_translation(surface_x, surface_y, 0.0);

            // We scale our z such that all our subsurfaces will be within the
            // range 0.0 - 1.0, which prevents our subsurfaces from being
            // ordered incorrectly WRT other surfaces subsurfaces.
            node.entity_node.set_scale(
                1.0 / self.pixel_scale.0,
                1.0 / self.pixel_scale.1,
                1.0 / self.subsurfaces.len() as f32,
            );
            node.entity_node.set_translation(
                self.position.0 as f32 / self.pixel_scale.0,
                self.position.1 as f32 / self.pixel_scale.1,
                -(self.z_order as f32),
            );
        }
        callbacks.append(&mut self.frame_callbacks);
        Ok(())
    }

    pub fn present(
        this: ObjectRef<Self>,
        client: &mut Client,
        callbacks: Vec<ObjectRef<Callback>>,
    ) -> Result<(), Error> {
        ftrace::duration!("wayland", "Surface::present");
        if let Ok(surface) = this.get(client) {
            let task_queue = client.task_queue();
            let session = surface
                .session()
                .ok_or(format_err!("Unable to present surface without a session."))?;
            fasync::Task::local(
                session
                    .present(0)
                    .map_ok(move |info| {
                        ftrace::duration!("wayland", "XdgToplevelView::present_callback");
                        ftrace::flow_end!("gfx", "present_callback", info.presentation_time);
                        if !callbacks.is_empty() {
                            // If we have a frame callback, invoke it and provide
                            // the presentation time received in the present
                            // callback.
                            task_queue.post(move |client| {
                                // If the underlying surface has been destroyed then
                                // skip sending the done event.
                                if this.get(client).is_ok() {
                                    callbacks.iter().try_for_each(|callback| {
                                        let time_ms = (info.presentation_time / 1_000_000) as u32;
                                        Callback::done(*callback, client, time_ms)
                                    })?;
                                }
                                callbacks
                                    .iter()
                                    .try_for_each(|callback| client.delete_id(callback.id()))
                            });
                        }
                    })
                    .unwrap_or_else(|e| eprintln!("present error: {:?}", e)),
            )
            .detach();
        }
        Ok(())
    }

    pub fn hit_test(
        &self,
        x: f32,
        y: f32,
        client: &Client,
    ) -> Option<(ObjectRef<Self>, (i32, i32))> {
        // Iterate over subsurfaces, starting with the top-most surface.
        for (surface_ref, _) in self.subsurfaces.iter().rev() {
            if let Ok(surface) = surface_ref.get(client) {
                let (x1, y1, x2, y2) = {
                    let geometry = surface.window_geometry();
                    (
                        surface.position.0,
                        surface.position.1,
                        surface.position.0 + geometry.width,
                        surface.position.1 + geometry.height,
                    )
                };
                if x >= x1 as f32 && y >= y1 as f32 && x < x2 as f32 && y < y2 as f32 {
                    return Some((*surface_ref, surface.position));
                }
            }
        }

        None
    }
}

impl RequestReceiver<WlSurface> for Surface {
    fn receive(
        this: ObjectRef<Self>,
        request: WlSurfaceRequest,
        client: &mut Client,
    ) -> Result<(), Error> {
        match request {
            WlSurfaceRequest::Destroy => {
                client.input_dispatcher.clear_focus_on_surface_destroy(this);
                client.delete_id(this.id())?;
            }
            WlSurfaceRequest::Attach { buffer, .. } => {
                if buffer == 0 {
                    this.get_mut(client)?.enqueue(SurfaceCommand::ClearBuffer);
                } else {
                    let attachment = BufferAttachment {
                        buffer_id: buffer,
                        buffer: client.get_object::<Buffer>(buffer)?.clone(),
                    };
                    this.get_mut(client)?.enqueue(SurfaceCommand::AttachBuffer(attachment));
                }
            }
            WlSurfaceRequest::Frame { callback } => {
                let callback = callback.implement(client, Callback)?;
                this.get_mut(client)?.enqueue(SurfaceCommand::Frame(callback));
            }
            WlSurfaceRequest::Commit => {
                let mut callbacks = Vec::new();
                let role = {
                    if let Some(SurfaceRole::Subsurface(subsurface_ref)) = this.get(client)?.role {
                        if subsurface_ref.get(client)?.is_sync() {
                            // We're a sync subsurface. We don't want to commit
                            // self yet. Rather we extract the current pending
                            // commands and defer them to be applied when our
                            // parent is committed.
                            let commands = {
                                let surface = this.get_mut(client)?;
                                mem::replace(&mut surface.pending_commands, Vec::new())
                            };
                            subsurface_ref.get_mut(client)?.add_pending_commands(commands);
                            return Ok(());
                        }
                    }

                    // If we're not a sync subsurface, we proceed with committing our
                    // state.
                    let task_queue = client.task_queue();
                    let surface = this.get_mut(client)?;
                    surface.commit_self(task_queue.clone(), &mut callbacks)?;
                    surface.role
                };

                // We're applying our state so we need to apply any state associated
                // with sync subsurfaces.
                {
                    // We briefly extract the subsurface vector from the surface
                    // to allow us to iterate over the subsurfaces to commit.
                    // We need to perform some mutable operations here (changing
                    // z-index), so this is safe as long as no new subsurfaces
                    // are added. That should never happen, and we assert that
                    // the subsurface vector is indeed empty when we re-insert
                    // the subsurface vector back into the surface.
                    let subsurfaces =
                        mem::replace(&mut this.get_mut(client)?.subsurfaces, Vec::new());
                    let result =
                        Self::commit_subsurfaces(client, &mut callbacks, subsurfaces.as_slice());
                    let subsurfaces =
                        mem::replace(&mut this.get_mut(client)?.subsurfaces, subsurfaces);
                    assert!(subsurfaces.is_empty());
                    result?;
                }

                // Notify the role objects that there's been a commit. This hook will
                // return a boolean indicating if the role needs a present. For example,
                // an xdg_toplevel will need a Present to get its newly updated state
                // onto the screen, but a sync wl_subsurface wants to defer a present
                // until its parent state is committed.
                let needs_present = role
                    .map(|role| role.finalize_commit(client, &mut callbacks))
                    .unwrap_or(Ok(false))?;

                // We trigger a present if explicitly requested of if there are any
                // remaining frame callbacks.
                if needs_present || !callbacks.is_empty() {
                    Self::present(this, client, callbacks)?;
                }
            }
            WlSurfaceRequest::Damage { .. } => {}
            WlSurfaceRequest::SetOpaqueRegion { .. } => {}
            WlSurfaceRequest::SetInputRegion { .. } => {}
            WlSurfaceRequest::SetBufferTransform { .. } => {}
            WlSurfaceRequest::SetBufferScale { .. } => {}
            WlSurfaceRequest::DamageBuffer { .. } => {}
        }
        Ok(())
    }
}

/// `SurfaceRole` holds the set of every role that can be assigned to a
/// wl_surface. Each variant will hold an `ObjectRef` to the role object.
#[derive(Copy, Clone, Debug)]
pub enum SurfaceRole {
    /// The surface is an xdg_surface. Note that xdg_surface isn't a role
    /// itself, but instead maps to sub-roles (ex: xdg_toplevel). We'll let
    /// the `XdgSurface` handle the xdg sub-roles, however.
    XdgSurface(ObjectRef<XdgSurface>),
    Subsurface(ObjectRef<Subsurface>),
}

impl SurfaceRole {
    /// Dispatches a commit command to the concrete role objects.
    fn finalize_commit(
        &self,
        client: &mut Client,
        callbacks: &mut Vec<ObjectRef<Callback>>,
    ) -> Result<bool, Error> {
        ftrace::duration!("wayland", "SurfaceRole::commit");
        match self {
            SurfaceRole::XdgSurface(xdg_surface_ref) => {
                XdgSurface::finalize_commit(*xdg_surface_ref, client)
            }
            SurfaceRole::Subsurface(subsurface_ref) => {
                Ok(subsurface_ref.get_mut(client)?.finalize_commit(callbacks))
            }
        }
    }
}

/// A `BufferAttachment` holds the state of the attached buffer to a `Surface`.
///
/// This amount to the set of arguments to the most recently received
/// `wl_surface::attach` request.
#[derive(Clone)]
pub struct BufferAttachment {
    pub buffer_id: wl::ObjectId,
    /// The buffer object.
    pub buffer: Buffer,
    // TODO(tjdetwiler): Add x, y parameters from wl_surface::attach.
}

impl BufferAttachment {
    pub fn id(&self) -> wl::ObjectId {
        self.buffer_id
    }
}

struct Region;

impl RequestReceiver<WlRegion> for Region {
    fn receive(
        this: ObjectRef<Self>,
        request: WlRegionRequest,
        client: &mut Client,
    ) -> Result<(), Error> {
        match request {
            WlRegionRequest::Destroy => {
                client.delete_id(this.id())?;
            }
            WlRegionRequest::Add { .. } => {}
            WlRegionRequest::Subtract { .. } => {}
        }
        Ok(())
    }
}
