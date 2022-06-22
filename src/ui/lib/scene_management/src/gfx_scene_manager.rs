// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::display_metrics::{DisplayMetrics, ViewingDistance},
    crate::graphics_utils::{ImageResource, ScreenCoordinates, ScreenSize},
    crate::pointerinjector_config::{
        InjectorViewportHangingGet, InjectorViewportPublisher, InjectorViewportSpec,
        InjectorViewportSubscriber,
    },
    crate::scene_manager::{self, PresentationMessage, PresentationSender, SceneManager},
    anyhow::Error,
    async_trait::async_trait,
    fidl,
    fidl_fuchsia_accessibility::{MagnificationHandlerRequest, MagnificationHandlerRequestStream},
    fidl_fuchsia_ui_app as ui_app, fidl_fuchsia_ui_gfx as ui_gfx,
    fidl_fuchsia_ui_scenic as ui_scenic, fidl_fuchsia_ui_views as ui_views,
    fuchsia_async as fasync, fuchsia_scenic as scenic, fuchsia_scenic,
    fuchsia_syslog::{fx_log_err, fx_log_info, fx_log_warn},
    futures::channel::mpsc::unbounded,
    futures::channel::oneshot,
    futures::TryStreamExt,
    input_pipeline::Size,
    parking_lot::Mutex,
    std::sync::{Arc, Weak},
};

pub type FocuserPtr = Arc<ui_views::FocuserProxy>;
pub type ViewRefInstalledPtr = Arc<ui_views::ViewRefInstalledProxy>;

/// The [`GfxSceneManager`] constructs an empty scene with a single white ambient light.
///
/// Each added view is positioned at (x, y, z) = 0, and sized to match the size of the display.
/// The display dimensions are computed at the time the [`GfxSceneManager`] is constructed.
pub struct GfxSceneManager {
    /// The ViewRefInstalled handle used to ensure that the root view is reattached to the scene
    /// after a11y view insertion.
    pub view_ref_installed: ViewRefInstalledPtr,

    /// The Scenic session associated with this [`GfxSceneManager`].
    pub session: scenic::SessionPtr,

    /// Presentation sender used to request presents for the root session.
    presentation_sender: PresentationSender,

    /// The view focuser associated with the [`session`].
    pub focuser: FocuserPtr,

    /// The id of the compositor used for the scene's layer stack.
    pub compositor_id: u32,

    /// The root node of the scene. Views & the cursor are added as children of this node.
    pub root_node: scenic::EntityNode,

    /// The size of the display, as determined when [`GfxSceneManager::new()`] was called.
    pub display_size: ScreenSize,

    /// The metrics for the display presenting the scene.
    pub display_metrics: DisplayMetrics,

    /// The camera of the scene.
    camera: scenic::Camera,

    // State of the camera clip space transform.
    clip_scale: f32,
    clip_offset_x: f32,
    clip_offset_y: f32,

    /// Scene topology:
    ///
    /// scene
    ///   |
    /// root_node
    ///   |
    /// global_root_view_holder
    ///   |
    /// global_root_view
    ///   |
    /// pointerinjector_view_holder
    ///   |
    /// pointerinjector_view
    ///   |
    /// a11y_proxy_view_holder*
    ///   |
    /// a11y_proxy_view
    ///   |
    /// client_root_view_holder_node
    ///
    ///
    /// *This represents the state when GfxSceneManager is first created, before
    /// the a11y view is inserted. After `insert_a11y_view` is called,
    /// a11y_proxy_view_holder will be removed, and the new topology will look
    /// like:
    ///  ...
    ///   |
    /// pointerinjector_view
    ///   |
    /// a11y_view_holder
    ///   |
    /// a11y_view (owned & created by a11y manager)
    ///   |
    /// new_a11y_proxy_view_holder (owned & created by a11y manager)
    ///   |
    /// a11y_proxy_view
    ///   |
    /// client_root_view_holder_node

    /// The root view of Scene Manager.
    /// This view is always static. It's used as the 'source' view when injecting events through
    /// fuchsia::ui::pointerinjector.
    _root_view_holder: scenic::ViewHolder,
    _root_view: scenic::View,

    /// The pointerinjector view is used as the 'target' view when injecting events through
    /// fuchsia::ui::pointerinjector.
    /// See also: https://cs.opensource.google/fuchsia/fuchsia/+/main:src/ui/a11y/docs/accessibility_view.md
    /// TODO(fxbug.dev/100980) This is where scale, rotation and translation for all child views
    /// should be set.
    _pointerinjector_view_holder: scenic::ViewHolder,
    pointerinjector_view: scenic::View,
    pointerinjector_session: scenic::SessionPtr,
    pointerinjector_presentation_sender: PresentationSender,

