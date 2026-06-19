//! Runtime UI — retained canvas + anchor layout (CHECKLIST T0 #13, the #1 gap).
//!
//! A retained widget tree (`UiNode`) with Unity/CSS-style **anchors**: each node's
//! rect is computed from its parent's rect via normalized anchor min/max (0..1) plus
//! pixel offsets, so UI scales/reflows with the screen and supports stretch, corner-
//! pinning, and centering. This module is the **layout solver** (fully testable);
//! the renderer draws the solved `Rect`s as panels/text/images, and the same tree
//! drives world-space VR UI. Layout being separate means it's verifiable headless.

/// A screen-space rectangle (pixels, origin top-left).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.right() && py >= self.y && py <= self.bottom()
    }
}

/// Anchors as normalized fractions of the parent rect (0 = left/top, 1 =
/// right/bottom). When min == max the edge is pinned (offset is an absolute pixel
/// position); when they differ the edge stretches with the parent.
#[derive(Clone, Copy, Debug)]
pub struct Anchors {
    pub min: [f32; 2],
    pub max: [f32; 2],
    /// Pixel offsets from the anchored edges: [left, top, right, bottom].
    pub offset: [f32; 4],
}

impl Anchors {
    /// Stretch to fill the parent with uniform pixel padding.
    pub fn stretch(pad: f32) -> Self {
        Self { min: [0.0, 0.0], max: [1.0, 1.0], offset: [pad, pad, -pad, -pad] }
    }
    /// Pin a fixed-size box to a corner/edge anchor (min==max) at pixel offset.
    pub fn pinned(anchor: [f32; 2], x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { min: anchor, max: anchor, offset: [x, y, x + w, y + h] }
    }
    /// Center a fixed-size box.
    pub fn centered(w: f32, h: f32) -> Self {
        Self {
            min: [0.5, 0.5],
            max: [0.5, 0.5],
            offset: [-w * 0.5, -h * 0.5, w * 0.5, h * 0.5],
        }
    }
}

/// What a node draws (the renderer's concern; layout ignores it).
#[derive(Clone, Debug, PartialEq)]
pub enum Widget {
    Panel { color: [f32; 4] },
    Label { text: String },
    Button { text: String },
    Image { texture: String },
}

/// A retained UI node.
#[derive(Clone, Debug)]
pub struct UiNode {
    pub name: String,
    pub anchors: Anchors,
    pub widget: Widget,
    pub visible: bool,
    pub children: Vec<UiNode>,
}

impl UiNode {
    pub fn new(name: &str, anchors: Anchors, widget: Widget) -> Self {
        Self { name: name.into(), anchors, widget, visible: true, children: Vec::new() }
    }
    pub fn child(mut self, c: UiNode) -> Self {
        self.children.push(c);
        self
    }

    /// Solve this node's rect inside `parent`.
    fn solve(&self, parent: Rect) -> Rect {
        let a = &self.anchors;
        let ax0 = parent.x + parent.w * a.min[0];
        let ay0 = parent.y + parent.h * a.min[1];
        let ax1 = parent.x + parent.w * a.max[0];
        let ay1 = parent.y + parent.h * a.max[1];
        let left = ax0 + a.offset[0];
        let top = ay0 + a.offset[1];
        let right = ax1 + a.offset[2];
        let bottom = ay1 + a.offset[3];
        Rect::new(left, top, (right - left).max(0.0), (bottom - top).max(0.0))
    }
}

/// A laid-out node: its solved rect + the widget, flattened in draw order
/// (parents before children).
#[derive(Clone, Debug)]
pub struct LaidOut<'a> {
    pub name: &'a str,
    pub rect: Rect,
    pub widget: &'a Widget,
}

/// The root canvas.
pub struct UiCanvas {
    pub root: Vec<UiNode>,
}

impl UiCanvas {
    pub fn new() -> Self {
        Self { root: Vec::new() }
    }
    pub fn add(&mut self, node: UiNode) {
        self.root.push(node);
    }

