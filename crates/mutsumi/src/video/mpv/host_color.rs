use std::{
    cell::RefCell,
    rc::Rc,
    sync::atomic::{
        AtomicU64,
        Ordering,
    },
    time::Duration,
};

use gdk_wayland::prelude::*;
use gtk::{
    gdk,
    glib,
};
use wayland_client::{
    Connection,
    Dispatch,
    EventQueue,
    Proxy,
    QueueHandle,
    protocol::{
        wl_registry,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_feedback_v1::{
        self,
        WpColorManagementSurfaceFeedbackV1,
    },
    wp_color_manager_v1::{
        self,
        WpColorManagerV1,
    },
    wp_image_description_info_v1::{
        self,
        WpImageDescriptionInfoV1,
    },
    wp_image_description_v1::{
        self,
        WpImageDescriptionV1,
    },
};

use super::color::{
    ColorDescription,
    PREFERRED_COLOR,
};

/// Observes the real GTK surface without taking over GTK's surface color state.
/// The owner must drop this object when that GDK surface is unrealized.
pub(crate) struct HostColorFeedback {
    attachment: u64,
    runtime: Rc<RefCell<Runtime>>,
}

const DISPLAY_RUNTIME_KEY: &str = "mutsumi-host-color-feedback-runtime-v1";
static NEXT_ATTACHMENT: AtomicU64 = AtomicU64::new(1);
static ACTIVE_ATTACHMENT: AtomicU64 = AtomicU64::new(0);

struct Runtime {
    connection: Connection,
    registry: Option<wl_registry::WlRegistry>,
    queue: EventQueue<FeedbackState>,
    state: FeedbackState,
    source: Option<glib::SourceId>,
    failed: bool,
    closed: bool,
}

struct FeedbackState {
    surface: Option<WlSurface>,
    attachment: Option<u64>,
    manager_global: Option<u32>,
    manager: Option<WpColorManagerV1>,
    feedback: Option<WpColorManagementSurfaceFeedbackV1>,
    image: Option<WpImageDescriptionV1>,
    info: Option<WpImageDescriptionInfoV1>,
    generation: u64,
    pending: ColorDescription,
    has_icc: bool,
}

fn publish(description: ColorDescription) {
    PREFERRED_COLOR.send_if_modified(|current| {
        if *current == description {
            return false;
        }
        *current = description;
        true
    });
}

impl HostColorFeedback {
    pub(crate) fn new(surface: &gdk::Surface) -> Option<Self> {
        // A newly realized window must not inherit another window's HDR target.
        let attachment = NEXT_ATTACHMENT.fetch_add(1, Ordering::Relaxed);
        ACTIVE_ATTACHMENT.store(attachment, Ordering::Relaxed);
        publish(ColorDescription::srgb());
        let surface = surface.downcast_ref::<gdk_wayland::WaylandSurface>()?;
        let display = surface.display();
        if display.is_closed() {
            return None;
        }
        let wl_surface = surface.wl_surface()?;
        // SAFETY: this private key is written only below with this exact type.
        // All access is on the GTK thread; clone before releasing the borrow.
        let cached = unsafe {
            display
                .data::<Rc<RefCell<Runtime>>>(DISPLAY_RUNTIME_KEY)
                .map(|runtime| runtime.as_ref().clone())
        };
        let runtime = if let Some(runtime) = cached {
            runtime
        } else {
            let connection = Connection::from_backend(wl_surface.backend().upgrade()?);
            let queue = connection.new_event_queue();
            // A registry has no server-side destructor. Create only one for
            // this display connection and retain it across player realizations.
            let registry = connection.display().get_registry(&queue.handle(), ());
            let runtime = Rc::new(RefCell::new(Runtime {
                connection,
                registry: Some(registry),
                queue,
                state: FeedbackState {
                    surface: None,
                    attachment: None,
                    manager_global: None,
                    manager: None,
                    feedback: None,
                    image: None,
                    info: None,
                    generation: 0,
                    pending: ColorDescription::default(),
                    has_icc: false,
                },
                source: None,
                failed: false,
                closed: false,
            }));
            let weak = Rc::downgrade(&runtime);
            display.connect_closed(move |_, _| {
                if let Some(runtime) = weak.upgrade() {
                    // GDK emits closed before disposing its Wayland connection.
                    runtime.borrow_mut().shutdown();
                }
            });
            // SAFETY: same private key/type as the read above. The runtime has
            // no owning GdkDisplay reference, so this does not create a cycle.
            unsafe { display.set_data(DISPLAY_RUNTIME_KEY, runtime.clone()) };
            runtime
        };

        {
            let mut runtime = runtime.borrow_mut();
            if runtime.closed || runtime.failed {
                return None;
            }
            // Process globals queued while no player was realized before
            // reusing the cached manager for the new surface.
            let Runtime { queue, state, .. } = &mut *runtime;
            if let Err(error) = queue.dispatch_pending(state) {
                tracing::warn!(%error, "Wayland color feedback reattach failed");
                runtime.failed = true;
                return None;
            }
            let handle = runtime.queue.handle();
            runtime.state.clear_feedback();
            runtime.state.surface = Some(wl_surface);
            runtime.state.attachment = Some(attachment);
            runtime.state.create_feedback(&handle);
        }
        Runtime::start_dispatch(&runtime);
        Some(Self {
            attachment,
            runtime,
        })
    }
}

impl Runtime {
    fn start_dispatch(runtime: &Rc<RefCell<Self>>) {
        if runtime.borrow().source.is_some() {
            return;
        }
        let weak = Rc::downgrade(runtime);
        let source = glib::timeout_add_local(Duration::from_millis(50), move || {
            let Some(runtime) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let mut runtime = runtime.borrow_mut();
            if runtime.failed {
                return glib::ControlFlow::Continue;
            }
            let Runtime { queue, state, .. } = &mut *runtime;
            // Guest queues share libwayland with GTK. Never read the socket or
            // block here; GTK enqueues events and this dispatches only ours.
            let result = queue.dispatch_pending(state);
            if let Err(error) = result {
                tracing::warn!(%error, "Wayland color feedback dispatch failed");
                runtime.failed = true;
                runtime.state.publish(ColorDescription::srgb());
            } else if let Err(error) = runtime.connection.flush() {
                // A full socket buffer is temporary; GTK will also flush it.
                if !matches!(&error, wayland_client::backend::WaylandError::Io(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock)
                {
                    tracing::warn!(%error, "Wayland color feedback flush failed");
                    runtime.failed = true;
                    runtime.state.publish(ColorDescription::srgb());
                }
            }
            glib::ControlFlow::Continue
        });
        runtime.borrow_mut().source = Some(source);
    }

    fn stop_dispatch(&mut self) {
        if let Some(source) = self.source.take() {
            source.remove();
        }
    }

    fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        self.stop_dispatch();
        self.state.publish(ColorDescription::srgb());
        self.state.clear_manager();
        self.state.surface = None;
        self.state.attachment = None;
        if let Some(registry) = self.registry.take() {
            let _ = self.connection.backend().destroy_object(&registry.id());
        }
        let _ = self.connection.flush();
        self.closed = true;
    }
}

impl Drop for HostColorFeedback {
    fn drop(&mut self) {
        let mut runtime = self.runtime.borrow_mut();
        if runtime.closed || runtime.state.attachment != Some(self.attachment) {
            return;
        }
        // A stale owner must not tear down a newly realized player's feedback.
        runtime.stop_dispatch();
        runtime.state.publish(ColorDescription::srgb());
        runtime.state.clear_feedback();
        runtime.state.surface = None;
        runtime.state.attachment = None;
        let _ = runtime.connection.flush();
        let _ = ACTIVE_ATTACHMENT.compare_exchange(
            self.attachment,
            0,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl FeedbackState {
    fn publish(&self, description: ColorDescription) {
        if self.attachment == Some(ACTIVE_ATTACHMENT.load(Ordering::Relaxed)) {
            publish(description);
        }
    }

    fn clear_image(&mut self) {
        if let Some(info) = self.info.take()
            && let Some(backend) = info.backend().upgrade()
        {
            // An information object has only a server-side done destructor.
            // Locally discard it when a newer preferred description wins.
            let _ = backend.destroy_object(&info.id());
        }
        if let Some(image) = self.image.take() {
            image.destroy();
        }
    }

    fn clear_feedback(&mut self) {
        self.clear_image();
        if let Some(feedback) = self.feedback.take() {
            feedback.destroy();
        }
        self.generation = self.generation.wrapping_add(1);
    }

    fn clear_manager(&mut self) {
        self.clear_feedback();
        if let Some(manager) = self.manager.take() {
            manager.destroy();
        }
        self.manager_global = None;
    }

    fn create_feedback(&mut self, handle: &QueueHandle<Self>) {
        if let (Some(manager), Some(surface), Some(attachment)) =
            (&self.manager, &self.surface, self.attachment)
        {
            self.feedback = Some(manager.get_surface_feedback(surface, handle, attachment));
            self.request_preferred(handle);
        }
    }

    fn request_preferred(&mut self, handle: &QueueHandle<Self>) {
        self.clear_image();
        self.generation = self.generation.wrapping_add(1);
        self.pending = ColorDescription::default();
        self.has_icc = false;
        if let Some(feedback) = &self.feedback {
            self.image = Some(feedback.get_preferred(handle, self.generation));
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for FeedbackState {
    fn event(
        state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(),
        _: &Connection, handle: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == WpColorManagerV1::interface().name
                && version >= 1
                && state.manager.is_none() =>
            {
                let manager: WpColorManagerV1 = registry.bind(name, 1, handle, ());
                state.manager = Some(manager);
                state.manager_global = Some(name);
                state.create_feedback(handle);
            }
            wl_registry::Event::GlobalRemove { name } if state.manager_global == Some(name) => {
                state.clear_manager();
                state.publish(ColorDescription::srgb());
            }
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagerV1, ()> for FeedbackState {
    fn event(
        _: &mut Self, _: &WpColorManagerV1, _: wp_color_manager_v1::Event, _: &(), _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpColorManagementSurfaceFeedbackV1, u64> for FeedbackState {
    fn event(
        state: &mut Self, _: &WpColorManagementSurfaceFeedbackV1,
        event: wp_color_management_surface_feedback_v1::Event, attachment: &u64, _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if state.attachment == Some(*attachment)
            && matches!(
                event,
                wp_color_management_surface_feedback_v1::Event::PreferredChanged { .. }
            )
        {
            state.request_preferred(handle);
        }
    }
}

impl Dispatch<WpImageDescriptionV1, u64> for FeedbackState {
    fn event(
        state: &mut Self, image: &WpImageDescriptionV1, event: wp_image_description_v1::Event,
        generation: &u64, _: &Connection, handle: &QueueHandle<Self>,
    ) {
        if *generation != state.generation {
            return;
        }
        match event {
            wp_image_description_v1::Event::Ready { .. } => {
                state.info = Some(image.get_information(handle, *generation));
            }
            wp_image_description_v1::Event::Failed { cause, .. } => {
                tracing::debug!(?cause, "Wayland preferred color description unavailable");
                state.clear_image();
                state.publish(ColorDescription::srgb());
            }
            _ => {}
        }
    }
}

impl Dispatch<WpImageDescriptionInfoV1, u64> for FeedbackState {
    fn event(
        state: &mut Self, _: &WpImageDescriptionInfoV1, event: wp_image_description_info_v1::Event,
        generation: &u64, _: &Connection, _: &QueueHandle<Self>,
    ) {
        if *generation != state.generation {
            return;
        }
        use wp_image_description_info_v1::Event;
        match event {
            Event::Primaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                state.pending.primaries = Some([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]);
            }
            Event::PrimariesNamed { primaries } => {
                state.pending.primaries_named = Some(primaries.into())
            }
            Event::TfPower { eexp } => state.pending.tf_power = Some(eexp),
            Event::TfNamed { tf } => state.pending.tf_named = Some(tf.into()),
            Event::Luminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                state.pending.luminances = Some((min_lum, max_lum, reference_lum));
            }
            Event::TargetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                state.pending.target_primaries = Some([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]);
            }
            Event::TargetLuminance { min_lum, max_lum } => {
                state.pending.target_luminance = Some((min_lum, max_lum));
            }
            Event::TargetMaxCll { max_cll } => state.pending.max_cll = Some(max_cll),
            Event::TargetMaxFall { max_fall } => state.pending.max_fall = Some(max_fall),
            Event::IccFile { .. } => state.has_icc = true,
            Event::Done => {
                // The server-side destructor has already released this proxy.
                state.info.take();
                state.clear_image();
                if state.has_icc {
                    tracing::debug!("ICC-only Wayland color feedback uses the SDR fallback");
                    state.publish(ColorDescription::srgb());
                } else {
                    match state.pending.to_gdk_color_state() {
                        Ok(_) => {
                            tracing::debug!(description = ?state.pending, "GTK surface preferred color changed");
                            state.publish(state.pending.clone());
                        }
                        Err(error) => {
                            tracing::debug!(%error, "Unsupported Wayland preferred color uses the SDR fallback");
                            state.publish(ColorDescription::srgb());
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