    /// The proxy View/ViewHolder pair exists so that the a11y manager can insert its view into the
    /// scene after SetRootView() has already been called.
    a11y_proxy_view_holder: scenic::ViewHolder,
    a11y_proxy_view: scenic::View,
    a11y_proxy_session: scenic::SessionPtr,
    a11y_proxy_presentation_sender: PresentationSender,

    /// Holds the view holder [`scenic::EntityNode`] (if `set_root_view` has been called).
    client_root_view_holder_node: Option<scenic::EntityNode>,

    /// Holds a duplicate ref to the view added in `set_root_view`.
    /// We need this so that if the a11y view is inserted we'll re-focus this view afterwards.
    client_root_view_ref: Option<ui_views::ViewRef>,

    /// Supports callers of fuchsia.ui.pointerinjector.configuration.setup.WatchViewport(), allowing
    /// each invocation to subscribe to changes in the viewport region.
    viewport_hanging_get: Arc<Mutex<InjectorViewportHangingGet>>,

    // Used to publish viewport changes to subscribers of |viewport_hanging_get|.
    // TODO(fxbug.dev/87517): use this to publish changes to screen resolution.
    viewport_publisher: Arc<Mutex<InjectorViewportPublisher>>,

    /// These are the ViewRefs returned by get_pointerinjection_view_refs().  They are used to
    /// configure input-pipeline handlers for pointer events.
    context_view_ref: ui_views::ViewRef,
    target_view_ref: ui_views::ViewRef,

    /// The node for the cursor. It is optional in case a scene doesn't render a cursor.
    cursor_node: Option<scenic::EntityNode>,

    /// The shapenode for the cursor. It is optional in case a scene doesn't render a cursor.
    cursor_shape: Option<scenic::ShapeNode>,

    /// The resources used to construct the scene. If these are dropped, they will be removed
    /// from Scenic, so they must be kept alive for the lifetime of `GfxSceneManager`.
    _resources: ScenicResources,
}

/// A struct containing all the Scenic resources which are unused but still need to be kept alive.
/// If the resources are dropped, they are automatically removed from the Scenic session.
struct ScenicResources {
    _ambient_light: scenic::AmbientLight,
    _compositor: scenic::DisplayCompositor,
    _layer: scenic::Layer,
    _layer_stack: scenic::LayerStack,
    _renderer: scenic::Renderer,
    _scene: scenic::Scene,
}

#[async_trait]
impl SceneManager for GfxSceneManager {
    async fn set_root_view(
        &mut self,
        view_provider: ui_app::ViewProviderProxy,
    ) -> Result<ui_views::ViewRef, Error> {
        if self.client_root_view_holder_node.is_some() {
            panic!("GFX set_root_view doesn't support replacing the current root view");
        };

        let token_pair = scenic::ViewTokenPair::new()?;
        let mut viewref_pair = scenic::ViewRefPair::new()?;

        // Make two additional copies of the ViewRef.
        // - The original will be used to create the root view.
        // - The first copy will be returned to the caller.
        // - The second copy will be stored, and used to re-focus the root view if insertion
        //   of the a11y view breaks the focus chain.
        let viewref_dup = fuchsia_scenic::duplicate_view_ref(&viewref_pair.view_ref)?;
        self.client_root_view_ref =
            Some(fuchsia_scenic::duplicate_view_ref(&viewref_pair.view_ref)?);

        view_provider.create_view_with_view_ref(
            token_pair.view_token.value,
            &mut viewref_pair.control_ref,
            &mut viewref_pair.view_ref,
        )?;

        let view_holder = GfxSceneManager::create_view_holder(
            &self.a11y_proxy_session,
            token_pair.view_holder_token,
            self.display_metrics,
            Some("root".to_string()),
        );

        let view_holder_node = scenic::EntityNode::new(self.a11y_proxy_session.clone());
        view_holder_node.attach(&view_holder);
        view_holder_node.set_translation(0.0, 0.0, 0.0);

        self.a11y_proxy_view.add_child(&view_holder_node);
        self.client_root_view_holder_node = Some(view_holder_node);
        GfxSceneManager::request_present(&self.a11y_proxy_presentation_sender);

        Ok(viewref_dup)
    }

    fn request_focus(
        &self,
        view_ref: &mut ui_views::ViewRef,
    ) -> fidl::client::QueryResponseFut<ui_views::FocuserRequestFocusResult> {
        self.focuser.request_focus(view_ref)
    }

