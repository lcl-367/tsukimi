//! Color management for the virtual surfaces consumed by GTK.

use std::{
    cell::{
        Cell,
        RefCell,
    },
    collections::HashMap,
    rc::{
        Rc,
        Weak,
    },
};

use wl_proxy::{
    object::{
        Object,
        ObjectCoreApi,
        ObjectUtils,
    },
    protocols::{
        color_management_v1::{
            wp_color_management_output_v1::{
                WpColorManagementOutputV1,
                WpColorManagementOutputV1Handler,
            },
            wp_color_management_surface_feedback_v1::{
                WpColorManagementSurfaceFeedbackV1,
                WpColorManagementSurfaceFeedbackV1Error,
                WpColorManagementSurfaceFeedbackV1Handler,
            },
            wp_color_management_surface_v1::{
                WpColorManagementSurfaceV1,
                WpColorManagementSurfaceV1Error,
                WpColorManagementSurfaceV1Handler,
            },
            wp_color_manager_v1::{
                WpColorManagerV1,
                WpColorManagerV1Error,
                WpColorManagerV1Feature,
                WpColorManagerV1Handler,
                WpColorManagerV1Primaries,
                WpColorManagerV1RenderIntent,
                WpColorManagerV1TransferFunction,
            },
            wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1,
            wp_image_description_creator_params_v1::{
                WpImageDescriptionCreatorParamsV1,
                WpImageDescriptionCreatorParamsV1Error,
                WpImageDescriptionCreatorParamsV1Handler,
            },
            wp_image_description_info_v1::WpImageDescriptionInfoV1,
            wp_image_description_v1::{
                WpImageDescriptionV1,
                WpImageDescriptionV1Cause,
                WpImageDescriptionV1Error,
                WpImageDescriptionV1Handler,
            },
        },
        wayland::{
            wl_callback::{
                WlCallback,
                WlCallbackHandler,
            },
            wl_output::{
                WlOutput,
                WlOutputHandler,
            },
            wl_surface::WlSurface,
        },
    },
};

use super::{
    super::color::ColorDescription,
    SharedState,
};

// These are Wayland enum values, not CICP numbers. Keep the advertised subset
// in step with ColorDescription::cicp(), without calling GTK on this thread.
const PRIMARIES: &[u32] = &[1, 3, 4, 6, 9];
const TRANSFERS: &[u32] = &[2, 3, 5, 9, 11, 13];

struct SurfaceColor {
    object_id: Option<u64>,
    pending: Option<ColorDescription>,
    current: ColorDescription,
}

impl Default for SurfaceColor {
    fn default() -> Self {
        Self {
            object_id: None,
            pending: None,
            current: ColorDescription::srgb(),
        }
    }
}

pub(super) struct ColorManagementState {
    preferred: ColorDescription,
    preferred_identity: u32,
    next_identity: u32,
    descriptions: HashMap<u64, ColorDescription>,
    surfaces: HashMap<u64, SurfaceColor>,
    feedback: HashMap<u64, (u64, Weak<WpColorManagementSurfaceFeedbackV1>)>,
    outputs: HashMap<u64, (Weak<WpColorManagementOutputV1>, Weak<WlOutput>)>,
    output_globals: HashMap<u32, Rc<Cell<bool>>>,
}

impl ColorManagementState {
    pub(super) fn new(preferred: ColorDescription) -> Self {
        Self {
            preferred,
            preferred_identity: 1,
            next_identity: 2,
            descriptions: HashMap::new(),
            surfaces: HashMap::new(),
            feedback: HashMap::new(),
            outputs: HashMap::new(),
            output_globals: HashMap::new(),
        }
    }

    fn allocate_identity(&mut self) -> u32 {
        let identity = self.next_identity;
        self.next_identity = identity.wrapping_add(1).max(1);
        identity
    }

    pub(super) fn set_preferred(&mut self, preferred: ColorDescription) {
        if self.preferred == preferred {
            return;
        }
        self.preferred = preferred;
        self.preferred_identity = self.allocate_identity();
        let identity = self.preferred_identity;
        self.feedback.retain(|_, (_, feedback)| {
            if let Some(feedback) = feedback.upgrade() {
                feedback.send_preferred_changed(identity);
                true
            } else {
                false
            }
        });
        self.outputs.retain(|_, (output, wl_output)| {
            if let Some(output) = output.upgrade() {
                if !output.get_handler_ref::<OutputHandler>().live.get() {
                    return false;
                }
                output.send_image_description_changed();
                if let Some(wl_output) = wl_output.upgrade()
                    && wl_output.client_id().is_some()
                    && wl_output.version() >= 2
                {
                    wl_output.send_done();
                }
                true
            } else {
                false
            }
        });
    }

    pub(super) fn add_output_global(&mut self, name: u32) -> Rc<Cell<bool>> {
        Rc::clone(
            self.output_globals
                .entry(name)
                .or_insert_with(|| Rc::new(Cell::new(true))),
        )
    }

    pub(super) fn remove_output_global(&mut self, name: u32) {
        if let Some(live) = self.output_globals.remove(&name) {
            live.set(false);
        }
    }

    pub(super) fn current(&self, surface_id: u64) -> ColorDescription {
        self.surfaces
            .get(&surface_id)
            .map(|surface| surface.current.clone())
            .unwrap_or_else(ColorDescription::srgb)
    }

    pub(super) fn commit(&mut self, surface_id: u64) {
        if let Some(surface) = self.surfaces.get_mut(&surface_id)
            && let Some(pending) = surface.pending.take()
        {
            surface.current = pending;
        }
    }

