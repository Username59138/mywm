//! The scene graph: a tree of nodes that the Lua config builds.
//!
//! The core never decides what goes on screen. A client window is just a node
//! of kind `Client`; it is drawn only while the config keeps it attached to
//! the tree under `root`.

use std::{collections::HashMap, time::Instant};

use slotmap::{SlotMap, new_key_type};
use smithay::backend::renderer::element::solid::SolidColorBuffer;

new_key_type! {
    pub struct NodeId;
}

pub type ClientId = u64;

/// Lua generation that owns core-created nodes (root and client nodes).
pub const CORE_OWNER: u32 = 0;

pub enum NodeKind {
    Group,
    Rect { color: [f32; 4], buffer: SolidColorBuffer },
    Client(ClientId),
}

impl NodeKind {
    pub fn name(&self) -> &'static str {
        match self {
            NodeKind::Group => "group",
            NodeKind::Rect { .. } => "rect",
            NodeKind::Client(_) => "client",
        }
    }
}

pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub opacity: f64,
    pub visible: bool,
    /// false = pointer input passes through this node and its subtree
    pub input: bool,
    /// Lua generation that created the node; a reload frees the old generation
    pub owner: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Prop {
    X,
    Y,
    W,
    H,
    Opacity,
}

impl Prop {
    pub fn parse(name: &str) -> Option<Prop> {
        Some(match name {
            "x" => Prop::X,
            "y" => Prop::Y,
            "w" => Prop::W,
            "h" => Prop::H,
            "opacity" => Prop::Opacity,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Easing {
    Linear,
    InQuad,
    OutQuad,
    InOutQuad,
    InCubic,
    OutCubic,
    InOutCubic,
    OutExpo,
}

impl Easing {
    pub const NAMES: &'static [&'static str] = &[
        "linear",
        "in_quad",
        "out_quad",
        "in_out_quad",
        "in_cubic",
        "out_cubic",
        "in_out_cubic",
        "out_expo",
    ];

    pub fn parse(name: &str) -> Option<Easing> {
        Some(match name {
            "linear" => Easing::Linear,
            "in_quad" => Easing::InQuad,
            "out_quad" => Easing::OutQuad,
            "in_out_quad" => Easing::InOutQuad,
            "in_cubic" => Easing::InCubic,
            "out_cubic" => Easing::OutCubic,
            "in_out_cubic" => Easing::InOutCubic,
            "out_expo" => Easing::OutExpo,
            _ => return None,
        })
    }

    fn apply(self, t: f64) -> f64 {
        match self {
            Easing::Linear => t,
            Easing::InQuad => t * t,
            Easing::OutQuad => 1.0 - (1.0 - t) * (1.0 - t),
            Easing::InOutQuad => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
                }
            }
            Easing::InCubic => t * t * t,
            Easing::OutCubic => 1.0 - (1.0 - t).powi(3),
            Easing::InOutCubic => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
            Easing::OutExpo => {
                if t >= 1.0 {
                    1.0
                } else {
                    1.0 - 2f64.powf(-10.0 * t)
                }
            }
        }
    }
}

struct Anim {
    node: NodeId,
    prop: Prop,
    from: f64,
    to: f64,
    start: Instant,
    duration: f64,
    easing: Easing,
    group: u64,
}

/// What the renderer draws, back to front, in absolute logical coordinates.
pub enum DrawItem {
    Rect { node: NodeId, x: f64, y: f64, alpha: f32 },
    Client { client: ClientId, x: f64, y: f64, alpha: f32 },
}

/// Animation groups that ended during a tick.
#[derive(Default)]
pub struct TickResult {
    pub finished: Vec<u64>,
    pub cancelled: Vec<u64>,
}

pub struct Scene {
    pub nodes: SlotMap<NodeId, Node>,
    pub root: NodeId,
    anims: Vec<Anim>,
    /// animation group -> number of running animations in it
    groups: HashMap<u64, usize>,
    next_group: u64,
    /// groups that ended without running (zero duration), reported on the next tick
    instant_groups: Vec<u64>,
    cancelled_groups: Vec<u64>,
}

impl Scene {
    pub fn new() -> Self {
        let mut nodes = SlotMap::with_key();
        let root = nodes.insert(Node::new(NodeKind::Group, CORE_OWNER));
        Scene {
            nodes,
            root,
            anims: Vec::new(),
            groups: HashMap::new(),
            next_group: 1,
            instant_groups: Vec::new(),
            cancelled_groups: Vec::new(),
        }
    }