    /// Creates an a11y view holder and attaches it to the scene. This method also deletes the
    /// existing proxy view/viewholder pair, and creates a new proxy view. It then returns the
    /// new proxy view holder token. The a11y manager is responsible for using this token to
    /// create the new proxy view holder.
    ///
    /// # Parameters
    /// - `a11y_view_ref`: The view ref of the a11y view.
    /// - `a11y_view_holder_token`: The token used to create the a11y view holder.
    fn insert_a11y_view(
        &mut self,
        a11y_view_holder_token: ui_views::ViewHolderToken,
    ) -> Result<ui_views::ViewHolderToken, Error> {
        // Create the new a11y view holder, and attach it as a child of the pointerinjector view.
        let a11y_view_holder = GfxSceneManager::create_view_holder(
            &self.pointerinjector_session,
            a11y_view_holder_token,
            self.display_metrics,
            Some(String::from("a11y view holder")),
        );
        self.pointerinjector_view.add_child(&a11y_view_holder);

        // Disconnect the old proxy view/viewholder from the scene graph.
        self.a11y_proxy_view_holder.detach();
        if let Some(ref view_holder_node) = self.client_root_view_holder_node {
            self.a11y_proxy_view.detach_child(&*view_holder_node);
        }

        // Generate a new proxy view/viewholder token pair, and create a new proxy view.
        // Save the proxy ViewRef so that we can observe when the view is attached to the scene.
        let proxy_token_pair = scenic::ViewTokenPair::new()?;
        let a11y_proxy_view_ref_pair = scenic::ViewRefPair::new()?;
        let a11y_proxy_view_ref =
            fuchsia_scenic::duplicate_view_ref(&a11y_proxy_view_ref_pair.view_ref)?;
        self.a11y_proxy_view = scenic::View::new3(
            self.a11y_proxy_session.clone(),
            proxy_token_pair.view_token,
            a11y_proxy_view_ref_pair.control_ref,
            a11y_proxy_view_ref_pair.view_ref,
            Some(String::from("a11y proxy view")),
        );

        // Reconnect existing view holders to the new a11y proxy view.
        if let Some(ref view_holder_node) = self.client_root_view_holder_node {
            self.a11y_proxy_view.add_child(&*view_holder_node);
        }

        GfxSceneManager::request_present(&self.pointerinjector_presentation_sender);
        GfxSceneManager::request_present(&self.a11y_proxy_presentation_sender);

        // If the root view was already set, inserting the a11y view will have broken the focus
        // chain. In this case, we need to re-focus the root view.
        let view_ref_installed = Arc::downgrade(&self.view_ref_installed);
        let focuser = Arc::downgrade(&self.focuser);
        if let Some(ref client_root_view_ref) = self.client_root_view_ref {
            let client_root_view_ref_dup =
                fuchsia_scenic::duplicate_view_ref(&client_root_view_ref)?;
            fasync::Task::local(async move {
                GfxSceneManager::focus_client_root_view(
                    view_ref_installed,
                    focuser,
                    client_root_view_ref_dup,
                    a11y_proxy_view_ref,
                )
                .await;
            })
            .detach();
        }

        Ok(proxy_token_pair.view_holder_token)
    }

    fn insert_a11y_view2(
        &mut self,
        _a11y_viewport_creation_token: ui_views::ViewportCreationToken,
    ) -> Result<ui_views::ViewportCreationToken, Error> {
        Err(anyhow::anyhow!("A11y should be configured to use Gfx, not Flatland"))
    }

    async fn set_camera_clip_space_transform(&mut self, x: f32, y: f32, scale: f32) {
        self.camera.set_camera_clip_space_transform(x, y, scale);
        self.clip_offset_x = x;
        self.clip_offset_y = y;
        self.clip_scale = scale;

        GfxSceneManager::request_present_and_await_next_frame(&self.presentation_sender).await;
        self.update_viewport().await;
    }

    async fn reset_camera_clip_space_transform(&mut self) {
        self.set_camera_clip_space_transform(0.0, 0.0, 1.0).await;
    }

    fn get_pointerinjection_view_refs(&self) -> (ui_views::ViewRef, ui_views::ViewRef) {
        (
            scenic::duplicate_view_ref(&self.context_view_ref).expect("failed to copy ViewRef"),
            scenic::duplicate_view_ref(&self.target_view_ref).expect("failed to copy ViewRef"),
        )
    }

