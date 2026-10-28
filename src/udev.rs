//! Running as the real compositor from a TTY: DRM/KMS for the monitors,
//! libinput for the input devices, libseat for access to both.
//!
//! One GPU (the primary one) drives every connected monitor; monitors are
//! placed left to right in their preferred modes.

use std::{collections::HashMap, path::Path, time::Duration};

use smithay::{
    backend::{
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType,
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDisplay, context::ContextPriority},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{ImportDma, ImportMemWl, gles::GlesRenderer},
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    output::{Mode as WlMode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            EventLoop, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::{
            Device as _,
            control::{self, Device as _, ModeTypeFlags, connector, crtc},
        },
        input::{
            AccelProfile, Device as InputDevice, DeviceCapability, Libinput,
        },
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Logical, Rectangle, Scale},
    wayland::drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd},
};
use tracing::{error, info, warn};

use crate::{
    render::{self, CLEAR_COLOR, Cursor, SceneElement},
    state::{Accel, Event, PointerSettings, State},
};

/// 8-bit formats are supported everywhere, NVIDIA included.
const COLOR_FORMATS: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;

struct Surface {
    output: Output,
    global: GlobalId,
    connector: connector::Handle,
    modes: Vec<control::Mode>,
    drm_output: DrmOutput<Allocator, Exporter, (), DrmDeviceFd>,
    /// a frame is queued and waits for its vblank
    queued: bool,
    /// a timer that renders again when nothing was queued
    timer: Option<RegistrationToken>,
}

struct Gpu {
    node: DrmNode,
    renderer: GlesRenderer,
    outputs: DrmOutputManager<Allocator, Exporter, (), DrmDeviceFd>,
    surfaces: HashMap<crtc::Handle, Surface>,
    _token: RegistrationToken,
}

pub struct UdevData {
    session: LibSeatSession,
    libinput: Libinput,
    /// input devices, for wm.input.mouse / touchpad
    devices: Vec<InputDevice>,
    gpu: Option<Gpu>,
    cursor: Cursor,
    /// false while another VT is shown
    active: bool,
}

impl UdevData {
    pub fn renderer(&mut self) -> Option<&mut GlesRenderer> {
        self.gpu.as_mut().map(|g| &mut g.renderer)
    }

    pub fn set_cursor(&mut self, cursor: Cursor) {
        self.cursor = cursor;
    }

    pub fn configure_devices(&mut self, mouse: &PointerSettings, touchpad: &PointerSettings) {
        for device in &mut self.devices {
            configure_device(device, mouse, touchpad);
        }
    }

    pub fn change_vt(&mut self, vt: i32) {
        if let Err(e) = self.session.change_vt(vt) {
            warn!("cannot switch to VT {vt}: {e}");
        }
    }
}

pub fn init(event_loop: &mut EventLoop<'static, State>, state: &mut State) -> Result<(), Box<dyn std::error::Error>> {
    let (session, notifier) = LibSeatSession::new()
        .map_err(|e| format!("cannot open a seat session (is elogind or seatd running?): {e}"))?;
    let seat = session.seat();

    let mut libinput = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput
        .udev_assign_seat(&seat)
        .map_err(|_| format!("libinput cannot use seat {seat}"))?;
    event_loop
        .handle()
        .insert_source(LibinputInputBackend::new(libinput.clone()), |event, _, state| {
            match &event {
                InputEvent::DeviceAdded { device } => {
                    let mut device = device.clone();
                    configure_device(&mut device, &state.mouse, &state.touchpad);
                    if let Some(udev) = state.udev.as_mut() {
                        udev.devices.push(device);
                    }
                }
                InputEvent::DeviceRemoved { device } => {
                    if let Some(udev) = state.udev.as_mut() {
                        udev.devices.retain(|d| d != device);
                    }
                }
                _ => {}
            }
            state.process_input_event(event)
        })?;

    event_loop.handle().insert_source(notifier, |event, _, state| match event {
        SessionEvent::PauseSession => state.udev_pause(),
        SessionEvent::ActivateSession => state.udev_resume(),
    })?;

    state.udev = Some(UdevData {
        session,
        libinput,
        devices: Vec::new(),
        gpu: None,
        cursor: Cursor::load(state.env_var("XCURSOR_THEME"), state.env_var("XCURSOR_SIZE")),
        active: true,
    });

    let udev = UdevBackend::new(&seat)?;
    let primary = primary_gpu(&seat)
        .ok()
        .flatten()
        .or_else(|| all_gpus(&seat).ok()?.into_iter().next())
        .ok_or("no GPU found")?;
    let primary = DrmNode::from_path(&primary).map_err(|e| format!("{}: {e}", primary.display()))?;
    let (dev_id, path) = udev
        .device_list()
        .find(|(id, _)| *id == primary.dev_id())
        .map(|(id, p)| (id, p.to_path_buf()))
        .ok_or("udev does not list the primary GPU")?;
    state.udev_add_gpu(DrmNode::from_dev_id(dev_id)?, &path)?;

    event_loop.handle().insert_source(udev, |event, _, state| match event {
        UdevEvent::Changed { device_id } => state.udev_device_changed(device_id),
        UdevEvent::Added { path, .. } => info!("ignoring {}: only one GPU is supported", path.display()),
        UdevEvent::Removed { .. } => {}
    })?;

    Ok(())
}