    pub fn create(&mut self, kind: NodeKind, owner: u32) -> NodeId {
        self.nodes.insert(Node::new(kind, owner))
    }

    pub fn exists(&self, id: NodeId) -> bool {
        self.nodes.contains_key(id)
    }

    fn is_ancestor(&self, ancestor: NodeId, mut node: NodeId) -> bool {
        loop {
            if node == ancestor {
                return true;
            }
            match self.nodes.get(node).and_then(|n| n.parent) {
                Some(p) => node = p,
                None => return false,
            }
        }
    }

    /// Attach `child` as the topmost child of `parent`, detaching it from its
    /// old parent first.
    pub fn add_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), String> {
        if !self.exists(parent) || !self.exists(child) {
            return Err("node was destroyed".into());
        }
        if child == self.root {
            return Err("the root node cannot be added to another node".into());
        }
        if self.is_ancestor(child, parent) {
            return Err("cannot add a node to itself or to one of its descendants".into());
        }
        self.detach(child);
        self.nodes[parent].children.push(child);
        self.nodes[child].parent = Some(parent);
        Ok(())
    }

    pub fn detach(&mut self, id: NodeId) {
        let Some(parent) = self.nodes.get(id).and_then(|n| n.parent) else {
            return;
        };
        if let Some(p) = self.nodes.get_mut(parent) {
            p.children.retain(|&c| c != id);
        }
        self.nodes[id].parent = None;
    }

    /// Whether the node is part of the tree under root, i.e. can be drawn.
    pub fn is_attached(&self, id: NodeId) -> bool {
        self.exists(id) && self.is_ancestor(self.root, id)
    }