    fn set_cursor_position(&mut self, position: input_pipeline::Position) {
        let location = ScreenCoordinates::from_pixels(position.x, position.y, self.display_metrics);
        if self.cursor_node.is_none() {
            // We don't already have a cursor node so let's make one with the default cursor
            if let Err(error) = self.set_cursor_image("/pkg/data/cursor.png") {
                fx_log_warn!("Failed to load image cursor: {:?}", error);
                self.set_cursor_shape(self.get_default_cursor());
            }
        }

        let (x, y) = location.pips();
        self.cursor_node().set_translation(x, y, GfxSceneManager::CURSOR_DEPTH);
        GfxSceneManager::request_present(&self.presentation_sender);
    }

    fn set_cursor_visibility(&mut self, visible: bool) {
        if let Some(shape) = self.cursor_shape.as_ref() {
            // Safe to unwrap as cursor shape can only exist if there is a cursor node.
            let node = self.cursor_node.as_ref().unwrap();
            if visible {
                node.add_child(shape);
            } else {
                node.remove_child(shape);
            }
            GfxSceneManager::request_present(&self.presentation_sender);
        }
    }

    fn get_pointerinjection_display_size(&self) -> Size {
        let (width_pixels, height_pixels) = self.display_size.pixels();
        Size { width: width_pixels, height: height_pixels }
    }

    fn get_pointerinjector_viewport_watcher_subscription(&self) -> InjectorViewportSubscriber {
        self.viewport_hanging_get.lock().new_subscriber()
    }

    fn get_display_metrics(&self) -> &DisplayMetrics {
        &self.display_metrics
    }
}

impl GfxSceneManager {
    /// The depth of the bounds of any added views. This can be used to compute where a view
    /// should be placed to render "in front of" another view.
    const VIEW_BOUNDS_DEPTH: f32 = -800.0;
    /// The depth at which to draw the cursor in order to ensure it's on top of everything else
    const CURSOR_DEPTH: f32 = GfxSceneManager::VIEW_BOUNDS_DEPTH - 1.0;