    pub(super) fn remove_surface(&mut self, surface_id: u64) {
        self.surfaces.remove(&surface_id);
        self.feedback.retain(|_, (id, _)| *id != surface_id);
    }
}

// The global outlives individual wl_output bindings, and may be advertised by
// several registries with different downstream names.
pub(super) struct OutputBinding {
    pub(super) live: Rc<Cell<bool>>,
}

impl WlOutputHandler for OutputBinding {}

pub(super) fn bind_manager(manager: &Rc<WpColorManagerV1>, state: Rc<RefCell<SharedState>>) {
    manager.set_forward_to_server(false);
    manager.set_handler(ManagerHandler { state });
    // Bind is handled locally, while earlier display.sync requests may still
    // be pending upstream. Do not let the local capability events overtake
    // those replies: WSI clients may install their listener after the registry
    // roundtrip returns. A server-side barrier preserves request ordering
    // without depending on timing or blocking the proxy event loop.
    let callback = manager.state().display().new_send_sync();
    callback.set_forward_to_client(false);
    callback.set_handler(ManagerInitialization {
        manager: Rc::downgrade(manager),
    });
}

struct ManagerInitialization {
    manager: Weak<WpColorManagerV1>,
}

impl WlCallbackHandler for ManagerInitialization {
    fn handle_done(&mut self, _slf: &Rc<WlCallback>, _callback_data: u32) {
        if let Some(manager) = self.manager.upgrade()
            && manager.client_id().is_some()
        {
            send_capabilities(&manager);
        }
        // This callback only has an upstream ID. The compositor destroys it
        // and sends delete_id; no downstream done/delete_id is appropriate.
    }
}

fn send_capabilities(manager: &Rc<WpColorManagerV1>) {
    manager.send_supported_intent(WpColorManagerV1RenderIntent::PERCEPTUAL);
    manager.send_supported_feature(WpColorManagerV1Feature::PARAMETRIC);
    manager.send_supported_feature(WpColorManagerV1Feature::SET_MASTERING_DISPLAY_PRIMARIES);
    for &primary in PRIMARIES {
        manager.send_supported_primaries_named(WpColorManagerV1Primaries(primary));
    }
    for &transfer in TRANSFERS {
        manager.send_supported_tf_named(WpColorManagerV1TransferFunction(transfer));
    }
    manager.send_done();
}

struct ManagerHandler {
    state: Rc<RefCell<SharedState>>,
}

impl WpColorManagerV1Handler for ManagerHandler {
    fn handle_destroy(&mut self, slf: &Rc<WpColorManagerV1>) {
        slf.delete_id();
    }

    fn handle_get_output(
        &mut self, _slf: &Rc<WpColorManagerV1>, id: &Rc<WpColorManagementOutputV1>,
        output: &Rc<WlOutput>,
    ) {
        // Our compositor's output is the GTK presentation target, rather than
        // an independently mapped mpv window on a physical wl_output.
        id.set_forward_to_server(false);
        id.set_handler(OutputHandler {
            state: Rc::clone(&self.state),
            live: Rc::clone(&output.get_handler_ref::<OutputBinding>().live),
        });
        self.state
            .borrow_mut()
            .color_management
            .outputs
            .insert(id.unique_id(), (Rc::downgrade(id), Rc::downgrade(output)));
    }

    fn handle_get_surface(
        &mut self, slf: &Rc<WpColorManagerV1>, id: &Rc<WpColorManagementSurfaceV1>,
        surface: &Rc<WlSurface>,
    ) {
        let mut state = self.state.borrow_mut();
        let color = state
            .color_management
            .surfaces
            .entry(surface.unique_id())
            .or_default();
        if color.object_id.is_some() {
            post_error(
                slf,
                WpColorManagerV1Error::SURFACE_EXISTS.0,
                "surface already has color management",
            );
            return;
        }
        color.object_id = Some(id.unique_id());
        id.set_forward_to_server(false);
        id.set_handler(SurfaceHandler {
            state: Rc::clone(&self.state),
            surface_id: surface.unique_id(),
        });
    }

    fn handle_get_surface_feedback(
        &mut self, _slf: &Rc<WpColorManagerV1>, id: &Rc<WpColorManagementSurfaceFeedbackV1>,
        surface: &Rc<WlSurface>,
    ) {
        id.set_forward_to_server(false);
        id.set_handler(FeedbackHandler {
            state: Rc::clone(&self.state),
        });
        self.state
            .borrow_mut()
            .color_management
            .feedback
            .insert(id.unique_id(), (surface.unique_id(), Rc::downgrade(id)));
    }

    fn handle_create_parametric_creator(
        &mut self, _slf: &Rc<WpColorManagerV1>, obj: &Rc<WpImageDescriptionCreatorParamsV1>,
    ) {
        obj.set_forward_to_server(false);
        obj.set_handler(ParamsHandler {
            state: Rc::clone(&self.state),
            description: ColorDescription::default(),
        });
    }

    fn handle_create_icc_creator(
        &mut self, slf: &Rc<WpColorManagerV1>, _obj: &Rc<WpImageDescriptionCreatorIccV1>,
    ) {
        post_error(
            slf,
            WpColorManagerV1Error::UNSUPPORTED_FEATURE.0,
            "ICC image descriptions are not supported",
        );
    }