    /// Destroy a node and its subtree. Client nodes of live clients are only
    /// detached: they belong to the core. Returns the clients whose nodes were
    /// freed (only closed clients).
    pub fn destroy(&mut self, id: NodeId, is_live: &dyn Fn(ClientId) -> bool) -> Vec<ClientId> {
        let mut freed = Vec::new();
        if id == self.root || !self.exists(id) {
            return freed;
        }
        self.detach(id);
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            let node = &self.nodes[n];
            if let NodeKind::Client(c) = node.kind {
                if is_live(c) {
                    let children = std::mem::take(&mut self.nodes[n].children);
                    self.nodes[n].parent = None;
                    for ch in children {
                        self.nodes[ch].parent = None;
                        stack.push(ch);
                    }
                    continue;
                }
                freed.push(c);
            }
            stack.extend(node.children.iter().copied());
            self.remove_anims_of(n);
            self.nodes.remove(n);
        }
        freed
    }

    /// Destroy all children of a node.
    pub fn clear(&mut self, id: NodeId, is_live: &dyn Fn(ClientId) -> bool) -> Vec<ClientId> {
        let children = self.nodes.get(id).map(|n| n.children.clone()).unwrap_or_default();
        let mut freed = Vec::new();
        for c in children {
            freed.extend(self.destroy(c, is_live));
        }
        freed
    }

    /// Free every node created by one Lua generation (after a reload, or after
    /// a config that failed to load).
    pub fn destroy_owned_by(&mut self, owner: u32, is_live: &dyn Fn(ClientId) -> bool) -> Vec<ClientId> {
        let owned: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.owner == owner)
            .map(|(id, _)| id)
            .collect();
        let mut freed = Vec::new();
        for id in owned {
            freed.extend(self.destroy(id, is_live));
        }
        freed
    }

    /// Free a client node directly (a closed client nobody shows any more).
    pub fn remove_client_node(&mut self, id: NodeId) {
        if !self.exists(id) {
            return;
        }
        self.detach(id);
        let children = std::mem::take(&mut self.nodes[id].children);
        for ch in children {
            self.nodes[ch].parent = None;
        }
        self.remove_anims_of(id);
        self.nodes.remove(id);
    }

    pub fn get_prop(&self, id: NodeId, prop: Prop) -> f64 {
        let n = &self.nodes[id];
        match prop {
            Prop::X => n.x,
            Prop::Y => n.y,
            Prop::W => n.w,
            Prop::H => n.h,
            Prop::Opacity => n.opacity,
        }
    }

    pub fn set_prop(&mut self, id: NodeId, prop: Prop, value: f64) {
        let n = &mut self.nodes[id];
        match prop {
            Prop::X => n.x = value,
            Prop::Y => n.y = value,
            Prop::W => n.w = value.max(0.0),
            Prop::H => n.h = value.max(0.0),
            Prop::Opacity => n.opacity = value.clamp(0.0, 1.0),
        }
        if matches!(prop, Prop::W | Prop::H) {
            n.sync_rect();
        }
    }

    /// Setting a property directly stops its running animation.
    pub fn set_prop_now(&mut self, id: NodeId, prop: Prop, value: f64) {
        self.cancel_anim(id, prop);
        self.set_prop(id, prop, value);
    }

    pub fn set_color(&mut self, id: NodeId, rgba: [f32; 4]) -> bool {
        match &mut self.nodes[id].kind {
            NodeKind::Rect { color, .. } => {
                *color = rgba;
                self.nodes[id].sync_rect();
                true
            }
            _ => false,
        }
    }

    fn cancel_anim(&mut self, node: NodeId, prop: Prop) {
        if let Some(i) = self.anims.iter().position(|a| a.node == node && a.prop == prop) {
            let a = self.anims.swap_remove(i);
            self.leave_group(a.group);
        }
    }

    fn remove_anims_of(&mut self, node: NodeId) {
        let mut i = 0;
        while i < self.anims.len() {
            if self.anims[i].node == node {
                let a = self.anims.swap_remove(i);
                self.leave_group(a.group);
            } else {
                i += 1;
            }
        }
    }

    fn leave_group(&mut self, group: u64) {
        if let Some(count) = self.groups.get_mut(&group) {
            *count -= 1;
            if *count == 0 {
                self.groups.remove(&group);
                self.cancelled_groups.push(group);
            }
        }
    }

    /// Start animating properties of a node. A running animation of the same
    /// property is replaced and the new one starts from the current value.
    /// Returns the group id that `tick` reports when all of them finish.
    pub fn animate(&mut self, node: NodeId, targets: &[(Prop, f64)], duration: f64, easing: Easing) -> u64 {
        let group = self.next_group;
        self.next_group += 1;

        for &(prop, _) in targets {
            self.cancel_anim(node, prop);
        }
        if duration <= 0.0 || targets.is_empty() {
            for &(prop, to) in targets {
                self.set_prop(node, prop, to);
            }
            self.instant_groups.push(group);
            return group;
        }

        let start = Instant::now();
        for &(prop, to) in targets {
            self.anims.push(Anim {
                node,
                prop,
                from: self.get_prop(node, prop),
                to,
                start,
                duration,
                easing,
                group,
            });
        }
        self.groups.insert(group, targets.len());
        group
    }

    /// Advance all animations to `now`.
    pub fn tick(&mut self, now: Instant) -> TickResult {
        let mut result = TickResult {
            finished: std::mem::take(&mut self.instant_groups),
            cancelled: Vec::new(),
        };

        let mut i = 0;
        while i < self.anims.len() {
            let a = &self.anims[i];
            let t = (now.saturating_duration_since(a.start).as_secs_f64() / a.duration).min(1.0);
            let value = a.from + (a.to - a.from) * a.easing.apply(t);
            let (node, prop, group) = (a.node, a.prop, a.group);
            self.set_prop(node, prop, value);

            if t >= 1.0 {
                self.anims.swap_remove(i);
                if let Some(count) = self.groups.get_mut(&group) {
                    *count -= 1;
                    if *count == 0 {
                        self.groups.remove(&group);
                        result.finished.push(group);
                    }
                }
            } else {
                i += 1;
            }
        }

        result.cancelled = std::mem::take(&mut self.cancelled_groups);
        result
    }

    /// Absolute position of a node (sum of its own and its ancestors' offsets).
    pub fn abs_pos(&self, id: NodeId) -> (f64, f64) {
        let (mut x, mut y) = (0.0, 0.0);
        let mut cur = Some(id);
        while let Some(n) = cur.and_then(|c| self.nodes.get(c)) {
            x += n.x;
            y += n.y;
            cur = n.parent;
        }
        (x, y)
    }

    /// Visible drawables, back to front.
    pub fn draw_list(&self) -> Vec<DrawItem> {
        let mut out = Vec::new();
        self.collect(self.root, 0.0, 0.0, 1.0, &mut out);
        out
    }

    fn collect(&self, id: NodeId, ox: f64, oy: f64, alpha: f64, out: &mut Vec<DrawItem>) {
        let n = &self.nodes[id];
        if !n.visible {
            return;
        }
        let (x, y) = (ox + n.x, oy + n.y);
        let alpha = alpha * n.opacity;
        if alpha <= 0.0 {
            return;
        }
        match n.kind {
            NodeKind::Group => {}
            NodeKind::Rect { .. } => out.push(DrawItem::Rect { node: id, x, y, alpha: alpha as f32 }),
            NodeKind::Client(client) => out.push(DrawItem::Client { client, x, y, alpha: alpha as f32 }),
        }
        for &c in &n.children {
            self.collect(c, x, y, alpha, out);
        }
    }

    /// Client nodes that can receive pointer input, front to back, with
    /// their absolute positions.
    pub fn input_clients(&self) -> Vec<(ClientId, f64, f64)> {
        let mut out = Vec::new();
        self.collect_input(self.root, 0.0, 0.0, &mut out);
        out.reverse();
        out
    }

    fn collect_input(&self, id: NodeId, ox: f64, oy: f64, out: &mut Vec<(ClientId, f64, f64)>) {
        let n = &self.nodes[id];
        if !n.visible || !n.input || n.opacity <= 0.0 {
            return;
        }
        let (x, y) = (ox + n.x, oy + n.y);
        if let NodeKind::Client(c) = n.kind {
            out.push((c, x, y));
        }
        for &c in &n.children {
            self.collect_input(c, x, y, out);
        }
    }
}