    /// Creates a new SceneManager.
    ///
    /// # Errors
    /// Returns an error if a Scenic session could not be initialized, or the scene setup fails.
    pub async fn new(
        scenic: ui_scenic::ScenicProxy,
        view_ref_installed_proxy: ui_views::ViewRefInstalledProxy,
        display_pixel_density: Option<f32>,
        viewing_distance: Option<ViewingDistance>,
    ) -> Result<Self, Error> {
        let view_ref_installed = Arc::new(view_ref_installed_proxy);

        let (session, focuser) = GfxSceneManager::create_session(&scenic)?;

        let ambient_light = GfxSceneManager::create_ambient_light(&session);
        let scene = GfxSceneManager::create_ambiently_lit_scene(&session, &ambient_light);

        let camera = scenic::Camera::new(session.clone(), &scene);
        let renderer = GfxSceneManager::create_renderer(&session, &camera);

        // Size the layer to fit the size of the display.
        let display_info = scenic.get_display_info().await?;

        let size_in_pixels = Size {
            width: display_info.width_in_px as f32,
            height: display_info.height_in_px as f32,
        };

        let display_metrics =
            DisplayMetrics::new(size_in_pixels, display_pixel_density, viewing_distance, None);

        scene.set_scale(display_metrics.pixels_per_pip(), display_metrics.pixels_per_pip(), 1.0);

        let layer = GfxSceneManager::create_layer(&session, &renderer, size_in_pixels);
        let layer_stack = GfxSceneManager::create_layer_stack(&session, &layer);
        let compositor = GfxSceneManager::create_compositor(&session, &layer_stack);

        // Add the root node to the scene immediately.
        let root_node = scenic::EntityNode::new(session.clone());

        scene.add_child(&root_node);

        // Create pointer injector view/viewholder and add to the scene.
        let root_view_token_pair = scenic::ViewTokenPair::new()?;
        let root_viewref_pair = scenic::ViewRefPair::new()?;
        let root_view_holder = GfxSceneManager::create_view_holder(
            &session,
            root_view_token_pair.view_holder_token,
            display_metrics,
            Some(String::from("root view holder")),
        );

        let context_view_ref = scenic::duplicate_view_ref(&root_viewref_pair.view_ref)?;

        let root_view = scenic::View::new3(
            session.clone(),
            root_view_token_pair.view_token,
            root_viewref_pair.control_ref,
            root_viewref_pair.view_ref,
            Some(String::from("root_view view")),
        );

        root_node.add_child(&root_view_holder);

        // Create pointer injector view/viewholder and add to the scene.
        let pointerinjector_token_pair = scenic::ViewTokenPair::new()?;
        let pointerinjector_viewref_pair = scenic::ViewRefPair::new()?;
        let pointerinjector_view_holder = GfxSceneManager::create_view_holder(
            &session,
            pointerinjector_token_pair.view_holder_token,
            display_metrics,
            Some(String::from("pointerinjector view holder")),
        );
        let (pointerinjector_session, _pointerinjector_focuser) =
            GfxSceneManager::create_session(&scenic)?;

        let target_view_ref = scenic::duplicate_view_ref(&pointerinjector_viewref_pair.view_ref)?;

        let pointerinjector_view = scenic::View::new3(
            pointerinjector_session.clone(),
            pointerinjector_token_pair.view_token,
            pointerinjector_viewref_pair.control_ref,
            pointerinjector_viewref_pair.view_ref,
            Some(String::from("pointerinjector view")),
        );
        root_view.add_child(&pointerinjector_view_holder);

        // Create a11y proxy view/viewholder and add to the scene.
        let a11y_proxy_token_pair = scenic::ViewTokenPair::new()?;
        let a11y_proxy_viewref_pair = scenic::ViewRefPair::new()?;
        let a11y_proxy_view_holder = GfxSceneManager::create_view_holder(
            &pointerinjector_session,
            a11y_proxy_token_pair.view_holder_token,
            display_metrics,
            Some(String::from("a11y proxy view holder")),
        );
        let (a11y_proxy_session, _a11y_proxy_focuser) = GfxSceneManager::create_session(&scenic)?;

        let a11y_proxy_view = scenic::View::new3(
            a11y_proxy_session.clone(),
            a11y_proxy_token_pair.view_token,
            a11y_proxy_viewref_pair.control_ref,
            a11y_proxy_viewref_pair.view_ref,
            Some(String::from("a11y proxy view")),
        );
        pointerinjector_view.add_child(&a11y_proxy_view_holder);

        let viewport_hanging_get: Arc<Mutex<InjectorViewportHangingGet>> =
            scene_manager::create_viewport_hanging_get(InjectorViewportSpec {
                width: size_in_pixels.width,
                height: size_in_pixels.height,
                scale: 1.,
                x_offset: 0.,
                y_offset: 0.,
            });

        let viewport_publisher = Arc::new(Mutex::new(viewport_hanging_get.lock().new_publisher()));

        let compositor_id = compositor.id();

        let resources = ScenicResources {
            _ambient_light: ambient_light,
            _compositor: compositor,
            _layer: layer,
            _layer_stack: layer_stack,
            _renderer: renderer,
            _scene: scene,
        };

        let (sender, receiver) = unbounded();
        scene_manager::start_presentation_loop(sender.clone(), receiver, Arc::downgrade(&session));
        GfxSceneManager::request_present(&sender);

        let (pointerinjector_sender, pointerinjector_receiver) = unbounded();
        scene_manager::start_presentation_loop(
            pointerinjector_sender.clone(),
            pointerinjector_receiver,
            Arc::downgrade(&pointerinjector_session),
        );
        GfxSceneManager::request_present(&pointerinjector_sender);

        let (a11y_proxy_sender, a11y_proxy_receiver) = unbounded();
        scene_manager::start_presentation_loop(
            a11y_proxy_sender.clone(),
            a11y_proxy_receiver,
            Arc::downgrade(&a11y_proxy_session),
        );
        GfxSceneManager::request_present(&a11y_proxy_sender);

        Ok(GfxSceneManager {
            view_ref_installed,
            session,
            presentation_sender: sender,
            focuser,
            compositor_id,
            root_node,
            display_size: ScreenSize::from_size(&size_in_pixels, display_metrics),
            camera,
            clip_scale: 1.,
            clip_offset_x: 0.,
            clip_offset_y: 0.,
            _root_view_holder: root_view_holder,
            _root_view: root_view,
            _pointerinjector_view_holder: pointerinjector_view_holder,
            pointerinjector_view,
            pointerinjector_session,
            pointerinjector_presentation_sender: pointerinjector_sender,
            a11y_proxy_view_holder,
            a11y_proxy_view,
            a11y_proxy_session,
            a11y_proxy_presentation_sender: a11y_proxy_sender,
            client_root_view_holder_node: None,
            client_root_view_ref: None,
            viewport_hanging_get,
            viewport_publisher: viewport_publisher,
            context_view_ref,
            target_view_ref,
            display_metrics,
            cursor_node: None,
            cursor_shape: None,
            _resources: resources,
        })
    }

