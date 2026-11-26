#[cfg(target_os = "windows")]
use crate::window::set_cloak;
use crate::{
    convert::{winit_key_code_to_code, winit_key_to_key},
    window::{WinState, Window, scaled_size},
    window_modifiers::WindowModifiers,
};
#[cfg(feature = "accesskit")]
use accesskit_winit::Adapter;
use hashbrown::HashMap;
use log::warn;
use std::{error::Error, fmt::Display, sync::Arc};
use vizia_input::ImeState;

// #[cfg(feature = "accesskit")]
// use accesskit::{Action, NodeBuilder, NodeId, TreeUpdate};
// #[cfg(feature = "accesskit")]
// use accesskit_winit;
// use std::cell::RefCell;
use vizia_core::context::EventProxy;
use vizia_core::prelude::*;
use vizia_core::{backend::*, events::EventManager};
use vizia_reactive::Runtime;
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalPosition, PhysicalPosition, PhysicalSize},
    error::EventLoopError,
    event::ElementState,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{NativeKeyCode, PhysicalKey},
    window::{CursorIcon, CustomCursor, WindowAttributes, WindowId, WindowLevel},
};

// #[cfg(all(
//     feature = "clipboard",
//     feature = "wayland",
//     any(
//         target_os = "linux",
//         target_os = "dragonfly",
//         target_os = "freebsd",
//         target_os = "netbsd",
//         target_os = "openbsd"
//     )
// ))]
// use raw_window_handle::{HasRawDisplayHandle, RawDisplayHandle};
use vizia_window::{Anchor, AnchorTarget, WindowPosition};

#[derive(Debug)]
pub enum UserEvent {
    Event(Event),
    #[cfg(feature = "accesskit")]
    AccessKitEvent(accesskit_winit::Event),
}

#[cfg(feature = "accesskit")]
impl From<accesskit_winit::Event> for UserEvent {
    fn from(action_request_event: accesskit_winit::Event) -> Self {
        UserEvent::AccessKitEvent(action_request_event)
    }
}

impl From<vizia_core::events::Event> for UserEvent {
    fn from(event: vizia_core::events::Event) -> Self {
        UserEvent::Event(event)
    }
}

type IdleCallback = Option<Box<dyn Fn(&mut Context)>>;

#[derive(Debug)]
pub enum ApplicationError {
    EventLoopError(EventLoopError),
    LogError,
}

impl Display for ApplicationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplicationError::EventLoopError(ele) => write!(f, "{}", ele),
            ApplicationError::LogError => write!(f, "log error"),
        }
    }
}

impl std::error::Error for ApplicationError {}

///Creating a new application creates a root `Window` and a `Context`. Views declared within the closure passed to `Application::new()` are added to the context and rendered into the root window.
///
/// # Example
/// ```no_run
/// # use vizia_core::prelude::*;
/// # use vizia_winit::application::Application;
/// Application::new(|cx|{
///    // Content goes here
/// })
/// .run();
///```
/// Calling `run()` on the `Application` causes the program to enter the event loop and for the main window to display.
pub struct Application {
    cx: BackendContext,
    event_manager: EventManager,
    pub(crate) event_loop: Option<EventLoop<UserEvent>>,
    on_idle: IdleCallback,
    window_description: WindowDescription,
    control_flow: ControlFlow,
    event_loop_proxy: EventLoopProxy<UserEvent>,
    windows: HashMap<WindowId, WinState>,
    window_ids: HashMap<Entity, WindowId>,
    /// Earliest time the next frame may start, when the app limits the frame rate.
    next_frame_due: Option<Instant>,
    /// When to re-read the display's refresh rate next (a mode change, or a move
    /// to another display without a `Moved` event, changes it).
    next_refresh_check: Option<Instant>,
    /// Vblank phase of the window's display, while the frame slots are locked to it.
    #[cfg(target_os = "macos")]
    vblank: Option<crate::vblank::VblankClock>,
    #[cfg(feature = "accesskit")]
    accesskit_adapter: Option<accesskit_winit::Adapter>,
    #[cfg(feature = "accesskit")]
    adapter_initialized: bool,
}

pub struct WinitEventProxy(EventLoopProxy<UserEvent>);

impl EventProxy for WinitEventProxy {
    fn send(&self, event: Event) -> Result<(), ()> {
        self.0.send_event(UserEvent::Event(event)).map_err(|_| ())
    }

    fn make_clone(&self) -> Box<dyn EventProxy> {
        Box::new(WinitEventProxy(self.0.clone()))
    }
}

impl Application {
    /// Time between frame slots, or `None` when this backend doesn't need them.
    ///
    /// On Windows a vsync'd `SwapBuffers` (WGL) blocks until vblank, so when the
    /// slots would be locked 1:1 to the display (`frame_slot_vblanks() == Some(1)`)
    /// the swap already presents exactly one frame per refresh. Slots at the
    /// measured rate on top of it add nothing, and their phase against vblank is
    /// arbitrary: when it settles just before vblank, frames miss it (measured on
    /// a 59.951 Hz display: 11 refreshes without a new frame in 30 s, in one of
    /// four runs). Frames then wait for the swap instead, as before the limit
    /// existed. macOS keeps the slots: `flushBuffer` doesn't lock to vblank there.
    fn frame_slot_interval(&self) -> Option<Duration> {
        #[cfg(target_os = "windows")]
        if self.window_description.vsync && self.cx.frame_slot_vblanks() == Some(1) {
            return None;
        }
        self.cx.min_frame_interval()
    }

    pub fn new<F>(content: F) -> Self
    where
        F: 'static + FnOnce(&mut Context),
    {
        let context = Context::new();

        let event_loop =
            EventLoop::<UserEvent>::with_user_event().build().expect("Failed to create event loop");

        let mut cx = BackendContext::new(context);

        // Mark the current thread as the UI thread so that rayon workers
        // correctly enqueue effects via SYNC_RUNTIME instead of the
        // thread-local RUNTIME (which nobody drains).
        Runtime::init_on_ui_thread();

        let proxy = event_loop.create_proxy();
        cx.set_event_proxy(Box::new(WinitEventProxy(proxy.clone())));

        // Ensure we wake the event loop when a SyncSignal is mutated off the UI thread.
        let waker_proxy = proxy.clone();
        Runtime::set_sync_effect_waker(move || {
            let _ = waker_proxy.send_event(UserEvent::Event(Event::new(())));
        });

        cx.renegotiate_language();
        cx.0.add_built_in_styles();
        (content)(cx.context());

        Self {
            cx,
            event_manager: EventManager::new(),
            event_loop: Some(event_loop),
            on_idle: None,
            window_description: WindowDescription::new(),
            control_flow: ControlFlow::Wait,
            event_loop_proxy: proxy,
            windows: HashMap::new(),
            window_ids: HashMap::new(),
            next_frame_due: None,
            next_refresh_check: None,
            #[cfg(target_os = "macos")]
            vblank: None,
            #[cfg(feature = "accesskit")]
            accesskit_adapter: None,
            #[cfg(feature = "accesskit")]
            adapter_initialized: false,
        }
    }

