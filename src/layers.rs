//! wlr-layer-shell: bars, wallpapers, launchers, notifications.
//!
//! A layer surface says where it wants to be (anchors, margins, how much of
//! the screen it keeps for itself). Like a window, it is not drawn until the
//! config puts `l.node` into its scene: the core works out where the surface
//! asked to be (`l.x`, `l.y`, ...) and what bars leave free of each output
//! (`o.usable`), and the config decides. The standard library does what the
//! surface asked for.

use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    delegate_layer_shell,
    desktop::{LayerSurface, WindowSurfaceType, layer_map_for_output},
    output::Output,
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    utils::{Logical, Rectangle},
    wayland::{
        compositor::with_states,
        shell::{
            wlr_layer::{Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState},
            xdg::PopupSurface,
        },
    },
};
use tracing::warn;

use crate::{
    scene::{CORE_OWNER, LayerId, NodeKind},
    state::{Event, LayerEntry, Stage, State},
};

impl State {
    fn output_by_name(&self, name: &str) -> Option<Output> {
        self.outputs.iter().find(|o| o.name() == name).cloned()
    }

    /// Lay out the layer surfaces of every output. Returns the shown layers
    /// that moved or changed size, and whether an output's free area changed.
    fn arrange_layers(&mut self) -> (Vec<LayerId>, bool) {
        let mut geos = Vec::new();
        for output in &self.outputs {
            let mut map = layer_map_for_output(output);
            map.arrange();
            let loc = output.current_location();
            for layer in map.layers() {
                if let Some(mut geo) = map.layer_geometry(layer) {
                    geo.loc += loc;
                    geos.push((layer.wl_surface().clone(), geo));
                }
            }
        }
        let mut moved = Vec::new();
        {
            let mut sh = self.shared.borrow_mut();
            for (surface, geo) in geos {
                let Some(id) = sh.layer_by_surface(&surface) else { continue };
                let l = sh.layers.get_mut(&id).unwrap();
                if l.geo != geo {
                    l.geo = geo;
                    if l.stage == Stage::Ready {
                        moved.push(id);
                    }
                }
            }
        }
        let before: Vec<Rectangle<i32, Logical>> = self.shared.borrow().outputs.iter().map(|o| o.usable).collect();
        self.update_outputs();
        let after: Vec<Rectangle<i32, Logical>> = self.shared.borrow().outputs.iter().map(|o| o.usable).collect();
        (moved, before != after)
    }

    /// Arrange, then tell the config what moved.
    fn relayout_layers(&mut self) {
        let (moved, usable_changed) = self.arrange_layers();
        for id in moved {
            self.emit(Event::LayerChange(id));
        }
        if usable_changed {
            self.emit(Event::OutputChange);
        }
    }

    /// A commit of a layer surface (called for every commit).
    pub fn layer_commit(&mut self, surface: &WlSurface) {
        let Some(id) = self.shared.borrow().layer_by_surface(surface) else { return };
        let initial_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .map(|d| d.lock().unwrap().initial_configure_sent)
                .unwrap_or(true)
        });
        self.relayout_layers();

        let (stage, node, wlr) = {
            let sh = self.shared.borrow();
            let Some(l) = sh.layers.get(&id) else { return };
            (l.stage, l.node, l.surface.layer_surface().clone())
        };
        if !initial_sent {
            // the first commit carries where it wants to be: the config hears
            // of it before the surface learns its size
            self.emit(Event::LayerNew(id, node));
            wlr.send_configure();
            return;
        }

        let mapped = with_renderer_surface_state(surface, |s| s.buffer().is_some()).unwrap_or(false);
        match (stage, mapped) {
            (Stage::New, true) => {
                self.shared.borrow_mut().layers.get_mut(&id).unwrap().stage = Stage::Ready;
                self.emit(Event::LayerReady(id));
            }
            (Stage::Ready, false) => {
                self.shared.borrow_mut().layers.get_mut(&id).unwrap().stage = Stage::New;
                self.layer_hidden(id, false);
            }
            _ => {}
        }
    }

    /// Hidden or gone: it loses the keyboard; where the keyboard goes next
    /// is the config's call (on layer.closed).
    fn layer_hidden(&mut self, id: LayerId, gone: bool) {
        if self.keyboard_focus == Some(id) {
            self.set_focus(None);
        }
        self.emit(Event::LayerClosed(id, gone));
    }

    /// An output went away: its layer surfaces are told to close.
    pub fn close_layers_on(&mut self, output: &str) {
        let sh = self.shared.borrow();
        for l in sh.layers.values().filter(|l| l.output == output) {
            l.surface.layer_surface().send_close();
        }
    }
}

impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(&mut self, surface: WlrLayerSurface, output: Option<WlOutput>, _layer: Layer, namespace: String) {
        // no output asked for: the one the pointer is on
        let output = output.as_ref().and_then(Output::from_resource).or_else(|| {
            let pos = self.seat.get_pointer().unwrap().current_location();
            let name = self
                .shared
                .borrow()
                .outputs
                .iter()
                .find(|o| {
                    pos.x >= o.x as f64 && pos.x < (o.x + o.width) as f64 && pos.y >= o.y as f64 && pos.y < (o.y + o.height) as f64
                })
                .map(|o| o.name.clone());
            name.and_then(|n| self.output_by_name(&n)).or_else(|| self.outputs.first().cloned())
        });
        let Some(output) = output else {
            warn!("a layer surface ({namespace}) but no output to put it on");
            surface.send_close();
            return;
        };
        let layer = LayerSurface::new(surface, namespace);
        if let Err(e) = layer_map_for_output(&output).map_layer(&layer) {
            warn!("cannot map a layer surface: {e}");
            return;
        }
        let mut sh = self.shared.borrow_mut();
        let id = sh.next_client;
        sh.next_client += 1;
        let node = sh.scene.create(NodeKind::Layer(id), CORE_OWNER);
        sh.layers.insert(
            id,
            LayerEntry { surface: layer, node, output: output.name(), stage: Stage::New, geo: Rectangle::default() },
        );
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, _popup: PopupSurface) {
        // tracked like any xdg popup (XdgShellHandler::new_popup)
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let Some(id) = self.shared.borrow().layer_by_surface(surface.wl_surface()) else { return };
        let (entry_output, was_ready) = {
            let sh = self.shared.borrow();
            let l = &sh.layers[&id];
            (l.output.clone(), l.stage == Stage::Ready)
        };
        if let Some(output) = self.output_by_name(&entry_output) {
            let mut map = layer_map_for_output(&output);
            let layer = map.layer_for_surface(surface.wl_surface(), WindowSurfaceType::TOPLEVEL).cloned();
            if let Some(layer) = layer {
                map.unmap_layer(&layer);
            }
        }
        if was_ready {
            self.layer_hidden(id, true);
        } else {
            self.emit(Event::LayerClosed(id, true));
        }
        {
            let mut sh = self.shared.borrow_mut();
            if let Some(l) = sh.layers.remove(&id) {
                sh.scene.remove_client_node(l.node);
            }
        }
        self.relayout_layers();
    }
}

delegate_layer_shell!(State);