    fn handle_create_windows_scrgb(
        &mut self, slf: &Rc<WpColorManagerV1>, _description: &Rc<WpImageDescriptionV1>,
    ) {
        post_error(
            slf,
            WpColorManagerV1Error::UNSUPPORTED_FEATURE.0,
            "Windows scRGB image descriptions are not supported",
        );
    }
}

struct SurfaceHandler {
    state: Rc<RefCell<SharedState>>,
    surface_id: u64,
}

impl SurfaceHandler {
    fn set_pending(&self, slf: &Rc<WpColorManagementSurfaceV1>, description: ColorDescription) {
        let mut state = self.state.borrow_mut();
        let Some(surface) = state.color_management.surfaces.get_mut(&self.surface_id) else {
            post_error(
                slf,
                WpColorManagementSurfaceV1Error::INERT.0,
                "underlying surface was destroyed",
            );
            return;
        };
        surface.pending = Some(description);
    }
}

impl WpColorManagementSurfaceV1Handler for SurfaceHandler {
    fn handle_destroy(&mut self, slf: &Rc<WpColorManagementSurfaceV1>) {
        if let Some(surface) = self
            .state
            .borrow_mut()
            .color_management
            .surfaces
            .get_mut(&self.surface_id)
        {
            surface.object_id = None;
            surface.pending = Some(ColorDescription::srgb());
        }
        slf.delete_id();
    }

    fn handle_set_image_description(
        &mut self, slf: &Rc<WpColorManagementSurfaceV1>, description: &Rc<WpImageDescriptionV1>,
        intent: WpColorManagerV1RenderIntent,
    ) {
        if intent != WpColorManagerV1RenderIntent::PERCEPTUAL {
            post_error(
                slf,
                WpColorManagementSurfaceV1Error::RENDER_INTENT.0,
                "unsupported rendering intent",
            );
            return;
        }
        let color = self
            .state
            .borrow()
            .color_management
            .descriptions
            .get(&description.unique_id())
            .cloned();
        let Some(color) = color else {
            post_error(
                slf,
                WpColorManagementSurfaceV1Error::IMAGE_DESCRIPTION.0,
                "image description is not ready",
            );
            return;
        };
        // Copy now: clients may destroy the description before surface.commit.
        self.set_pending(slf, color);
    }

    fn handle_unset_image_description(&mut self, slf: &Rc<WpColorManagementSurfaceV1>) {
        self.set_pending(slf, ColorDescription::srgb());
    }
}

struct FeedbackHandler {
    state: Rc<RefCell<SharedState>>,
}

impl WpColorManagementSurfaceFeedbackV1Handler for FeedbackHandler {
    fn handle_destroy(&mut self, slf: &Rc<WpColorManagementSurfaceFeedbackV1>) {
        self.state
            .borrow_mut()
            .color_management
            .feedback
            .remove(&slf.unique_id());
        slf.delete_id();
    }

    fn handle_get_preferred(
        &mut self, slf: &Rc<WpColorManagementSurfaceFeedbackV1>,
        description: &Rc<WpImageDescriptionV1>,
    ) {
        if !self
            .state
            .borrow()
            .color_management
            .feedback
            .contains_key(&slf.unique_id())
        {
            post_error(
                slf,
                WpColorManagementSurfaceFeedbackV1Error::INERT.0,
                "underlying surface was destroyed",
            );
            return;
        }
        create_preferred(&self.state, description);
    }

    fn handle_get_preferred_parametric(
        &mut self, slf: &Rc<WpColorManagementSurfaceFeedbackV1>,
        description: &Rc<WpImageDescriptionV1>,
    ) {
        self.handle_get_preferred(slf, description);
    }
}

struct OutputHandler {
    state: Rc<RefCell<SharedState>>,
    live: Rc<Cell<bool>>,
}

impl WpColorManagementOutputV1Handler for OutputHandler {
    fn handle_destroy(&mut self, slf: &Rc<WpColorManagementOutputV1>) {
        self.state
            .borrow_mut()
            .color_management
            .outputs
            .remove(&slf.unique_id());
        slf.delete_id();
    }

    fn handle_get_image_description(
        &mut self, _slf: &Rc<WpColorManagementOutputV1>, description: &Rc<WpImageDescriptionV1>,
    ) {
        if self.live.get() {
            create_preferred(&self.state, description);
        } else {
            description.set_forward_to_server(false);
            description.set_handler(DescriptionHandler {
                state: Rc::clone(&self.state),
                description: None,
                allow_information: false,
            });
            description.send_failed(WpImageDescriptionV1Cause::NO_OUTPUT, "output was removed");
        }
    }
}

fn create_preferred(state: &Rc<RefCell<SharedState>>, object: &Rc<WpImageDescriptionV1>) {
    let (description, identity) = {
        let state = state.borrow();
        (
            state.color_management.preferred.clone(),
            state.color_management.preferred_identity,
        )
    };
    create_description(state, object, description, identity, true);
}

fn create_description(
    state: &Rc<RefCell<SharedState>>, object: &Rc<WpImageDescriptionV1>,
    description: ColorDescription, identity: u32, allow_information: bool,
) {
    state
        .borrow_mut()
        .color_management
        .descriptions
        .insert(object.unique_id(), description.clone());
    object.set_forward_to_server(false);
    object.set_handler(DescriptionHandler {
        state: Rc::clone(state),
        description: Some(description),
        allow_information,
    });
    object.send_ready(identity);
}

struct DescriptionHandler {
    state: Rc<RefCell<SharedState>>,
    description: Option<ColorDescription>,
    allow_information: bool,
}