    pub fn handle_magnification_handler_request_stream(
        mut request_stream: MagnificationHandlerRequestStream,
        scene_manager: Arc<futures::lock::Mutex<Box<dyn SceneManager>>>,
    ) {
        fasync::Task::local(async move {
            loop {
                let request = request_stream.try_next().await;
                match request {
                    Ok(Some(MagnificationHandlerRequest::SetClipSpaceTransform {
                        x,
                        y,
                        scale,
                        responder,
                    })) => {
                        { scene_manager.lock().await.set_camera_clip_space_transform(x, y, scale) }
                            .await;
                        if let Err(e) = responder.send() {
                            fx_log_warn!(
                                "Failed to send MagnificationHandlerRequest() response: {}",
                                e
                            );
                        }
                    }
                    Ok(None) => {
                        return;
                    }
                    Err(e) => {
                        { scene_manager.lock().await.reset_camera_clip_space_transform() }.await;
                        fx_log_err!("Error obtaining MagnificationHandlerRequest: {}", e);
                        return;
                    }
                }
            }
        })
        .detach()
    }

    /// Creates a new Scenic session.
    ///
    /// # Parameters
    /// - `scenic`: The [`ScenicProxy`] which is used to create the session.
    ///
    /// # Errors
    /// If the [`scenic::SessionPtr`] could not be created.
    fn create_session(
        scenic: &ui_scenic::ScenicProxy,
    ) -> Result<(scenic::SessionPtr, FocuserPtr), Error> {
        let (session_proxy, session_request_stream) = fidl::endpoints::create_proxy()?;
        let (focuser_proxy, focuser_request_stream) = fidl::endpoints::create_proxy()?;
        scenic.create_session2(session_request_stream, None, Some(focuser_request_stream))?;

        Ok((scenic::Session::new(session_proxy), Arc::new(focuser_proxy)))
    }

    /// Creates a scene with the given ambient light.
    ///
    /// # Parameters
    /// - `session`: The Scenic session to create the scene in.
    /// - `light`: The [`scenic::AmbientLight`] which is added to the created [`scenic::Scene`].
    fn create_ambiently_lit_scene(
        session: &scenic::SessionPtr,
        light: &scenic::AmbientLight,
    ) -> scenic::Scene {
        let scene = scenic::Scene::new(session.clone());
        scene.add_ambient_light(&light);

        scene
    }

    /// Creates a new ambient light for the [`GfxSceneManager`]'s scene.
    ///
    /// # Parameters
    /// - `session`: The Scenic session to create the light in.
    fn create_ambient_light(session: &scenic::SessionPtr) -> scenic::AmbientLight {
        let ambient_light = scenic::AmbientLight::new(session.clone());
        ambient_light.set_color(ui_gfx::ColorRgb { red: 1.0, green: 1.0, blue: 1.0 });

        ambient_light
    }

    /// Creates a renderer in the given session.
    ///
    /// # Parameters
    /// - `session`: The Scenic session to create the renderer in.
    /// - `camera`: The camera to use for the renderer.
    fn create_renderer(session: &scenic::SessionPtr, camera: &scenic::Camera) -> scenic::Renderer {
        let renderer = scenic::Renderer::new(session.clone());
        renderer.set_camera(camera);

        renderer
    }

    /// Creates a new layer.
    ///
    /// # Parameters
    /// - `session`: The Scenic session to create the layer in.
    /// - `renderer`: The renderer for the layer.
    /// - `display_size`: The size of the display in pixels.
    fn create_layer(
        session: &scenic::SessionPtr,
        renderer: &scenic::Renderer,
        display_size: Size,
    ) -> scenic::Layer {
        let layer = scenic::Layer::new(session.clone());
        layer.set_size(display_size.width, display_size.height);
        layer.set_renderer(&renderer);

        layer
    }

    /// Creates a new layer stack with one layer.
    ///
    /// # Parameters
    /// - `session`: The Scenic session to create the layer stack in.
    /// - `layer`: The layer to add to the layer stack.
    fn create_layer_stack(
        session: &scenic::SessionPtr,
        layer: &scenic::Layer,
    ) -> scenic::LayerStack {
        let layer_stack = scenic::LayerStack::new(session.clone());
        layer_stack.add_layer(&layer);

        layer_stack
    }

    /// Creates a new compositor.
    ///
    /// # Parameters
    /// - `session`: The Scenic session to create the compositor in.
    /// - `layer_stack`: The layer stack to composite.
    fn create_compositor(
        session: &scenic::SessionPtr,
        layer_stack: &scenic::LayerStack,
    ) -> scenic::DisplayCompositor {
        let compositor = scenic::DisplayCompositor::new(session.clone());
        compositor.set_layer_stack(&layer_stack);

        compositor
    }

