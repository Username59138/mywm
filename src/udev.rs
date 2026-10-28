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
            control::{Device as _, ModeTypeFlags, connector, crtc},
        },
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Logical, Rectangle, Scale},
};
use tracing::{error, info, warn};

use crate::{
    render::{self, CLEAR_COLOR, Cursor, SceneElement},
    state::{Event, State},
};

/// 8-bit formats are supported everywhere, NVIDIA included.
const COLOR_FORMATS: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;

struct Surface {
    output: Output,
    global: GlobalId,
    connector: connector::Handle,
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
    gpu: Option<Gpu>,
    cursor: Cursor,
    /// false while another VT is shown
    active: bool,
}

impl UdevData {
    pub fn renderer(&mut self) -> Option<&mut GlesRenderer> {
        self.gpu.as_mut().map(|g| &mut g.renderer)
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
            state.process_input_event(event)
        })?;

    event_loop.handle().insert_source(notifier, |event, _, state| match event {
        SessionEvent::PauseSession => state.udev_pause(),
        SessionEvent::ActivateSession => state.udev_resume(),
    })?;

    state.udev = Some(UdevData {
        session,
        libinput,
        gpu: None,
        cursor: Cursor::load(),
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
        let Some(&mode) = info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| info.modes().first())
        else {
            warn!("{name}: the monitor reports no modes");
            return;
        };
        let wl_mode = WlMode::from(mode);
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
        output.set_preferred(wl_mode);
        output.change_current_state(Some(wl_mode), None, None, Some((x, 0).into()));
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
                return;
            }
        };

        info!(
            "output {name}: {}x{} @ {:.2} Hz at x = {x}",
            wl_mode.size.w,
            wl_mode.size.h,
            wl_mode.refresh as f64 / 1000.0
        );
        gpu.surfaces.insert(
            crtc,
            Surface {
                output: output.clone(),
                global,
                connector: info.handle(),
                drm_output,
                queued: false,
                timer: None,
            },
        );
        self.outputs.push(output);
        self.loop_handle.insert_idle(move |state| state.udev_render(crtc));
    }

    fn udev_remove_surface(&mut self, crtc: crtc::Handle) {
        let Some(gpu) = self.udev.as_mut().and_then(|u| u.gpu.as_mut()) else { return };
        let Some(surface) = gpu.surfaces.remove(&crtc) else { return };
        if let Some(t) = surface.timer {
            self.loop_handle.remove(t);
        }
        self.display_handle.remove_global::<State>(surface.global);
        info!("output {} disconnected", surface.output.name());
        self.outputs.retain(|o| o != &surface.output);
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
        self.pre_frame();

        let Some(udev) = self.udev.as_mut() else { return };
        let Some(gpu) = udev.gpu.as_mut() else { return };
        let Some(surface) = gpu.surfaces.get_mut(&crtc) else { return };
        if surface.queued || surface.timer.is_some() {
            return; // a vblank or the timer brings the next render
        }

        let output = surface.output.clone();
        let scale = Scale::from(output.current_scale().fractional_scale());
        let origin = output.current_location().to_f64();
        let size = output
            .current_mode()
            .map(|m| m.size.to_f64().to_logical(scale.x))
            .unwrap_or_default();
        let area: Rectangle<f64, Logical> = Rectangle::new(origin, size);

        let sh = self.shared.borrow();
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