impl WpImageDescriptionV1Handler for DescriptionHandler {
    fn handle_destroy(&mut self, slf: &Rc<WpImageDescriptionV1>) {
        self.state
            .borrow_mut()
            .color_management
            .descriptions
            .remove(&slf.unique_id());
        slf.delete_id();
    }

    fn handle_get_information(
        &mut self, slf: &Rc<WpImageDescriptionV1>, information: &Rc<WpImageDescriptionInfoV1>,
    ) {
        let Some(description) = self.description.as_ref() else {
            post_error(
                slf,
                WpImageDescriptionV1Error::NOT_READY.0,
                "image description is not ready",
            );
            return;
        };
        if !self.allow_information {
            post_error(
                slf,
                WpImageDescriptionV1Error::NO_INFORMATION.0,
                "client-created image description has no information interface",
            );
            return;
        }
        information.set_forward_to_server(false);
        send_information(information, description);
        information.send_done();
        information.delete_id();
    }
}

struct ParamsHandler {
    state: Rc<RefCell<SharedState>>,
    description: ColorDescription,
}

impl ParamsHandler {
    fn unsupported(slf: &Rc<WpImageDescriptionCreatorParamsV1>) {
        post_error(
            slf,
            WpImageDescriptionCreatorParamsV1Error::UNSUPPORTED_FEATURE.0,
            "parameter was not advertised as supported",
        );
    }
}

impl WpImageDescriptionCreatorParamsV1Handler for ParamsHandler {
    fn handle_create(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, object: &Rc<WpImageDescriptionV1>,
    ) {
        if self.description.primaries_named.is_none() || self.description.tf_named.is_none() {
            post_error(
                slf,
                WpImageDescriptionCreatorParamsV1Error::INCOMPLETE_SET.0,
                "primaries and transfer function are required",
            );
            slf.delete_id();
            return;
        }
        if !valid_light_levels(&self.description) {
            post_error(
                slf,
                WpImageDescriptionCreatorParamsV1Error::INVALID_LUMINANCE.0,
                "content light levels exceed the image description's luminance range",
            );
            slf.delete_id();
            return;
        }
        let encoding = self.description.cicp().and_then(|encoding| {
            if supported_mastering_primaries(&self.description) {
                Ok(encoding)
            } else {
                Err(
                    "mastering display primaries are invalid or exceed the supported color gamut"
                        .into(),
                )
            }
        });
        match encoding {
            Ok(_) => {
                let identity = self.state.borrow_mut().color_management.allocate_identity();
                create_description(
                    &self.state,
                    object,
                    self.description.clone(),
                    identity,
                    false,
                );
            }
            Err(reason) => {
                object.set_forward_to_server(false);
                object.set_handler(DescriptionHandler {
                    state: Rc::clone(&self.state),
                    description: None,
                    allow_information: false,
                });
                object.send_failed(WpImageDescriptionV1Cause::UNSUPPORTED, &reason);
            }
        }
        slf.delete_id();
    }

    fn handle_set_tf_named(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>,
        tf: WpColorManagerV1TransferFunction,
    ) {
        if self.description.tf_named.is_some() {
            post_error(
                slf,
                WpImageDescriptionCreatorParamsV1Error::ALREADY_SET.0,
                "transfer function is already set",
            );
        } else if !TRANSFERS.contains(&tf.0) {
            post_error(
                slf,
                WpImageDescriptionCreatorParamsV1Error::INVALID_TF.0,
                "unsupported named transfer function",
            );
        } else {
            self.description.tf_named = Some(tf.0);
        }
    }

    fn handle_set_primaries_named(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>,
        primaries: WpColorManagerV1Primaries,
    ) {
        if self.description.primaries_named.is_some() {
            post_error(
                slf,
                WpImageDescriptionCreatorParamsV1Error::ALREADY_SET.0,
                "primaries are already set",
            );
        } else if !PRIMARIES.contains(&primaries.0) {
            post_error(
                slf,
                WpImageDescriptionCreatorParamsV1Error::INVALID_PRIMARIES_NAMED.0,
                "unsupported named primaries",
            );
        } else {
            self.description.primaries_named = Some(primaries.0);
        }
    }

    fn handle_set_tf_power(&mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, _eexp: u32) {
        Self::unsupported(slf);
    }

    fn handle_set_primaries(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, _rx: i32, _ry: i32, _gx: i32,
        _gy: i32, _bx: i32, _by: i32, _wx: i32, _wy: i32,
    ) {
        Self::unsupported(slf);
    }

    fn handle_set_luminances(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, _min: u32, _max: u32,
        _reference: u32,
    ) {
        Self::unsupported(slf);
    }

    fn handle_set_mastering_display_primaries(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, rx: i32, ry: i32, gx: i32, gy: i32,
        bx: i32, by: i32, wx: i32, wy: i32,
    ) {
        if let Err(error) =
            set_mastering_primaries(&mut self.description, [rx, ry, gx, gy, bx, by, wx, wy])
        {
            post_error(slf, error.0, "mastering display primaries are already set");
        }
    }

    fn handle_set_mastering_luminance(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, min: u32, max: u32,
    ) {
        if let Err(error) = set_mastering_luminance(&mut self.description, min, max) {
            post_error(
                slf,
                error.0,
                "mastering luminance is already set or has an invalid range",
            );
        }
    }

    fn handle_set_max_cll(&mut self, _slf: &Rc<WpImageDescriptionCreatorParamsV1>, max_cll: u32) {
        self.description.max_cll = Some(max_cll);
    }