impl State {
    /// The config may have changed XCURSOR_THEME / XCURSOR_SIZE.
    pub fn reload_cursor(&mut self) {
        if self.udev.is_none() {
            return;
        }
        let cursor = Cursor::load(self.env_var("XCURSOR_THEME"), self.env_var("XCURSOR_SIZE"));
        if let Some(udev) = self.udev.as_mut() {
            udev.set_cursor(cursor);
        }
    }

    fn udev_add_gpu(&mut self, node: DrmNode, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let udev = self.udev.as_mut().ok_or("no session")?;
        let fd = udev
            .session
            .open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, notifier) = DrmDevice::new(fd.clone(), true)?;
        let gbm = GbmDevice::new(fd)?;
        let display = unsafe { EGLDisplay::new(gbm.clone())? };
        let context = EGLContext::new_with_priority(&display, ContextPriority::High)?;
        let renderer = unsafe { GlesRenderer::new(context)? };

        self.shm_state.update_formats(renderer.shm_formats());
        self.init_dmabuf(renderer.dmabuf_formats());

        let render_formats = renderer.egl_context().dmabuf_render_formats().clone();
        let render_node = node.node_with_type(NodeType::Render).and_then(Result::ok);
        let outputs = DrmOutputManager::new(
            drm,
            GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT),
            GbmFramebufferExporter::new(gbm.clone(), render_node),
            Some(gbm),
            COLOR_FORMATS.iter().copied(),
            render_formats,
        );

        // explicit sync: clients (NVIDIA ones above all) tell us when their
        // buffer is ready instead of relying on implicit fences
        let device_fd = outputs.device().device_fd().clone();
        if supports_syncobj_eventfd(&device_fd) {
            self.syncobj = Some(DrmSyncobjState::new::<State>(&self.display_handle, device_fd));
            info!("explicit sync enabled");
        } else {
            info!("the GPU driver does not support explicit sync");
        }

        let token = self
            .loop_handle
            .insert_source(notifier, |event, _, state| match event {
                DrmEvent::VBlank(crtc) => state.udev_vblank(crtc),
                DrmEvent::Error(e) => error!("DRM: {e:?}"),
            })
            .map_err(|e| e.to_string())?;

        info!("using GPU {node}");
        self.udev.as_mut().unwrap().gpu = Some(Gpu {
            node,
            renderer,
            outputs,
            surfaces: HashMap::new(),
            _token: token,
        });
        self.udev_scan_connectors();
        Ok(())
    }

    fn udev_device_changed(&mut self, device_id: u64) {
        let ours = self
            .udev
            .as_ref()
            .and_then(|u| u.gpu.as_ref())
            .is_some_and(|g| g.node.dev_id() == device_id);
        if ours {
            self.udev_scan_connectors();
        }
    }

    /// Match the outputs to the connected monitors (at start and on hotplug).
    fn udev_scan_connectors(&mut self) {
        let Some(gpu) = self.udev.as_mut().and_then(|u| u.gpu.as_mut()) else { return };
        let drm = gpu.outputs.device();
        let res = match drm.resource_handles() {
            Ok(r) => r,
            Err(e) => {
                warn!("cannot read DRM resources: {e}");
                return;
            }
        };
        let connected: Vec<connector::Info> = res
            .connectors()
            .iter()
            .filter_map(|&c| drm.get_connector(c, true).ok())
            .filter(|i| i.state() == connector::State::Connected)
            .collect();

        let gone: Vec<crtc::Handle> = gpu
            .surfaces
            .iter()
            .filter(|(_, s)| !connected.iter().any(|c| c.handle() == s.connector))
            .map(|(crtc, _)| *crtc)
            .collect();

        let mut used: Vec<crtc::Handle> = gpu.surfaces.keys().filter(|c| !gone.contains(c)).copied().collect();
        let mut new = Vec::new();
        for info in connected {
            if gpu.surfaces.values().any(|s| s.connector == info.handle()) {
                continue;
            }
            let crtc = info
                .encoders()
                .iter()
                .filter_map(|&e| drm.get_encoder(e).ok())
                .flat_map(|e| res.filter_crtcs(e.possible_crtcs()))
                .find(|c| !used.contains(c));
            match crtc {
                Some(crtc) => {
                    used.push(crtc);
                    new.push((info, crtc));
                }
                None => warn!("no free CRTC for connector {:?}", info.handle()),
            }
        }

        let changed = !gone.is_empty() || !new.is_empty();
        for crtc in gone {
            self.udev_remove_surface(crtc);
        }
        for (info, crtc) in new {
            self.udev_add_surface(info, crtc);
        }
        if changed {
            self.update_outputs();
            self.emit(Event::OutputChange);
        }
    }

    fn udev_add_surface(&mut self, info: connector::Info, crtc: crtc::Handle) {
        let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
        let modes = info.modes().to_vec();
        let Some(&preferred) = modes
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| modes.first())
        else {
            warn!("{name}: the monitor reports no modes");
            return;
        };
        let (w_mm, h_mm) = info.size().unwrap_or((0, 0));
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (w_mm as i32, h_mm as i32).into(),
                subpixel: Subpixel::Unknown,
                make: "Unknown".into(),
                model: "Unknown".into(),
            },
        );
        for m in &modes {
            output.add_mode(WlMode::from(*m));
        }

        // to the right of the outputs that are already there
        let x = self
            .outputs
            .iter()
            .filter_map(|o| {
                let w = o.current_mode()?.size.to_f64().to_logical(o.current_scale().fractional_scale()).w;
                Some(o.current_location().x + w.round() as i32)
            })
            .max()
            .unwrap_or(0);
        output.set_preferred(WlMode::from(preferred));
        output.change_current_state(Some(WlMode::from(preferred)), None, None, Some((x, 0).into()));

        // The config sees the monitor before it is lit and may choose its
        // mode, position and scale (applied to `output` right away).
        self.outputs.push(output.clone());
        self.update_outputs();
        self.emit(Event::OutputAdded(name.clone()));
        self.apply_output_config(&name);

        let wl_mode = output.current_mode().unwrap_or(WlMode::from(preferred));
        let mode = modes
            .iter()
            .copied()
            .find(|m| WlMode::from(*m) == wl_mode)
            .unwrap_or(preferred);
        let global = output.create_global::<State>(&self.display_handle);

        let gpu = self.udev.as_mut().and_then(|u| u.gpu.as_mut()).unwrap();
        let mut planes = gpu.outputs.device().planes(&crtc).ok();
        // overlay planes are known to break on NVIDIA
        let nvidia = gpu
            .outputs
            .device()
            .get_driver()
            .is_ok_and(|d| d.name().to_string_lossy().to_lowercase().contains("nvidia"));
        if nvidia && let Some(p) = planes.as_mut() {
            p.overlay.clear();
        }

        let drm_output = match gpu.outputs.initialize_output::<_, SceneElement>(
            crtc,
            mode,
            &[info.handle()],
            &output,
            planes,
            &mut gpu.renderer,
            &DrmOutputRenderElements::default(),
        ) {
            Ok(o) => o,
            Err(e) => {
                warn!("{name}: cannot set up the output: {e}");
                self.display_handle.remove_global::<State>(global);
                self.outputs.retain(|o| o != &output);
                self.update_outputs();
                self.emit(Event::OutputRemoved(name));
                return;
            }
        };

        let loc = output.current_location();
        info!(
            "output {name}: {} at {},{}",
            crate::state::format_mode(&WlMode::from(mode)),
            loc.x,
            loc.y
        );
        gpu.surfaces.insert(
            crtc,
            Surface {
                output,
                global,
                connector: info.handle(),
                modes,
                drm_output,
                queued: false,
                timer: None,
            },
        );
        self.loop_handle.insert_idle(move |state| state.udev_render(crtc));
    }

    /// Switch a lit output to another of its modes. An output that is not
    /// lit yet only needs its state changed, so that counts as success.
    pub fn udev_set_mode(&mut self, output: &Output, mode: WlMode) -> bool {
        let Some(gpu) = self.udev.as_mut().and_then(|u| u.gpu.as_mut()) else { return false };
        let Some((&crtc, surface)) = gpu.surfaces.iter().find(|(_, s)| &s.output == output) else {
            return true;
        };
        let Some(drm_mode) = surface.modes.iter().copied().find(|m| WlMode::from(*m) == mode) else {
            return false;
        };
        let elements: DrmOutputRenderElements<GlesRenderer, SceneElement> = DrmOutputRenderElements::default();
        match gpu.outputs.use_mode(&crtc, drm_mode, &mut gpu.renderer, &elements) {
            Ok(()) => {
                self.loop_handle.insert_idle(move |state| state.udev_render(crtc));
                true
            }
            Err(e) => {
                warn!("{}: cannot switch the mode: {e}", output.name());
                false
            }
        }
    }

    fn udev_remove_surface(&mut self, crtc: crtc::Handle) {
        let Some(gpu) = self.udev.as_mut().and_then(|u| u.gpu.as_mut()) else { return };
        let Some(surface) = gpu.surfaces.remove(&crtc) else { return };
        if let Some(t) = surface.timer {
            self.loop_handle.remove(t);
        }
        self.display_handle.remove_global::<State>(surface.global);
        info!("output {} disconnected", surface.output.name());
        self.close_layers_on(&surface.output.name());
        self.outputs.retain(|o| o != &surface.output);
        self.update_outputs();
        self.emit(Event::OutputRemoved(surface.output.name()));
    }

    fn udev_surface(&mut self, crtc: crtc::Handle) -> Option<&mut Surface> {
        self.udev.as_mut()?.gpu.as_mut()?.surfaces.get_mut(&crtc)
    }

    fn udev_vblank(&mut self, crtc: crtc::Handle) {
        let Some(surface) = self.udev_surface(crtc) else { return };
        surface.queued = false;
        if let Err(e) = surface.drm_output.frame_submitted() {
            warn!("frame submission failed: {e}");
        }
        self.udev_render(crtc);
    }

    /// Draw one output. If something changed, the frame is queued and the
    /// next render follows its vblank; otherwise a timer tries again after
    /// one refresh.
    fn udev_render(&mut self, crtc: crtc::Handle) {
        if !self.udev.as_ref().is_some_and(|u| u.active) {
            return;
        }
        let Some(output) = self.udev_surface(crtc).and_then(|s| {
            // a vblank or the timer brings the next render
            (!s.queued && s.timer.is_none()).then(|| s.output.clone())
        }) else {
            return;
        };
        self.pre_frame();
        self.emit_frame(&output);

        let Some(udev) = self.udev.as_mut() else { return };
        let Some(gpu) = udev.gpu.as_mut() else { return };
        let Some(surface) = gpu.surfaces.get_mut(&crtc) else { return };

        let scale = Scale::from(output.current_scale().fractional_scale());
        let origin = output.current_location().to_f64();
        let size = output
            .current_mode()
            .map(|m| m.size.to_f64().to_logical(scale.x))
            .unwrap_or_default();
        let area: Rectangle<f64, Logical> = Rectangle::new(origin, size);

        let mut sh = self.shared.borrow_mut();
        let pointer = self.seat.get_pointer().unwrap().current_location();
        let mut elements = Vec::new();
        if area.contains(pointer) {
            elements.extend(render::cursor_elements(
                &self.cursor_status,
                &udev.cursor,
                &mut gpu.renderer,
                pointer - origin,
                scale,
            ));
        }
        elements.extend(sh.notes.elements(&mut gpu.renderer, origin, size.w.round() as i32, scale));
        elements.extend(render::scene_elements(&sh, &mut gpu.renderer, origin, scale));

        let rendered = match surface
            .drm_output
            .render_frame(&mut gpu.renderer, &elements, CLEAR_COLOR, FrameFlags::DEFAULT)
        {
            Ok(result) => !result.is_empty,
            Err(e) => {
                warn!("{}: rendering failed: {e:?}", output.name());
                false
            }
        };
        if rendered {
            match surface.drm_output.queue_frame(()) {
                Ok(()) => surface.queued = true,
                Err(e) => warn!("{}: cannot queue the frame: {e}", output.name()),
            }
        }

        let time = self.start_time.elapsed();
        render::send_frames(&sh, &output, area, time);
        render::send_cursor_frame(&self.cursor_status, &output, time);
        drop(sh);

        if !surface.queued {
            let refresh = output.current_mode().map_or(60_000, |m| m.refresh.max(1)) as u64;
            let delay = Duration::from_micros(1_000_000_000 / refresh);
            surface.timer = self
                .loop_handle
                .insert_source(Timer::from_duration(delay), move |_, _, state| {
                    if let Some(s) = state.udev_surface(crtc) {
                        s.timer = None;
                    }
                    state.udev_render(crtc);
                    TimeoutAction::Drop
                })
                .ok();
        }

        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();
    }

    /// Another VT is shown: stop touching the devices.
    fn udev_pause(&mut self) {
        let Some(udev) = self.udev.as_mut() else { return };
        info!("session paused");
        udev.active = false;
        udev.libinput.suspend();
        if let Some(gpu) = udev.gpu.as_mut() {
            gpu.outputs.pause();
        }
    }

    /// Back on our VT: take the devices again and redraw everything.
    fn udev_resume(&mut self) {
        let Some(udev) = self.udev.as_mut() else { return };
        info!("session resumed");
        udev.active = true;
        if udev.libinput.resume().is_err() {
            error!("cannot resume libinput");
        }
        let Some(gpu) = udev.gpu.as_mut() else { return };
        if let Err(e) = gpu.outputs.activate(false) {
            error!("cannot reactivate the GPU: {e}");
        }
        let crtcs: Vec<crtc::Handle> = gpu.surfaces.keys().copied().collect();
        for crtc in crtcs {
            if let Some(s) = gpu.surfaces.get_mut(&crtc) {
                s.queued = false;
            }
            self.loop_handle.insert_idle(move |state| state.udev_render(crtc));
        }
    }
}