    /// Creates a view holder in the supplied session using the provided token and display metrics.
    ///
    /// # Parameters
    /// - `session`: The scenic session in which to create the view holder.
    /// - `view_holder_token`: The view holder token used to create the view holder.
    /// - `display_metrics`: The metrics for the display presenting the scene.
    /// - `name`: The debug name of the view holder.
    fn create_view_holder(
        session: &scenic::SessionPtr,
        view_holder_token: ui_views::ViewHolderToken,
        display_metrics: DisplayMetrics,
        name: Option<String>,
    ) -> scenic::ViewHolder {
        let view_holder = scenic::ViewHolder::new(session.clone(), view_holder_token, name);

        let view_properties = ui_gfx::ViewProperties {
            bounding_box: ui_gfx::BoundingBox {
                min: ui_gfx::Vec3 { x: 0.0, y: 0.0, z: GfxSceneManager::VIEW_BOUNDS_DEPTH },
                max: ui_gfx::Vec3 {
                    x: display_metrics.width_in_pips(),
                    y: display_metrics.height_in_pips(),
                    z: 0.0,
                },
            },
            downward_input: true,
            focus_change: true,
            inset_from_min: ui_gfx::Vec3 { x: 0.0, y: 0.0, z: 0.0 },
            inset_from_max: ui_gfx::Vec3 { x: 0.0, y: 0.0, z: 0.0 },
        };
        view_holder.set_view_properties(view_properties);

        view_holder
    }

    /// Sets focus on the root view if it exists.
    async fn focus_client_root_view(
        weak_view_ref_installed: Weak<ui_views::ViewRefInstalledProxy>,
        weak_focuser: Weak<ui_views::FocuserProxy>,
        mut client_root_view_ref: ui_views::ViewRef,
        mut proxy_view_ref: ui_views::ViewRef,
    ) {
        if let Some(view_ref_installed) = weak_view_ref_installed.upgrade() {
            let watch_result = view_ref_installed.watch(&mut proxy_view_ref).await;
            match watch_result {
                // Handle fidl::Errors.
                Err(e) => fx_log_warn!("Failed with err: {}", e),
                // Handle ui_views::ViewRefInstalledError.
                Ok(Err(value)) => fx_log_warn!("Failed with err: {:?}", value),
                Ok(_) => {
                    // Now set focus on the view_ref.
                    if let Some(focuser) = weak_focuser.upgrade() {
                        let focus_result = focuser.request_focus(&mut client_root_view_ref).await;
                        match focus_result {
                            Ok(_) => fx_log_info!("Refocused client view"),
                            Err(e) => fx_log_warn!("Failed with err: {:?}", e),
                        }
                    } else {
                        fx_log_warn!("Failed to upgrade weak manager");
                    }
                }
            }
        }
    }

    /// Gets the `EntityNode` for the cursor or creates one if it doesn't exist yet.
    ///
    /// # Returns
    /// The [`scenic::EntityNode`] that contains the cursor.
    fn cursor_node(&mut self) -> &scenic::EntityNode {
        if self.cursor_node.is_none() {
            self.cursor_node = Some(scenic::EntityNode::new(self.session.clone()));
            self.root_node.add_child(self.cursor_node.as_ref().unwrap());
        }

        self.cursor_node.as_ref().unwrap()
    }

    /// Requests that all previously enqueued operations are presented.
    fn request_present(presentation_sender: &PresentationSender) {
        presentation_sender
            .unbounded_send(PresentationMessage::RequestPresent)
            .expect("failed to send RequestPresent message");
    }

    /// Requests that all previously enqueued operations are presented.
    ///
    /// # Notes
    /// Returns only once the next frame has been rendered.
    async fn request_present_and_await_next_frame(presentation_sender: &PresentationSender) {
        let (sender, receiver) = oneshot::channel::<()>();
        presentation_sender
            .unbounded_send(PresentationMessage::RequestPresentWithPingback(sender))
            .expect("failed to send RequestPresentWithPingback message");
        _ = receiver.await;
    }

    /// Sets the image to use for the scene's cursor.
    ///
    /// # Parameters
    /// - `image_path`: The path to the image to be used for the cursor.
    ///
    /// # Notes
    /// Due to a current limitation in the `Scenic` api this should only be called once and must be
    /// called *before* `set_cursor_location`.
    fn set_cursor_image(&mut self, image_path: &str) -> Result<(), Error> {
        let image = ImageResource::new(image_path, self.session())?;
        let cursor_rect = scenic::Rectangle::new(self.session(), image.width, image.height);
        let cursor_shape = scenic::ShapeNode::new(self.session());
        cursor_shape.set_shape(&cursor_rect);
        cursor_shape.set_material(&image.material);
        cursor_shape.set_translation(image.width / 2.0, image.height / 2.0, 0.0);

        self.set_cursor_shape(cursor_shape);
        Ok(())
    }