    fn handle_set_max_fall(&mut self, _slf: &Rc<WpImageDescriptionCreatorParamsV1>, max_fall: u32) {
        self.description.max_fall = Some(max_fall);
    }
}

fn post_error<T: Object>(object: &Rc<T>, code: u32, message: &str) {
    if let Some(client) = object.client() {
        let object: Rc<dyn Object> = object.clone();
        client.display().send_error(object, code, message);
    }
}

fn default_luminances(tf: Option<u32>) -> (u32, u32, u32) {
    match tf {
        Some(1) => (100, 100, 100),
        Some(11) => (50, 10000, 203),
        Some(13) => (50, 1000, 203),
        _ => (2000, 80, 80),
    }
}

fn valid_light_levels(description: &ColorDescription) -> bool {
    let (default_min, default_max, _) = default_luminances(description.tf_named);
    let (min, max) = description
        .target_luminance
        .unwrap_or((default_min, default_max));
    for value in [description.max_cll, description.max_fall]
        .into_iter()
        .flatten()
    {
        // CTA metadata uses zero for an unspecified content light level.
        if value != 0 && (u64::from(value) * 10000 <= u64::from(min) || value > max) {
            return false;
        }
    }
    match (description.max_cll, description.max_fall) {
        (Some(cll), Some(fall)) if cll != 0 && fall != 0 => fall <= cll,
        _ => true,
    }
}

fn set_mastering_primaries(
    description: &mut ColorDescription, primaries: [i32; 8],
) -> Result<(), WpImageDescriptionCreatorParamsV1Error> {
    if description.target_primaries.is_some() {
        return Err(WpImageDescriptionCreatorParamsV1Error::ALREADY_SET);
    }
    // Validation against the encoding gamut must wait until create: clients
    // may provide mastering metadata before the named encoding primaries.
    description.target_primaries = Some(primaries);
    Ok(())
}

fn set_mastering_luminance(
    description: &mut ColorDescription, min: u32, max: u32,
) -> Result<(), WpImageDescriptionCreatorParamsV1Error> {
    if description.target_luminance.is_some() {
        return Err(WpImageDescriptionCreatorParamsV1Error::ALREADY_SET);
    }
    if u64::from(max) * 10000 <= u64::from(min) {
        return Err(WpImageDescriptionCreatorParamsV1Error::INVALID_LUMINANCE);
    }
    description.target_luminance = Some((min, max));
    Ok(())
}