/// Apply the config's settings to a pointer device; what the config leaves
/// out gets the device default. Devices that can tap are touchpads.
fn configure_device(device: &mut InputDevice, mouse: &PointerSettings, touchpad: &PointerSettings) {
    if !device.has_capability(DeviceCapability::Pointer) {
        return;
    }
    let is_touchpad = device.config_tap_finger_count() > 0;
    let s = if is_touchpad { touchpad } else { mouse };

    if device.config_accel_is_available() {
        let speed = s.speed.unwrap_or_else(|| device.config_accel_default_speed());
        let _ = device.config_accel_set_speed(speed);
        let profile = match s.accel {
            Some(Accel::Flat) => Some(AccelProfile::Flat),
            Some(Accel::Adaptive) => Some(AccelProfile::Adaptive),
            None => device.config_accel_default_profile(),
        };
        if let Some(p) = profile
            && device.config_accel_profiles().contains(&p)
        {
            let _ = device.config_accel_set_profile(p);
        }
    }
    if device.config_scroll_has_natural_scroll() {
        let on = s.natural_scroll.unwrap_or_else(|| device.config_scroll_default_natural_scroll_enabled());
        let _ = device.config_scroll_set_natural_scroll_enabled(on);
    }
    if device.config_left_handed_is_available() {
        let on = s.left_handed.unwrap_or_else(|| device.config_left_handed_default());
        let _ = device.config_left_handed_set(on);
    }
    if is_touchpad {
        let on = s.tap.unwrap_or_else(|| device.config_tap_default_enabled());
        let _ = device.config_tap_set_enabled(on);
        if device.config_dwt_is_available() {
            let on = s.dwt.unwrap_or_else(|| device.config_dwt_default_enabled());
            let _ = device.config_dwt_set_enabled(on);
        }
    }
}