    fn create_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_entity: Entity,
        window_description: &WindowDescription,
        #[allow(unused_variables)] owner: Option<Arc<winit::window::Window>>,
    ) -> Result<Arc<winit::window::Window>, Box<dyn Error>> {
        #[allow(unused_mut)]
        let mut window_attributes =
            apply_window_description(window_description, self.window_description.user_scale_factor);

        let window_state = WinState::new(
            event_loop,
            window_entity,
            window_attributes,
            owner,
            window_description.vsync,
        )?;
        let window = window_state.window.clone();

        if let Some(position) = window_description.position {
            window.set_outer_position(LogicalPosition::new(position.x, position.y));
        } else {
            let (anchor, mut parent_anchor) =
                match (window_description.anchor, window_description.parent_anchor) {
                    (Some(a), None) => (Some(a), Some(a)),
                    (None, Some(b)) => (Some(b.opposite()), Some(b)),
                    t => t,
                };

            if let Some(anchor) = anchor {
                let (y, x) = match anchor {
                    Anchor::TopLeft => (0.0, 0.0),
                    Anchor::TopCenter => (0.0, 0.5),
                    Anchor::TopRight => (0.0, 1.0),
                    Anchor::Left => (0.5, 0.0),
                    Anchor::Center => (0.5, 0.5),
                    Anchor::Right => (0.5, 1.0),
                    Anchor::BottomLeft => (1.0, 0.0),
                    Anchor::BottomCenter => (1.0, 0.5),
                    Anchor::BottomRight => (1.0, 1.0),
                };

                let window_size = window.inner_size();

                let anchor_target = window_description.anchor_target.unwrap_or_default();
                let parent = match anchor_target {
                    AnchorTarget::Monitor => window
                        .current_monitor()
                        .map(|monitor| (PhysicalPosition::default(), monitor.size())),
                    AnchorTarget::Window => self
                        .cx
                        .0
                        .tree
                        .get_parent_window(window_entity)
                        .and_then(|parent_window| self.window_ids.get(&parent_window))
                        .and_then(|id| self.windows.get(id))
                        .and_then(|WinState { window, .. }| {
                            let position = window
                                .outer_position()
                                .inspect_err(|e| warn!("can't get window position: {e:?}"));
                            Some((position.ok()?, window.inner_size()))
                        }),
                    AnchorTarget::Mouse => self
                        .cx
                        .0
                        .tree
                        .get_parent_window(window_entity)
                        .and_then(|parent_window| self.window_ids.get(&parent_window))
                        .and_then(|id| self.windows.get(id))
                        .and_then(|WinState { window, .. }| {
                            window
                                .outer_position()
                                .inspect_err(|e| warn!("can't get window position: {e:?}"))
                                .ok()
                        })
                        .map(|pos| {
                            (
                                PhysicalPosition::new(
                                    pos.x + self.cx.0.mouse.cursor_x as i32,
                                    pos.y + self.cx.0.mouse.cursor_y as i32,
                                ),
                                PhysicalSize::new(0, 0),
                            )
                        }),
                };

                if let Some((parent_position, parent_size)) = parent {
                    if anchor_target != AnchorTarget::Window {
                        parent_anchor = Some(anchor);
                    }

                    let (py, px) = match parent_anchor.unwrap_or_default() {
                        Anchor::TopLeft => (0.0, 0.0),
                        Anchor::TopCenter => (0.0, 0.5),
                        Anchor::TopRight => (0.0, 1.0),
                        Anchor::Left => (0.5, 0.0),
                        Anchor::Center => (0.5, 0.5),
                        Anchor::Right => (0.5, 1.0),
                        Anchor::BottomLeft => (1.0, 0.0),
                        Anchor::BottomCenter => (1.0, 0.5),
                        Anchor::BottomRight => (1.0, 1.0),
                    };

                    let x = (((parent_size.width as f32 * px) as i32
                        - (window_size.width as f32 * x) as i32)
                        as f32) as i32;
                    let y = (((parent_size.height as f32 * py) as i32
                        - (window_size.height as f32 * y) as i32)
                        as f32) as i32;

                    let offset = window_description.offset.unwrap_or_default();
                    let offset: PhysicalPosition<i32> = PhysicalPosition::from_logical(
                        LogicalPosition::new(offset.x, offset.y),
                        window.scale_factor() * self.window_description.user_scale_factor,
                    );

                    window.set_outer_position(PhysicalPosition::new(
                        parent_position.x + x + offset.x,
                        parent_position.y + y + offset.y,
                    ));
                }
            }
        }

        let window_id = window_state.window.id();
        self.windows.insert(window_id, window_state);
        self.window_ids.insert(window_entity, window_id);
        if window_entity == Entity::root() {
            self.cx.set_display_refresh_rate(monitor_refresh_rate(&window));
        }
        Ok(window)
    }

    /// A scale factor applied on top of the system's DPI scaling, defaults to 1.0. It can be
    /// changed at runtime with `WindowEvent::SetUserScaleFactor`.
    ///
    /// The context has a single scale factor, so this applies to every window of the application.
    pub fn user_scale_factor(mut self, factor: f64) -> Self {
        self.window_description.user_scale_factor = factor;
        self
    }

    /// Applies a user scale factor requested with `WindowEvent::SetUserScaleFactor`.
    ///
    /// Every window keeps its logical size, so its physical size changes by the ratio of the new
    /// factor to the old one (a maximized or fullscreen window keeps its physical size instead).
    fn apply_user_scale_factor(&mut self) {
        let Some(factor) = self
            .cx
            .0
            .windows
            .get(&Entity::root())
            .map(|window_state| window_state.window_description.user_scale_factor)
        else {
            return;
        };
        let old_factor = self.window_description.user_scale_factor;
        if factor == old_factor {
            return;
        }
        self.window_description.user_scale_factor = factor;

        for window_state in self.windows.values_mut() {
            let window = window_state.window.clone();
            let entity = window_state.entity;
            self.cx.set_scale_factor(window.scale_factor() * factor);

            // Update the size limits first so that they don't block the new size.
            if let Some(description) =
                self.cx.0.windows.get(&entity).map(|state| &state.window_description)
            {
                window.set_min_inner_size(
                    description.min_inner_size.map(|size| scaled_size(size, factor)),
                );
                window.set_max_inner_size(
                    description.max_inner_size.map(|size| scaled_size(size, factor)),
                );
            }

            let mut size = window.inner_size();
            if !window.is_maximized() && window.fullscreen().is_none() {
                let ratio = factor / old_factor;
                let requested = PhysicalSize::new(
                    (size.width as f64 * ratio).round() as u32,
                    (size.height as f64 * ratio).round() as u32,
                );
                match window.request_inner_size(requested) {
                    // Applied immediately: a `Resized` event may not follow.
                    Some(applied) => {
                        window_state.resize(applied);
                        size = applied;
                    }
                    // Lay out at the requested size now; `Resized` corrects it if the
                    // system picks a different one.
                    None => size = requested,
                }
            }

            // Recomputes the window's logical size with the new scale factor.
            self.cx.set_window_size(entity, size.width as f32, size.height as f32);
            self.cx.needs_refresh(entity);
            window.request_redraw();
        }
    }

    /// Sets the default built-in theming to be ignored.
    pub fn ignore_default_theme(mut self) -> Self {
        self.cx.context().ignore_default_theme = true;
        self
    }

    pub fn should_poll(mut self) -> Self {
        self.control_flow = ControlFlow::Poll;

        self
    }

    /// Takes a closure which will be called at the end of every loop of the application.
    ///
    /// The callback provides a place to run 'idle' processing and happens at the end of each loop but before drawing.
    /// If the callback pushes events into the queue in state then the event loop will re-run. Care must be taken not to
    /// push events into the queue every time the callback runs unless this is intended.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use vizia_core::prelude::*;
    /// # use vizia_winit::application::Application;
    /// #
    /// Application::new(|cx| {
    ///     // Build application here
    /// })
    /// .on_idle(|cx| {
    ///     // Code here runs at the end of every event loop after OS and vizia events have been handled
    /// })
    /// .run();
    /// ```
    pub fn on_idle<F: 'static + Fn(&mut Context)>(mut self, callback: F) -> Self {
        self.on_idle = Some(Box::new(callback));

        self
    }

    /// Returns a `ContextProxy` which can be used to send events from another thread.
    pub fn get_proxy(&self) -> ContextProxy {
        self.cx.0.get_proxy()
    }

    pub fn run(mut self) -> Result<(), ApplicationError> {
        self.event_loop.take().unwrap().run_app(&mut self).map_err(ApplicationError::EventLoopError)
    }
}