    /// Allows the client to customize the look of the cursor by supplying their own ShapeNode
    ///
    /// # Parameters
    /// - `shape`: The [`scenic::ShapeNode`] to be used as the cursor.
    ///
    /// # Notes
    /// Due to a current limitation in the `Scenic` api this should only be called once and must be
    /// called *before* `set_cursor_location`.
    fn set_cursor_shape(&mut self, shape: scenic::ShapeNode) {
        if !self.cursor_shape.is_none() {
            let current_shape = self.cursor_shape.as_ref().unwrap();
            let node = self.cursor_node.as_ref().unwrap();
            node.remove_child(current_shape);
        }

        self.cursor_node().add_child(&shape);
        self.cursor_shape = Some(shape);
        GfxSceneManager::request_present(&self.presentation_sender);
    }

    /// Creates a default cursor shape for use with the client hasn't created a custom cursor
    ///
    /// # Returns
    /// The [`scenic::ShapeNode`] to be used as the cursor.
    fn get_default_cursor(&self) -> scenic::ShapeNode {
        const CURSOR_DEFAULT_WIDTH: f32 = 20.0;
        const CURSOR_DEFAULT_HEIGHT: f32 = 20.0;

        let cursor_rect = scenic::RoundedRectangle::new(
            self.session(),
            CURSOR_DEFAULT_WIDTH,
            CURSOR_DEFAULT_HEIGHT,
            0.0,
            CURSOR_DEFAULT_WIDTH / 2.0,
            CURSOR_DEFAULT_WIDTH / 2.0,
            CURSOR_DEFAULT_WIDTH / 2.0,
        );
        let cursor_shape = scenic::ShapeNode::new(self.session());
        cursor_shape.set_shape(&cursor_rect);

        // Adjust position so that the upper left corner matches the pointer location
        cursor_shape.set_translation(CURSOR_DEFAULT_WIDTH / 2.0, CURSOR_DEFAULT_HEIGHT / 2.0, 0.0);

        let material = scenic::Material::new(self.session());
        material.set_color(ui_gfx::ColorRgba { red: 255, green: 0, blue: 255, alpha: 255 });
        cursor_shape.set_material(&material);

        cursor_shape
    }

    fn session(&self) -> scenic::SessionPtr {
        return self.session.clone();
    }

    async fn update_viewport(&self) {
        let (width_pixels, height_pixels) = self.display_size.pixels();

        // Viewport should match the visible part of the display 1:1. To do this
        // we need to match the ClipSpaceTransform.
        //
        // Since the ClipSpaceTransform is defined in Vulkan NDC with scaling,
        // and the Viewport is defined in pixel coordinates, we need to be able
        // to transform offsets to pixel coordinates. This is done by
        // multiplying by half the display length and inverting the scale.
        //
        // Because the ClipSpaceTransform is defined with its origin in the
        // center, and the Viewport with its origin in the top left corner, we
        // need to add a center offset to compensate.  This turns out to be as
        // simple as half the scaled display length minus half the ClipSpace
        // length, which equals scale - 1 in NDC.
        //
        // Finally, because the ClipSpaceTransform and the Viewport transform
        // are defined in opposite directions (camera to scene vs context to
        // viewport), all the transforms should be inverted for the Viewport
        // transform. This means an inverted scale and negative clip offsets.
        //
        // (See the same logic in root presenter: https://cs.opensource.google/fuchsia/fuchsia/+/main:src/ui/bin/root_presenter/presentation.cc;drc=44c08193dbb4ed5d82804d0faf7bce76d95d4dab;l=423)
        self.viewport_publisher.lock().set({
            let inverted_scale = 1. / self.clip_scale;
            let center_offset_ndc = self.clip_scale - 1.;
            let ndc_to_pixel_x = inverted_scale * width_pixels * 0.5;
            let ndc_to_pixel_y = inverted_scale * height_pixels * 0.5;
            let x_offset = ndc_to_pixel_x * (center_offset_ndc - self.clip_offset_x);
            let y_offset = ndc_to_pixel_y * (center_offset_ndc - self.clip_offset_y);
            InjectorViewportSpec {
                width: width_pixels,
                height: height_pixels,
                scale: inverted_scale,
                x_offset,
                y_offset,
            }
        })
    }
}