    /// Solve the whole tree for a `width`×`height` screen, returning every visible
    /// node's rect in draw order (depth-first, parents first).
    pub fn layout(&self, width: f32, height: f32) -> Vec<LaidOut<'_>> {
        let screen = Rect::new(0.0, 0.0, width, height);
        let mut out = Vec::new();
        for n in &self.root {
            solve_rec(n, screen, &mut out);
        }
        out
    }

    /// Topmost visible Button whose rect contains the point (for click routing).
    /// Searches in reverse draw order so the last-drawn (top) wins.
    pub fn button_at(&self, width: f32, height: f32, px: f32, py: f32) -> Option<String> {
        let laid = self.layout(width, height);
        laid.iter().rev().find_map(|l| match l.widget {
            Widget::Button { .. } if l.rect.contains(px, py) => Some(l.name.to_string()),
            _ => None,
        })
    }
}

impl Default for UiCanvas {
    fn default() -> Self {
        Self::new()
    }
}

fn solve_rec<'a>(node: &'a UiNode, parent: Rect, out: &mut Vec<LaidOut<'a>>) {
    if !node.visible {
        return;
    }
    let rect = node.solve(parent);
    out.push(LaidOut { name: &node.name, rect, widget: &node.widget });
    for c in &node.children {
        solve_rec(c, rect, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretch_fills_parent_with_padding() {
        let mut ui = UiCanvas::new();
        ui.add(UiNode::new(
            "bg",
            Anchors::stretch(10.0),
            Widget::Panel { color: [0.0, 0.0, 0.0, 1.0] },
        ));
        let laid = ui.layout(800.0, 600.0);
        assert_eq!(laid[0].rect, Rect::new(10.0, 10.0, 780.0, 580.0));
    }

    #[test]
    fn pinned_corner_is_resolution_independent() {
        let mut ui = UiCanvas::new();
        // A 100x40 button pinned to the bottom-right with 20px inset.
        ui.add(UiNode::new(
            "quit",
            Anchors::pinned([1.0, 1.0], -120.0, -60.0, 100.0, 40.0),
            Widget::Button { text: "Quit".into() },
        ));
        let small = ui.layout(800.0, 600.0)[0].rect;
        let big = ui.layout(1920.0, 1080.0)[0].rect;
        // Same size, and the same inset from the bottom-right corner at both res.
        assert_eq!(small.w, 100.0);
        assert!((small.right() - 800.0) == (big.right() - 1920.0));
        assert!((small.bottom() - 600.0) == (big.bottom() - 1080.0));
    }

    #[test]
    fn nested_anchors_are_relative_to_parent() {
        let panel = UiNode::new("panel", Anchors::centered(200.0, 100.0), Widget::Panel { color: [1.0; 4] })
            .child(UiNode::new("ok", Anchors::stretch(5.0), Widget::Button { text: "OK".into() }));
        let mut ui = UiCanvas::new();
        ui.add(panel);
        let laid = ui.layout(800.0, 600.0);
        // panel centered: (300,250,200,100); child stretched inside with 5px pad.
        assert_eq!(laid[0].rect, Rect::new(300.0, 250.0, 200.0, 100.0));
        assert_eq!(laid[1].rect, Rect::new(305.0, 255.0, 190.0, 90.0));
    }

    #[test]
    fn button_hit_test_picks_topmost() {
        let mut ui = UiCanvas::new();
        ui.add(UiNode::new(
            "play",
            Anchors::centered(100.0, 40.0),
            Widget::Button { text: "Play".into() },
        ));
        // Center of an 800x600 screen is inside the centered button.
        assert_eq!(ui.button_at(800.0, 600.0, 400.0, 300.0), Some("play".into()));
        assert_eq!(ui.button_at(800.0, 600.0, 10.0, 10.0), None);
    }
}