impl ApplicationHandler<UserEvent> for Application {
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, user_event: UserEvent) {
        match user_event {
            UserEvent::Event(event) => {
                self.cx.send_event(event);
            }

            #[cfg(feature = "accesskit")]
            UserEvent::AccessKitEvent(access_event) => {
                match access_event.window_event {
                    accesskit_winit::WindowEvent::InitialTreeRequested => {
                        let tree_update = self.cx.init_accessibility_tree();
                        if let Some(adapter) = &mut self.accesskit_adapter {
                            adapter.update_if_active(|| {
                                self.adapter_initialized = true;
                                tree_update
                            });
                        }
                    }
                    accesskit_winit::WindowEvent::ActionRequested(action_request) => {
                        let node_id = action_request.target_node;

                        if action_request.action != Action::ScrollIntoView {
                            let entity = Entity::new(node_id.0, 0);

                            // Handle focus action from screen reader
                            if action_request.action == Action::Focus {
                                self.cx.0.with_current(entity, |cx| {
                                    cx.focus();
                                });
                            }

                            self.cx.send_event(
                                Event::new(WindowEvent::ActionRequest(action_request))
                                    .direct(entity),
                            );
                        }
                    }
                    accesskit_winit::WindowEvent::AccessibilityDeactivated => todo!(),
                }
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.windows.is_empty() {
            // Create the main window
            let main_window: Arc<winit::window::Window> = self
                .create_window(event_loop, Entity::root(), &self.window_description.clone(), None)
                .expect("failed to create initial window");
            let custom_cursors = Arc::new(load_default_cursors(event_loop));
            self.cx.add_main_window(
                Entity::root(),
                &self.window_description,
                (main_window.scale_factor() * self.window_description.user_scale_factor) as f32,
            );
            self.cx.add_window(Window {
                window: Some(main_window.clone()),
                on_close: None,
                on_create: None,
                should_close: false,
                custom_cursors: custom_cursors.clone(),
                #[cfg(target_os = "macos")]
                last_cursor: None,
            });

            self.cx.0.windows.insert(
                Entity::root(),
                WindowState {
                    window_description: self.window_description.clone(),
                    ..Default::default()
                },
            );

            #[cfg(feature = "accesskit")]
            {
                self.accesskit_adapter = Some(Adapter::with_event_loop_proxy(
                    event_loop,
                    &main_window,
                    self.event_loop_proxy.clone(),
                ));
            }

            main_window.set_visible(self.window_description.visible);

            // set current system theme if available
            if let Some(theme) = main_window.theme() {
                let theme = match theme {
                    winit::window::Theme::Light => ThemeMode::LightMode,
                    winit::window::Theme::Dark => ThemeMode::DarkMode,
                };
                self.cx.emit_origin(WindowEvent::ThemeChanged(theme));
            }

            self.cx.0.add_built_in_styles();

            // Create any subwindows
            for (window_entity, window_state) in self.cx.0.windows.clone().into_iter() {
                if window_entity == Entity::root() {
                    continue;
                }
                let owner = window_state.owner.and_then(|entity| {
                    self.window_ids
                        .get(&entity)
                        .and_then(|id| self.windows.get(id).map(|ws| ws.window.clone()))
                });

                let window = self
                    .create_window(
                        event_loop,
                        window_entity,
                        &window_state.window_description,
                        owner,
                    )
                    .expect("Failed to create window");

                self.cx.add_main_window(
                    window_entity,
                    &window_state.window_description,
                    (window.scale_factor() * self.window_description.user_scale_factor) as f32,
                );

                window.set_visible(window_state.window_description.visible);

                self.cx.0.with_current(window_entity, |cx| {
                    if let Some(content) = &window_state.content {
                        (content)(cx)
                    }
                });
                self.cx.mutate_window(window_entity, |cx, win: &mut Window| {
                    win.window = Some(window.clone());
                    win.custom_cursors = custom_cursors.clone();
                    if let Some(callback) = &win.on_create {
                        (callback)(&mut EventContext::new_with_current(
                            cx.context(),
                            window_entity,
                        ));
                    }
                });
                self.cx.needs_refresh(window_entity);
            }
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: winit::event::WindowEvent,
    ) {
        let window = match self.windows.get_mut(&window_id) {
            Some(window) => window,
            None => return,
        };

        match event {
            winit::event::WindowEvent::Resized(size) => {
                window.resize(size);
                self.cx.set_window_size(window.entity, size.width as f32, size.height as f32);
                self.cx.needs_refresh(window.entity);
                window.window().request_redraw();

                #[cfg(target_os = "windows")]
                {
                    self.event_manager.flush_events(self.cx.context(), |_| {});

                    self.cx.process_style_updates();

                    if self.cx.process_animations() {
                        window.window().request_redraw();
                    }

                    self.cx.process_visual_updates();

                    // #[cfg(feature = "accesskit")]

                    // self.cx.process_tree_updates(|tree_updates| {
                    //     for update in tree_updates.iter_mut() {
                    //         self.accesskit_adapter
                    //             .unwrap()
                    //             .update_if_active(|| update.take().unwrap());
                    //     }
                    // });

                    // for update in self.cx.0.tree_updates.iter_mut() {
                    //     self.accesskit_adapter
                    //         .as_mut()
                    //         .unwrap()
                    //         .update_if_active(|| update.take().unwrap());
                    // }

                    // self.cx.0.tree_updates.clear();

                    window.window().request_redraw();
                }
            }

            winit::event::WindowEvent::Moved(position) => {
                let window_entity = window.entity;
                if window_entity == Entity::root() {
                    // The window may now be on a display with a different refresh rate.
                    self.cx.set_display_refresh_rate(monitor_refresh_rate(window.window()));
                    // Re-read the vblank phase for the new display now rather than at
                    // the next periodic check: until then the slots have the new
                    // rate but no phase, and can sit on a vblank boundary.
                    self.next_refresh_check = None;
                }
                self.cx.emit_window_event(
                    window_entity,
                    WindowEvent::WindowMoved(WindowPosition { x: position.x, y: position.y }),
                );

                #[cfg(target_os = "windows")]
                {
                    self.event_manager.flush_events(self.cx.context(), |_| {});

                    self.cx.process_style_updates();

                    if self.cx.process_animations() {
                        window.window().request_redraw();
                    }

                    self.cx.process_visual_updates();

                    // #[cfg(feature = "accesskit")]

                    // self.cx.process_tree_updates(|tree_updates| {
                    //     for update in tree_updates.iter_mut() {
                    //         self.accesskit_adapter
                    //             .unwrap()
                    //             .update_if_active(|| update.take().unwrap());
                    //     }
                    // });

                    // for update in self.cx.0.tree_updates.iter_mut() {
                    //     self.accesskit_adapter
                    //         .as_mut()
                    //         .unwrap()
                    //         .update_if_active(|| update.take().unwrap());
                    // }

                    // self.cx.0.tree_updates.clear();
                }
            }

            winit::event::WindowEvent::CloseRequested | winit::event::WindowEvent::Destroyed => {
                let window_entity = window.entity;
                self.cx.emit_window_event(window_entity, WindowEvent::WindowClose);
            }
            winit::event::WindowEvent::DroppedFile(path) => {
                self.cx.emit_window_event(window.entity, WindowEvent::Drop(DropData::File(path)));
            }

            winit::event::WindowEvent::HoveredFile(_) => {}
            winit::event::WindowEvent::HoveredFileCancelled => {}
            winit::event::WindowEvent::Focused(is_focused) => {
                self.cx.emit_window_event(window.entity, WindowEvent::WindowFocused(is_focused));

                self.cx.0.window_has_focus = is_focused;
                // #[cfg(feature = "accesskit")]
                // accesskit.update_if_active(|| TreeUpdate {
                //     nodes: vec![],
                //     tree: None,
                //     focus: is_focused.then_some(self.cx.focused().accesskit_id()).unwrap_or(NodeId(0)),
                // });
            }
            winit::event::WindowEvent::KeyboardInput { device_id: _, event, is_synthetic: _ } => {
                let code = match event.physical_key {
                    PhysicalKey::Code(code) => winit_key_code_to_code(code),
                    PhysicalKey::Unidentified(native) => match native {
                        NativeKeyCode::Windows(_scancode) => return,
                        _ => return,
                    },
                };

                let key = match event.logical_key {
                    winit::keyboard::Key::Named(named_key) => winit_key_to_key(named_key),
                    _ => None,
                };

                if event.state == ElementState::Pressed {
                    match &event.logical_key {
                        winit::keyboard::Key::Character(character) => {
                            if let Some(character) = character.as_str().chars().next() {
                                self.cx.emit_window_event(
                                    window.entity,
                                    WindowEvent::CharInput(character),
                                );
                            }
                        }
                        // Some platforms report space as a named key instead of character text.
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Space) => {
                            self.cx.emit_window_event(window.entity, WindowEvent::CharInput(' '));
                        }
                        _ => {}
                    }
                }

                let event = match event.state {
                    winit::event::ElementState::Pressed => WindowEvent::KeyDown(code, key),
                    winit::event::ElementState::Released => WindowEvent::KeyUp(code, key),
                };

                self.cx.emit_window_event(window.entity, event);
            }
            winit::event::WindowEvent::ModifiersChanged(modifiers) => {
                self.cx.modifiers().set(Modifiers::SHIFT, modifiers.state().shift_key());

                self.cx.modifiers().set(Modifiers::ALT, modifiers.state().alt_key());

                self.cx.modifiers().set(Modifiers::CTRL, modifiers.state().control_key());

                self.cx.modifiers().set(Modifiers::SUPER, modifiers.state().super_key());
            }
            winit::event::WindowEvent::Ime(ime) => match ime {
                winit::event::Ime::Enabled => {
                    self.cx.0.set_ime_state(ImeState::StartComposition);
                    self.cx.emit_window_event(window.entity, WindowEvent::ImeActivate(true));
                }
                winit::event::Ime::Preedit(text, cursor) => {
                    self.cx.0.set_ime_state(ImeState::Composing {
                        preedit: Some(text.clone()),
                        cursor_pos: cursor,
                    });
                    self.cx.emit_window_event(window.entity, WindowEvent::ImePreedit(text, cursor));
                }
                winit::event::Ime::Commit(text) => {
                    self.cx.0.set_ime_state(ImeState::EndComposition);
                    self.cx.emit_window_event(window.entity, WindowEvent::ImeCommit(text));
                }
                winit::event::Ime::Disabled => {
                    self.cx.0.set_ime_state(ImeState::Inactive);
                    self.cx.emit_window_event(window.entity, WindowEvent::ImeActivate(false));
                }
            },
            winit::event::WindowEvent::CursorMoved { device_id: _, position } => {
                self.cx.emit_window_event(
                    window.entity,
                    WindowEvent::MouseMove(position.x as f32, position.y as f32),
                );
            }
            winit::event::WindowEvent::CursorEntered { device_id: _ } => {
                self.cx.emit_window_event(window.entity, WindowEvent::MouseEnter);
            }
            winit::event::WindowEvent::CursorLeft { device_id: _ } => {
                self.cx.emit_window_event(window.entity, WindowEvent::MouseLeave);
            }
            winit::event::WindowEvent::MouseWheel { device_id: _, delta, phase: _ } => {
                let out_event = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => {
                        WindowEvent::MouseScroll(x, y)
                    }
                    winit::event::MouseScrollDelta::PixelDelta(pos) => {
                        WindowEvent::MouseScroll(
                            pos.x as f32 / 20.0,
                            pos.y as f32 / 20.0, // this number calibrated for wayland
                        )
                    }
                };

                self.cx.emit_window_event(window.entity, out_event);
            }
            winit::event::WindowEvent::MouseInput { device_id: _, state, button } => {
                let button = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    winit::event::MouseButton::Other(val) => MouseButton::Other(val),
                    winit::event::MouseButton::Back => MouseButton::Back,
                    winit::event::MouseButton::Forward => MouseButton::Forward,
                };

                let event = match state {
                    winit::event::ElementState::Pressed => WindowEvent::MouseDown(button),
                    winit::event::ElementState::Released => WindowEvent::MouseUp(button),
                };

                self.cx.emit_window_event(window.entity, event);
            }

            winit::event::WindowEvent::ScaleFactorChanged {
                scale_factor,
                inner_size_writer: _,
            } => {
                self.cx.set_scale_factor(scale_factor * self.window_description.user_scale_factor);
                self.cx.needs_refresh(window.entity);
                if window.entity == Entity::root() {
                    self.cx.set_display_refresh_rate(monitor_refresh_rate(window.window()));
                    self.next_refresh_check = None;
                }
            }
            winit::event::WindowEvent::ThemeChanged(theme) => {
                let theme = match theme {
                    winit::window::Theme::Light => ThemeMode::LightMode,
                    winit::window::Theme::Dark => ThemeMode::DarkMode,
                };
                self.cx.emit_window_event(window.entity, WindowEvent::ThemeChanged(theme));
            }
            winit::event::WindowEvent::Occluded(_) => {}
            winit::event::WindowEvent::RedrawRequested => {
                for window in self.windows.values_mut() {
                    let start = Instant::now();
                    window.make_current();
                    let current = Instant::now();
                    self.cx.record_frame_phase(FramePhase::MakeCurrent, current - start);
                    //self.cx.needs_refresh(window.entity);
                    let drew =
                        self.cx.draw(window.entity, &mut window.surface, &mut window.dirty_surface);
                    let drawn = Instant::now();
                    self.cx.record_frame_phase(FramePhase::Draw, drawn - current);
                    if drew {
                        let (flush, present) = window.swap_buffers();
                        self.cx.record_frame_phase(FramePhase::Flush, flush);
                        self.cx.record_frame_phase(FramePhase::Present, present);
                        self.cx.finish_frame(Instant::now());
                    }

                    // Un-cloak
                    #[cfg(target_os = "windows")]
                    if window.is_initially_cloaked {
                        window.is_initially_cloaked = false;
                        set_cloak(window.window(), false);
                    }
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.windows.is_empty() {
            event_loop.exit();
            return;
        }

        event_loop.set_control_flow(self.control_flow);

        let start = Instant::now();
        Runtime::drain_pending_work();

        self.event_manager.flush_events(self.cx.context(), |_| {});
        self.apply_user_scale_factor();
        let events_done = Instant::now();
        self.cx.record_frame_phase(FramePhase::Events, events_done - start);

        self.cx.process_style_updates();

        let mut wants_redraw = self.cx.process_animations();
        let style_done = Instant::now();
        if self.next_refresh_check.is_none_or(|t| style_done >= t) {
            self.next_refresh_check = Some(style_done + REFRESH_RATE_CHECK_INTERVAL);
            if let Some(window) =
                self.window_ids.get(&Entity::root()).and_then(|id| self.windows.get(id))
            {
                self.cx.set_display_refresh_rate(monitor_refresh_rate(window.window()));
                #[cfg(target_os = "macos")]
                {
                    use winit::platform::macos::MonitorHandleExtMacOS;
                    let display = window.window().current_monitor().map(|m| m.native_id());
                    let locked = self.cx.frame_slot_vblanks().is_some();
                    match (locked, display) {
                        (true, Some(id))
                            if self.vblank.as_ref().is_some_and(|v| v.display() == id) => {}
                        (true, Some(id)) => self.vblank = crate::vblank::VblankClock::new(id),
                        _ => self.vblank = None,
                    }
                }
            }
        }
        #[cfg(target_os = "macos")]
        let vblank_wake = self.vblank.as_mut().and_then(|v| v.poll(style_done));
        #[cfg(not(target_os = "macos"))]
        let vblank_wake: Option<Instant> = None;
        self.cx.record_frame_phase(FramePhase::Style, style_done - events_done);

        self.cx.process_visual_updates();
        self.cx.record_frame_phase(FramePhase::Layout, style_done.elapsed());

        #[cfg(feature = "accesskit")]
        {
            self.cx.process_tree_updates();

            if self.adapter_initialized {
                for update in self.cx.0.tree_updates.iter_mut() {
                    self.accesskit_adapter
                        .as_mut()
                        .unwrap()
                        .update_if_active(|| update.take().unwrap());
                }
            }

            self.cx.0.tree_updates.clear();
        }

        if let Some(idle_callback) = &self.on_idle {
            self.cx.set_current(Entity::root());
            (idle_callback)(self.cx.context());
        }

        if self.cx.has_queued_events() {
            self.event_loop_proxy
                .send_event(UserEvent::Event(Event::new(())))
                .expect("Failed to send event");
        }

        wants_redraw |=
            self.cx.0.windows.iter().any(|(_, window_state)| !window_state.redraw_list.is_empty());
        // A `WindowEvent::Redraw` (e.g. from `Handle::modify`) also goes through the slots.
        #[cfg(target_os = "macos")]
        {
            wants_redraw |= crate::window::REDRAW_PENDING.swap(false, std::sync::atomic::Ordering::Relaxed);
        }

        // With a frame-rate limit, a redraw is requested only once the next frame
        // slot has come; until then the loop sleeps until that slot (or a timer).
        let mut deferred_until = None;
        if wants_redraw {
            let now = Instant::now();
            match (self.frame_slot_interval(), self.next_frame_due) {
                // Allow a little early: wake-ups are late more often than early,
                // and the swap still waits for vblank.
                (Some(_), Some(due)) if now + FRAME_SLOT_SLACK < due => deferred_until = Some(due),
                (interval, due) => {
                    for window in self.windows.values() {
                        window.window().request_redraw();
                    }
                    // Fixed-rate slots keep the average rate at the limit even when it
                    // doesn't divide the refresh rate; after a stall, restart from now
                    // instead of bursting to catch up.
                    self.next_frame_due = interval.map(|interval| {
                        let next =
                            due.filter(|due| now < *due + interval).unwrap_or(now) + interval;
                        // Locked to the display: keep the slots at a fixed phase
                        // after vblank, so the frames don't drift into the vblank
                        // boundary (see `FRAME_SLOT_PHASE`).
                        #[cfg(target_os = "macos")]
                        if let (Some(v), Some(_)) = (&self.vblank, self.cx.frame_slot_vblanks()) {
                            return v.align(next, FRAME_SLOT_PHASE, now);
                        }
                        next
                    });
                }
            }
        }

        if self.control_flow != ControlFlow::Poll {
            let wake = [self.cx.get_next_timer_time(), deferred_until, vblank_wake]
                .into_iter()
                .flatten()
                .min();
            if let Some(wake) = wake {
                event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
            } else {
                event_loop.set_control_flow(ControlFlow::Wait);
            }
        }

        let window_entities = self
            .cx
            .0
            .windows
            .iter()
            .filter_map(|(entity, state)| state.should_close.then_some(*entity))
            .collect::<Vec<_>>();

        for window_entity in window_entities {
            self.cx.0.remove(window_entity);
        }

        // Sync window state with context
        self.windows.retain(|_, win| self.cx.0.windows.contains_key(&win.entity));
        self.window_ids.retain(|e, _| self.cx.0.windows.contains_key(e));

        if self.windows.len() != self.cx.0.windows.len() {
            for (window_entity, window_state) in self.cx.0.windows.clone().iter() {
                if !self.window_ids.contains_key(window_entity) {
                    let owner = window_state.owner.and_then(|entity| {
                        self.window_ids
                            .get(&entity)
                            .and_then(|id| self.windows.get(id).map(|ws| ws.window.clone()))
                    });

                    let window = self
                        .create_window(
                            event_loop,
                            *window_entity,
                            &window_state.window_description,
                            owner,
                        )
                        .expect("Failed to create window");

                    self.cx.add_main_window(
                        *window_entity,
                        &window_state.window_description,
                        (window.scale_factor() * self.window_description.user_scale_factor) as f32,
                    );

                    window.set_visible(window_state.window_description.visible);

                    self.cx.0.with_current(*window_entity, |cx| {
                        if let Some(content) = &window_state.content {
                            (content)(cx)
                        }
                    });

                    self.cx.mutate_window(*window_entity, |cx, win: &mut Window| {
                        win.window = Some(window.clone());
                        if let Some(callback) = &win.on_create {
                            (callback)(&mut EventContext::new_with_current(
                                cx.context(),
                                *window_entity,
                            ));
                        }
                    });
                }
            }
        }

        if self.windows.is_empty() {
            event_loop.exit();
        }
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, _cause: winit::event::StartCause) {
        let start = Instant::now();
        self.cx.process_timers();
        self.cx.record_frame_phase(FramePhase::Events, start.elapsed());
        self.cx.emit_scheduled_events();
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {}
}

impl WindowModifiers for Application {
    fn title<T: ToStringLocalized>(mut self, title: impl Res<T> + Clone + 'static) -> Self {
        self.window_description.title = title.get_value(&self.cx.0).to_string_local(&self.cx.0);

        let getter_for_locale = title.clone();

        title.set_or_bind(&mut self.cx.0, move |cx, val| {
            let title_str = val.get_value(cx).to_string_local(cx);

            cx.emit(WindowEvent::SetTitle(title_str));
        });

        let locale = self.cx.0.environment().locale;
        locale.set_or_bind(&mut self.cx.0, move |cx, _| {
            let title = getter_for_locale.get_value(cx).to_string_local(cx);
            cx.emit(WindowEvent::SetTitle(title));
        });

        self
    }

    fn inner_size<S: Into<WindowSize>>(mut self, size: impl Res<S>) -> Self {
        self.window_description.inner_size = size.get_value(&self.cx.0).into();

        size.set_or_bind(&mut self.cx.0, |cx, size| {
            cx.emit(WindowEvent::SetSize(size.get_value(cx).into()));
        });

        self
    }

    fn min_inner_size<S: Into<WindowSize>>(mut self, size: impl Res<Option<S>>) -> Self {
        self.window_description.min_inner_size = size.get_value(&self.cx.0).map(|s| s.into());

        size.set_or_bind(&mut self.cx.0, |cx, size| {
            cx.emit(WindowEvent::SetMinSize(size.get_value(cx).map(|s| s.into())));
        });

        self
    }

    fn max_inner_size<S: Into<WindowSize>>(mut self, size: impl Res<Option<S>>) -> Self {
        self.window_description.max_inner_size = size.get_value(&self.cx.0).map(|s| s.into());

        size.set_or_bind(&mut self.cx.0, |cx, size| {
            cx.emit(WindowEvent::SetMaxSize(size.get_value(cx).map(|s| s.into())));
        });
        self
    }

    fn position<P: Into<WindowPosition>>(mut self, position: impl Res<P>) -> Self {
        self.window_description.position = Some(position.get_value(&self.cx.0).into());

        position.set_or_bind(&mut self.cx.0, |cx, size| {
            cx.emit(WindowEvent::SetPosition(size.get_value(cx).into()));
        });

        self
    }

    fn offset<P: Into<WindowPosition>>(mut self, offset: impl Res<P>) -> Self {
        self.window_description.offset = Some(offset.get_value(&self.cx.0).into());

        self
    }

    fn anchor<P: Into<Anchor>>(mut self, anchor: impl Res<P>) -> Self {
        self.window_description.anchor = Some(anchor.get_value(&self.cx.0).into());

        self
    }

    fn anchor_target<P: Into<AnchorTarget>>(mut self, anchor_target: impl Res<P>) -> Self {
        self.window_description.anchor_target = Some(anchor_target.get_value(&self.cx.0).into());

        self
    }

    fn parent_anchor<P: Into<Anchor>>(mut self, parent_anchor: impl Res<P>) -> Self {
        self.window_description.parent_anchor = Some(parent_anchor.get_value(&self.cx.0).into());

        self
    }

    fn resizable(mut self, flag: impl Res<bool>) -> Self {
        self.window_description.resizable = flag.get_value(&self.cx.0);

        flag.set_or_bind(&mut self.cx.0, |cx, flag| {
            cx.emit(WindowEvent::SetResizable(flag.get_value(cx)));
        });

        self
    }

    fn minimized(mut self, flag: impl Res<bool>) -> Self {
        self.window_description.minimized = flag.get_value(&self.cx.0);

        flag.set_or_bind(&mut self.cx.0, |cx, flag| {
            cx.emit(WindowEvent::SetMinimized(flag.get_value(cx)));
        });
        self
    }

    fn maximized(mut self, flag: impl Res<bool>) -> Self {
        self.window_description.maximized = flag.get_value(&self.cx.0);

        flag.set_or_bind(&mut self.cx.0, |cx, flag| {
            cx.emit(WindowEvent::SetMaximized(flag.get_value(cx)));
        });

        self
    }

    fn visible(mut self, flag: impl Res<bool>) -> Self {
        self.window_description.visible = flag.get_value(&self.cx.0);

        flag.set_or_bind(&mut self.cx.0, |cx, flag| {
            cx.emit(WindowEvent::SetVisible(flag.get_value(cx)));
        });

        self
    }

    fn transparent(mut self, flag: bool) -> Self {
        self.window_description.transparent = flag;

        self
    }

    fn decorations(mut self, flag: bool) -> Self {
        self.window_description.decorations = flag;

        self
    }

    fn always_on_top(mut self, flag: bool) -> Self {
        self.window_description.always_on_top = flag;
        self
    }

    fn vsync(mut self, flag: bool) -> Self {
        self.window_description.vsync = flag;

        self
    }

    fn icon(mut self, width: u32, height: u32, image: Vec<u8>) -> Self {
        self.window_description.icon = Some(image);
        self.window_description.icon_width = width;
        self.window_description.icon_height = height;

        self
    }

    fn on_close(self, _callback: impl Fn(&mut EventContext)) -> Self {
        self
    }

    fn on_create(self, _callback: impl Fn(&mut EventContext)) -> Self {
        self
    }

    fn enabled_window_buttons(mut self, window_buttons: WindowButtons) -> Self {
        self.window_description.enabled_window_buttons = window_buttons;

        self
    }
}

/// Window sizes are scaled by the user scale factor (winit's logical units only include the
/// system's DPI scaling). Positions are screen coordinates and are not.
fn apply_window_description(
    description: &WindowDescription,
    user_scale_factor: f64,
) -> WindowAttributes {
    let mut window_attributes = winit::window::Window::default_attributes();

    window_attributes = window_attributes
        .with_title(&description.title)
        .with_inner_size(scaled_size(description.inner_size, user_scale_factor));

    if let Some(min_inner_size) = description.min_inner_size {
        window_attributes =
            window_attributes.with_min_inner_size(scaled_size(min_inner_size, user_scale_factor));
    }

    if let Some(max_inner_size) = description.max_inner_size {
        window_attributes =
            window_attributes.with_max_inner_size(scaled_size(max_inner_size, user_scale_factor));
    }

    if let Some(position) = description.position {
        window_attributes =
            window_attributes.with_position(LogicalPosition::new(position.x, position.y));
    }

    window_attributes
        .with_resizable(description.resizable)
        .with_maximized(description.maximized)
        // Accesskit requires that the window start invisible until accesskit is initialized.
        .with_visible(false)
        .with_window_level(if description.always_on_top {
            WindowLevel::AlwaysOnTop
        } else {
            WindowLevel::Normal
        })
        .with_transparent(description.transparent)
        .with_decorations(description.decorations)
        .with_window_icon(description.icon.as_ref().map(|icon| {
            winit::window::Icon::from_rgba(
                icon.clone(),
                description.icon_width,
                description.icon_height,
            )
            .unwrap()
        }))
        .with_enabled_buttons(
            winit::window::WindowButtons::from_bits(description.enabled_window_buttons.bits())
                .unwrap(),
        )
}

#[allow(unused_variables)]
pub fn load_default_cursors(event_loop: &ActiveEventLoop) -> HashMap<CursorIcon, CustomCursor> {
    #[allow(unused_mut)]
    let mut custom_cursors = HashMap::new();

    #[cfg(target_os = "windows")]
    {
        let mut load_cursor = |cursor, bytes, x, y| {
            custom_cursors.insert(
                cursor,
                event_loop.create_custom_cursor(
                    CustomCursor::from_rgba(bytes, 32, 32, x, y)
                        .expect("Failed to create custom cursor"),
                ),
            );
        };

        load_cursor(
            CursorIcon::Alias, //
            include_bytes!("../resources/cursors/windows/aliasb"),
            0,
            0,
        );
        load_cursor(
            CursorIcon::Cell, //
            include_bytes!("../resources/cursors/windows/cell"),
            7,
            7,
        );
        load_cursor(
            CursorIcon::ColResize,
            include_bytes!("../resources/cursors/windows/col_resize"),
            10,
            8,
        );
        load_cursor(
            CursorIcon::Copy, //
            include_bytes!("../resources/cursors/windows/copy"),
            0,
            0,
        );
        load_cursor(
            CursorIcon::Grab, //
            include_bytes!("../resources/cursors/windows/grab"),
            6,
            0,
        );
        load_cursor(
            CursorIcon::Grabbing, //
            include_bytes!("../resources/cursors/windows/grabbing"),
            6,
            0,
        );
        load_cursor(
            CursorIcon::RowResize, //
            include_bytes!("../resources/cursors/windows/row_resize"),
            9,
            10,
        );
        load_cursor(
            CursorIcon::VerticalText, //
            include_bytes!("../resources/cursors/windows/vertical_text"),
            9,
            3,
        );
        load_cursor(
            CursorIcon::ZoomIn, //
            include_bytes!("../resources/cursors/windows/zoom_in"),
            6,
            6,
        );
        load_cursor(
            CursorIcon::ZoomOut, //
            include_bytes!("../resources/cursors/windows/zoom_out"),
            6,
            6,
        );
    }

    custom_cursors
}

/// How early a rate-limited frame may start relative to its slot.
const FRAME_SLOT_SLACK: Duration = Duration::from_micros(500);

/// Where a frame slot starts when the slots are locked to the display, as a
/// fraction of the period after the display link's vblank time.
///
/// Calibrated by measurement (macOS 26, M1, 59.95 Hz display, OpenGL on Metal,
/// Metal System Trace): with slots at phase p the app's GPU commit lands at
/// p - 0.32 of the window server's VSYNC interval (wake-up and ~1.5 ms of work
/// after the slot, minus the display link's offset from VSYNC). Frames whose
/// commit lands within ~0.1 of the VSYNC boundary are shown a vblank late or
/// replaced; 0.2-0.9 is clean. 0.85 puts the commit mid-interval (0.53 measured,
/// sd 0.03), so jitter has the most room either way.
#[cfg(target_os = "macos")]
const FRAME_SLOT_PHASE: f64 = 0.85;

/// How often the display's refresh rate is re-read while the event loop runs.
const REFRESH_RATE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Refresh rate (Hz) of the monitor the window is on, if the platform reports it.
///
/// winit reports the display mode's rate, which is rounded: a "60 Hz" mode often
/// really refreshes at 59.94 or 59.951 Hz. Frame slots paced from the rounded
/// value drift against the display (see `frame_slot_rate`), so the precise rate
/// is used where the platform has it: the display link's nominal refresh period
/// on macOS, DWM's composition rate on Windows.
fn monitor_refresh_rate(window: &winit::window::Window) -> Option<f64> {
    let monitor = window.current_monitor()?;
    let rounded = monitor.refresh_rate_millihertz().map(|mhz| mhz as f64 / 1000.0);
    #[cfg(target_os = "macos")]
    let precise = {
        use winit::platform::macos::MonitorHandleExtMacOS;
        display_link_refresh_rate(monitor.native_id())
    };
    #[cfg(target_os = "windows")]
    let precise = dwm_refresh_rate();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let precise = None;
    refine_refresh_rate(rounded, precise)
}

/// Takes the precise rate when it agrees with the mode's rounded rate (within
/// 1 Hz): DWM's rate is for the composition, not per monitor, and must not
/// replace another display's rate.
fn refine_refresh_rate(rounded: Option<f64>, precise: Option<f64>) -> Option<f64> {
    let precise = precise.filter(|p| p.is_finite() && *p > 0.0);
    match (rounded, precise) {
        (Some(r), Some(p)) if (r - p).abs() < 1.0 => Some(p),
        (None, p) => p,
        (r, _) => r,
    }
}

/// `CVDisplayLinkGetNominalOutputVideoRefreshPeriod` of a display, in Hz: an
/// exact ratio from the display timing (e.g. 24000000 / 400330 = 59.951 Hz).
/// CoreVideo is part of macOS, so no new dependency. CVDisplayLink is deprecated
/// since macOS 15 in favour of view/screen display links, which only deliver
/// callbacks; this only reads the period and never starts the link.
#[cfg(target_os = "macos")]
fn display_link_refresh_rate(display_id: u32) -> Option<f64> {
    use std::ffi::c_void;

    #[repr(C)]
    struct CVTime {
        time_value: i64,
        time_scale: i32,
        flags: i32,
    }
    const CV_TIME_IS_INDEFINITE: i32 = 1;
    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        fn CVDisplayLinkCreateWithCGDisplay(display: u32, link: *mut *mut c_void) -> i32;
        fn CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link: *mut c_void) -> CVTime;
        fn CVDisplayLinkRelease(link: *mut c_void);
    }

    // SAFETY: the link is created, read and released here; it is never started.
    let period = unsafe {
        let mut link = std::ptr::null_mut();
        if CVDisplayLinkCreateWithCGDisplay(display_id, &mut link) != 0 || link.is_null() {
            return None;
        }
        let period = CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link);
        CVDisplayLinkRelease(link);
        period
    };
    (period.flags & CV_TIME_IS_INDEFINITE == 0 && period.time_value > 0 && period.time_scale > 0)
        .then(|| period.time_scale as f64 / period.time_value as f64)
}

/// DWM's composition refresh rate (`DWM_TIMING_INFO::rateRefresh`), in Hz.
#[cfg(target_os = "windows")]
#[allow(clippy::zero_ptr)]
fn dwm_refresh_rate() -> Option<f64> {
    use windows_sys::Win32::{
        Foundation::HWND,
        Graphics::Dwm::{DWM_TIMING_INFO, DwmGetCompositionTimingInfo},
    };
    // SAFETY: plain out-parameter call; cbSize is set as the API requires.
    let rate = unsafe {
        let mut info: DWM_TIMING_INFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<DWM_TIMING_INFO>() as u32;
        // Since Windows 8.1 the window handle must be null. (`0 as HWND` like
        // `set_cloak`'s cast: HWND is an integer in some windows-sys versions.)
        if DwmGetCompositionTimingInfo(0 as HWND, &mut info) != 0 {
            return None;
        }
        info.rateRefresh
    };
    (rate.uiDenominator != 0).then(|| rate.uiNumerator as f64 / rate.uiDenominator as f64)
}