fn supported_mastering_primaries(description: &ColorDescription) -> bool {
    let Some(target) = description.target_primaries else {
        return true;
    };
    let Some(container) = description.primaries_named.and_then(named_primaries) else {
        return false;
    };
    let points = |values: [i32; 8]| {
        std::array::from_fn::<_, 4, _>(|i| (i64::from(values[i * 2]), i64::from(values[i * 2 + 1])))
    };
    let target = points(target);
    let container = points(container);
    if target
        .iter()
        .any(|&(x, y)| x < 0 || y <= 0 || x + y > 1_000_000)
    {
        return false;
    }
    let cross = |a: (i64, i64), b: (i64, i64), c: (i64, i64)| {
        (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
    };
    let inside = |triangle: &[(i64, i64); 4], point| {
        let signs = [
            cross(triangle[0], triangle[1], point),
            cross(triangle[1], triangle[2], point),
            cross(triangle[2], triangle[0], point),
        ];
        signs.iter().all(|&value| value >= 0) || signs.iter().all(|&value| value <= 0)
    };
    cross(target[0], target[1], target[2]) != 0
        && inside(&target, target[3])
        && target.into_iter().all(|point| inside(&container, point))
}

fn named_primaries(named: u32) -> Option<[i32; 8]> {
    Some(match named {
        1 => [
            640000, 330000, 300000, 600000, 150000, 60000, 312700, 329000,
        ],
        3 => [
            640000, 330000, 290000, 600000, 150000, 60000, 312700, 329000,
        ],
        4 => [
            630000, 340000, 310000, 595000, 155000, 70000, 312700, 329000,
        ],
        6 => [
            708000, 292000, 170000, 797000, 131000, 46000, 312700, 329000,
        ],
        9 => [
            680000, 320000, 265000, 690000, 150000, 60000, 312700, 329000,
        ],
        _ => return None,
    })
}

fn send_information(info: &Rc<WpImageDescriptionInfoV1>, description: &ColorDescription) {
    let primaries = description
        .primaries
        .or_else(|| description.primaries_named.and_then(named_primaries));
    if let Some([rx, ry, gx, gy, bx, by, wx, wy]) = primaries {
        info.send_primaries(rx, ry, gx, gy, bx, by, wx, wy);
    }
    if let Some(named) = description.primaries_named {
        info.send_primaries_named(WpColorManagerV1Primaries(named));
    }
    if let Some(named) = description.tf_named {
        info.send_tf_named(WpColorManagerV1TransferFunction(named));
    } else if let Some(power) = description.tf_power {
        info.send_tf_power(power);
    }
    let (min, max, reference) = description
        .luminances
        .unwrap_or_else(|| default_luminances(description.tf_named));
    info.send_luminances(min, max, reference);
    if let Some([rx, ry, gx, gy, bx, by, wx, wy]) = description.target_primaries.or(primaries) {
        info.send_target_primaries(rx, ry, gx, gy, bx, by, wx, wy);
    }
    let (min, max) = description.target_luminance.unwrap_or((min, max));
    info.send_target_luminance(min, max);
    if let Some(cll) = description.max_cll {
        info.send_target_max_cll(cll);
    }
    if let Some(fall) = description.max_fall {
        info.send_target_max_fall(fall);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            DisplayHandler,
            ProxyLifecycleEvent,
        },
        *,
    };
    use wl_proxy::{
        baseline::Baseline,
        object::ObjectRcUtils,
        protocols::{
            ObjectInterface,
            color_management_v1::wp_image_description_info_v1::WpImageDescriptionInfoV1Handler,
            wayland::{
                wl_display::{
                    WlDisplay,
                    WlDisplayHandler,
                },
                wl_registry::{
                    WlRegistry,
                    WlRegistryHandler,
                },
            },
        },
        state::State,
    };

    #[test]
    fn output_color_lifecycle_follows_global_across_bindings_and_registries() {
        struct ServerDisplay(Rc<RefCell<Vec<Rc<WlRegistry>>>>);
        impl WlDisplayHandler for ServerDisplay {
            fn handle_get_registry(&mut self, _: &Rc<WlDisplay>, registry: &Rc<WlRegistry>) {
                registry.set_handler(ServerRegistry);
                // Exercise different downstream names for the same global.
                if !self.0.borrow().is_empty() {
                    registry.send_global(200, ObjectInterface::WpColorManagerV1, 1);
                }
                registry.send_global(100, ObjectInterface::WlOutput, 4);
                registry.send_global(101, ObjectInterface::WlOutput, 4);
                if self.0.borrow().is_empty() {
                    registry.send_global(200, ObjectInterface::WpColorManagerV1, 1);
                }
                self.0.borrow_mut().push(Rc::clone(registry));
            }

            fn handle_sync(&mut self, _: &Rc<WlDisplay>, callback: &Rc<WlCallback>) {
                callback.send_done(0);
                callback.delete_id();
            }
        }
        struct ServerRegistry;
        impl WlRegistryHandler for ServerRegistry {
            fn handle_bind(&mut self, _: &Rc<WlRegistry>, name: u32, object: Rc<dyn Object>) {
                assert!(matches!(name, 100 | 101));
                object.downcast::<WlOutput>().set_handler(ServerOutput);
            }
        }
        struct ServerOutput;
        impl WlOutputHandler for ServerOutput {
            fn handle_release(&mut self, output: &Rc<WlOutput>) {
                output.delete_id();
            }
        }
        #[derive(Default)]
        struct RegistryEvents {
            globals: Vec<(u32, ObjectInterface)>,
            removed: Vec<u32>,
        }
        struct ClientRegistry(Rc<RefCell<RegistryEvents>>);
        impl WlRegistryHandler for ClientRegistry {
            fn handle_global(
                &mut self, _: &Rc<WlRegistry>, name: u32, interface: ObjectInterface, _: u32,
            ) {
                self.0.borrow_mut().globals.push((name, interface));
            }

            fn handle_global_remove(&mut self, _: &Rc<WlRegistry>, name: u32) {
                self.0.borrow_mut().removed.push(name);
            }
        }
        struct OutputEvents(Rc<Cell<u32>>);
        impl WpColorManagementOutputV1Handler for OutputEvents {
            fn handle_image_description_changed(&mut self, _: &Rc<WpColorManagementOutputV1>) {
                self.0.set(self.0.get() + 1);
            }
        }
        struct DescriptionEvents(Rc<RefCell<Vec<Result<u32, WpImageDescriptionV1Cause>>>>);
        impl WpImageDescriptionV1Handler for DescriptionEvents {
            fn handle_ready(&mut self, _: &Rc<WpImageDescriptionV1>, identity: u32) {
                self.0.borrow_mut().push(Ok(identity));
            }

            fn handle_failed(
                &mut self, _: &Rc<WpImageDescriptionV1>, cause: WpImageDescriptionV1Cause, _: &str,
            ) {
                self.0.borrow_mut().push(Err(cause));
            }
        }
        struct InformationEvents(Rc<Cell<bool>>);
        impl WpImageDescriptionInfoV1Handler for InformationEvents {
            fn handle_done(&mut self, _: &Rc<WpImageDescriptionInfoV1>) {
                self.0.set(true);
            }
        }
        let server = State::builder(Baseline::ALL_OF_THEM)
            .without_server()
            .build()
            .unwrap();
        let _server_guard = server.create_destructor();
        let (server_client, server_fd) = server.connect().unwrap();
        let server_registries = Rc::new(RefCell::new(Vec::new()));
        server_client
            .display()
            .set_handler(ServerDisplay(Rc::clone(&server_registries)));
        let proxy = State::builder(Baseline::ALL_OF_THEM)
            .with_server_fd(&Rc::new(server_fd))
            .build()
            .unwrap();
        let _proxy_guard = proxy.create_destructor();
        let (proxy_client, client_fd) = proxy.connect().unwrap();
        let shared = Rc::new(RefCell::new(SharedState {
            color_management: ColorManagementState::new(ColorDescription::srgb()),
            buffer_info: HashMap::new(),
            event_tx: flume::unbounded().0,
            frame_callbacks: HashMap::new(),
            next_callback_batch_id: 1,
            toplevels: Vec::new(),
            viewport: None,
            configure_serial: 1,
            fractional_scales: Vec::new(),
            surfaces: Vec::new(),
            shm_buffer_info: HashMap::new(),
        }));
        proxy_client.display().set_handler(DisplayHandler {
            state: Rc::clone(&shared),
            lifecycle_tx: flume::unbounded::<ProxyLifecycleEvent>().0,
        });
        let client = State::builder(Baseline::ALL_OF_THEM)
            .with_server_fd(&Rc::new(client_fd))
            .build()
            .unwrap();
        let _client_guard = client.create_destructor();
        client.set_default_forward_to_client(false);
        let dispatch = || {
            for _ in 0..32 {
                let mut did_work = false;
                for state in [&client, &proxy, &server] {
                    did_work |= state.dispatch_available().unwrap();
                }
                if !did_work {
                    return;
                }
            }
            panic!("protocol exchange did not settle");
        };
        let registries = std::array::from_fn::<_, 2, _>(|_| {
            let registry = client.display().new_send_get_registry();
            let events = Rc::new(RefCell::new(RegistryEvents::default()));
            registry.set_handler(ClientRegistry(Rc::clone(&events)));
            (registry, events)
        });
        dispatch();
        let output_names = registries.each_ref().map(|(_, events)| {
            events
                .borrow()
                .globals
                .iter()
                .filter_map(|&(name, interface)| {
                    (interface == ObjectInterface::WlOutput).then_some(name)
                })
                .collect::<Vec<_>>()
        });
        assert_ne!(output_names[0][0], output_names[1][0]);
        let manager_name = registries[0]
            .1
            .borrow()
            .globals
            .iter()
            .find(|(_, interface)| *interface == ObjectInterface::WpColorManagerV1)
            .unwrap()
            .0;
        let manager = client.create_object::<WpColorManagerV1>(1);
        registries[0].0.send_bind(manager_name, manager.clone());
        let bind_output = |registry: &Rc<WlRegistry>, name| {
            let output = client.create_object::<WlOutput>(4);
            registry.send_bind(name, output.clone());
            output
        };
        let output_a = bind_output(&registries[0].0, output_names[0][0]);
        let output_a_second = bind_output(&registries[1].0, output_names[1][0]);
        let output_b = bind_output(&registries[0].0, output_names[0][1]);
        let color_output = |output: &Rc<WlOutput>| {
            let color = manager.new_send_get_output(output);
            let changed = Rc::new(Cell::new(0));
            color.set_handler(OutputEvents(Rc::clone(&changed)));
            (color, changed)
        };
        let colors = [&output_a, &output_a_second, &output_b].map(color_output);
        let description = |output: &Rc<WpColorManagementOutputV1>| {
            let description = output.new_send_get_image_description();
            let events = Rc::new(RefCell::new(Vec::new()));
            description.set_handler(DescriptionEvents(Rc::clone(&events)));
            dispatch();
            let result = events.borrow_mut().pop().expect("ready or failed event");
            if result.is_ok() {
                let done = Rc::new(Cell::new(false));
                description
                    .new_send_get_information()
                    .set_handler(InformationEvents(Rc::clone(&done)));
                dispatch();
                assert!(done.get());
            }
            description.send_destroy();
            result
        };
        dispatch();
        assert!(description(&colors[0].0).is_ok());

        // Releasing a wl_output binding must not make its color output inert.
        output_a.send_release();
        dispatch();
        assert!(description(&colors[0].0).is_ok());
        shared.borrow_mut().color_management.set_preferred(pq());
        dispatch();
        assert_eq!(
            colors.each_ref().map(|(_, changed)| changed.get()),
            [1, 1, 1]
        );

        for registry in server_registries.borrow().iter() {
            registry.send_global_remove(100);
        }
        dispatch();
        for (i, (_, events)) in registries.iter().enumerate() {
            assert_eq!(events.borrow().removed, [output_names[i][0]]);
        }
        assert_eq!(
            description(&colors[0].0),
            Err(WpImageDescriptionV1Cause::NO_OUTPUT)
        );
        assert_eq!(
            description(&colors[1].0),
            Err(WpImageDescriptionV1Cause::NO_OUTPUT)
        );
        assert!(description(&colors[2].0).is_ok());
        // Existing bindings to a removed global may still create inert objects.
        let late = color_output(&output_a_second);
        assert_eq!(
            description(&late.0),
            Err(WpImageDescriptionV1Cause::NO_OUTPUT)
        );
        shared
            .borrow_mut()
            .color_management
            .set_preferred(ColorDescription::srgb());
        dispatch();
        assert_eq!(
            colors.each_ref().map(|(_, changed)| changed.get()),
            [1, 1, 2]
        );
        assert_eq!(late.1.get(), 0);

        // Reusing an upstream name creates a fresh lifetime, not a revival.
        for registry in server_registries.borrow().iter() {
            registry.send_global(100, ObjectInterface::WlOutput, 4);
        }
        dispatch();
        let replacement_name = registries[0].1.borrow().globals.last().unwrap().0;
        let replacement = color_output(&bind_output(&registries[0].0, replacement_name));
        assert!(description(&replacement.0).is_ok());
        assert_eq!(
            description(&colors[1].0),
            Err(WpImageDescriptionV1Cause::NO_OUTPUT)
        );
        for registry in server_registries.borrow().iter() {
            registry.send_global_remove(100);
        }
        dispatch();
        assert_eq!(
            description(&replacement.0),
            Err(WpImageDescriptionV1Cause::NO_OUTPUT)
        );
        assert_eq!(
            registries[0].1.borrow().removed,
            [output_names[0][0], replacement_name]
        );
    }

    fn pq() -> ColorDescription {
        ColorDescription {
            primaries_named: Some(6),
            tf_named: Some(11),
            ..Default::default()
        }
    }

    #[test]
    fn surface_color_changes_only_on_commit_and_survives_description_destruction() {
        let mut state = ColorManagementState::new(ColorDescription::srgb());
        state.descriptions.insert(10, pq());
        state.surfaces.insert(
            1,
            SurfaceColor {
                pending: state.descriptions.get(&10).cloned(),
                ..Default::default()
            },
        );
        state.descriptions.remove(&10);
        assert_eq!(state.current(1), ColorDescription::srgb());
        state.commit(1);
        assert_eq!(state.current(1), pq());
        state.commit(1);
        assert_eq!(state.current(1), pq());
        state.surfaces.get_mut(&1).unwrap().pending = Some(ColorDescription::srgb());
        assert_eq!(state.current(1), pq());
        state.commit(1);
        assert_eq!(state.current(1), ColorDescription::srgb());
    }

    #[test]
    fn destroyed_surfaces_do_not_keep_color_state() {
        let mut state = ColorManagementState::new(ColorDescription::srgb());
        state.surfaces.insert(
            1,
            SurfaceColor {
                current: pq(),
                ..Default::default()
            },
        );
        state.remove_surface(1);
        assert_eq!(state.current(1), ColorDescription::srgb());
    }

    #[test]
    fn named_primaries_and_default_luminances_use_protocol_units() {
        assert_eq!(named_primaries(6).unwrap()[0..2], [708000, 292000]);
        assert_eq!(named_primaries(9).unwrap()[2..4], [265000, 690000]);
        assert_eq!(default_luminances(Some(11)), (50, 10000, 203));
        assert_eq!(default_luminances(Some(13)), (50, 1000, 203));
        assert_eq!(default_luminances(Some(9)), (2000, 80, 80));
        assert!(named_primaries(0).is_none());
    }

    #[test]
    fn content_light_levels_reject_invalid_ranges_without_rejecting_unspecified_metadata() {
        let mut description = pq();
        description.max_cll = Some(0);
        description.max_fall = Some(0);
        assert!(valid_light_levels(&description));
        description.max_cll = Some(1000);
        description.max_fall = Some(400);
        assert!(valid_light_levels(&description));
        description.max_fall = Some(1001);
        assert!(!valid_light_levels(&description));
        description.max_fall = Some(0);
        description.max_cll = Some(10001);
        assert!(!valid_light_levels(&description));
    }

    #[test]
    fn advertised_encodings_have_matching_gdk_cicp_mappings() {
        for &primary in PRIMARIES {
            for &transfer in TRANSFERS {
                let description = ColorDescription {
                    primaries_named: Some(primary),
                    tf_named: Some(transfer),
                    ..Default::default()
                };
                assert!(description.cicp().is_ok());
            }
        }
    }

    #[test]
    fn mastering_metadata_preserves_encoding_and_checks_duplicates() {
        let mut description = pq();
        let encoding = description.cicp().unwrap();
        let primaries = named_primaries(6).unwrap();
        assert!(set_mastering_primaries(&mut description, primaries).is_ok());
        assert!(set_mastering_luminance(&mut description, 0, 505).is_ok());
        assert_eq!(description.target_primaries, Some(primaries));
        assert_eq!(description.target_luminance, Some((0, 505)));
        assert_eq!(description.cicp().unwrap(), encoding);
        assert!(supported_mastering_primaries(&description));
        assert_eq!(
            set_mastering_primaries(&mut description, primaries),
            Err(WpImageDescriptionCreatorParamsV1Error::ALREADY_SET)
        );
        assert_eq!(
            set_mastering_luminance(&mut description, 0, 1000),
            Err(WpImageDescriptionCreatorParamsV1Error::ALREADY_SET)
        );
        assert_eq!(description.target_luminance, Some((0, 505)));
    }

    #[test]
    fn mastering_luminance_checks_scaled_units_and_content_light_bounds() {
        let mut description = pq();
        assert_eq!(
            set_mastering_luminance(&mut description, 10000, 1),
            Err(WpImageDescriptionCreatorParamsV1Error::INVALID_LUMINANCE)
        );
        assert_eq!(
            set_mastering_luminance(&mut description, 0, 0),
            Err(WpImageDescriptionCreatorParamsV1Error::INVALID_LUMINANCE)
        );
        assert_eq!(description.target_luminance, None);
        assert!(set_mastering_luminance(&mut description, 1, 505).is_ok());
        description.max_cll = Some(505);
        assert!(valid_light_levels(&description));
        description.max_cll = Some(506);
        assert!(!valid_light_levels(&description));
    }

    #[test]
    fn mastering_primaries_reject_degenerate_and_unsupported_gamuts() {
        let mut description = pq();
        description.target_primaries = named_primaries(1);
        assert!(supported_mastering_primaries(&description));
        // Display P3's red primary extends slightly beyond BT.2020's red-green
        // edge; accepting it would require extended_target_volume support.
        description.target_primaries = named_primaries(9);
        assert!(!supported_mastering_primaries(&description));
        description.target_primaries = Some([0; 8]);
        assert!(!supported_mastering_primaries(&description));
        let mut invalid = named_primaries(6).unwrap();
        invalid[0] = -1;
        description.target_primaries = Some(invalid);
        assert!(!supported_mastering_primaries(&description));
        description.primaries_named = Some(1);
        description.target_primaries = named_primaries(6);
        assert!(!supported_mastering_primaries(&description));
    }
}