impl Node {
    fn new(kind: NodeKind, owner: u32) -> Self {
        let mut node = Node {
            kind,
            parent: None,
            children: Vec::new(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            opacity: 1.0,
            visible: true,
            input: true,
            owner,
        };
        node.sync_rect();
        node
    }

    fn sync_rect(&mut self) {
        let (w, h) = (self.w.round() as i32, self.h.round() as i32);
        if let NodeKind::Rect { color, buffer } = &mut self.kind {
            buffer.update((w, h), *color);
        }
    }
}

pub fn parse_color(s: &str) -> Option<[f32; 4]> {
    let hex = s.strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    let (r, g, b, a) = match hex.len() {
        6 => (byte(0)?, byte(2)?, byte(4)?, 255),
        8 => (byte(0)?, byte(2)?, byte(4)?, byte(6)?),
        _ => return None,
    };
    let f = |v: u8| v as f32 / 255.0;
    // render elements expect premultiplied alpha
    let a = f(a);
    Some([f(r) * a, f(g) * a, f(b) * a, a])
}

pub fn format_color(c: [f32; 4]) -> String {
    let a = c[3];
    let un = |v: f32| if a > 0.0 { v / a } else { 0.0 };
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}{:02x}", b(un(c[0])), b(un(c[1])), b(un(c[2])), b(a))
}
