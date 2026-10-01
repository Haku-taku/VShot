// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! The tree of selectable regions, and the sources that build its levels.
//!
//! Interactive picking used to be one flat list of windows, each from a
//! compositor of its own.  A region is now a *tree*: a window is one level, the
//! UI elements inside it another, and a future source (a monitor, say) another
//! still.  Every level is one currency ([`RegionNode`]) so the picker walks the
//! whole tree the same way whatever produced it.
//!
//! Two properties matter to everything downstream:
//!
//! * A node's rect is always in **global logical pixels**, the space the
//!   captured scene and the pointer use.  A source that knows only relative
//!   geometry — an element inside its window — adds the origin itself, so no
//!   caller ever has to know which source a rect came from.
//! * A node's `children` are the level below it.  That is what the wheel steps
//!   through, and an empty `children` just means the node is a leaf.

use crate::geometry::{Point, Rect};

/// Which source produced a node.  A picker shows it because the step from a
/// window to its elements is a step between kinds, not only between levels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegionKind {
    /// A toplevel window, from the compositor's own list or the pixel fallback.
    Window,
    /// A widget inside a window, from the accessibility tree.
    Element,
    /// One output.  Not produced yet; named so the tree has the shape it will
    /// grow into.
    Monitor,
}

/// One selectable region, with the regions inside it.
///
/// The rect is global, the children are relative to nothing (they carry global
/// rects of their own), and `label` is what the picker shows the user — a
/// window's `class — title`, an element's role and name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegionNode {
    pub kind: RegionKind,
    pub rect: Rect,
    pub label: String,
    /// The regions one level down, in the source's own order.  Empty for a
    /// leaf.
    pub children: Vec<RegionNode>,
}

impl RegionNode {
    /// A leaf region of one kind.
    pub fn leaf(kind: RegionKind, rect: Rect, label: impl Into<String>) -> Self {
        Self {
            kind,
            rect,
            label: label.into(),
            children: Vec::new(),
        }
    }

    /// The same region carrying the regions inside it.
    pub fn with_children(mut self, children: Vec<RegionNode>) -> Self {
        self.children = children;
        self
    }

    /// The deepest node containing `point`, as a path of child indices from
    /// `self`.  The path is what the picker remembers so a wheel gesture can
    /// retrace it: element 0 is the child of `self`, and so on down.
    ///
    /// Ties are broken by taking the *last* matching child, so a later sibling
    /// drawn over an earlier one wins — the same rule the window picker uses
    /// for overlapping windows.
    pub fn deepest_path_containing(&self, point: Point) -> Vec<usize> {
        if !self.rect.contains(point) {
            return Vec::new();
        }
        let mut path = Vec::new();
        let mut node = self;
        loop {
            let mut next = None;
            for (index, child) in node.children.iter().enumerate() {
                if child.rect.contains(point) {
                    next = Some((index, child));
                }
            }
            match next {
                Some((index, child)) => {
                    path.push(index);
                    node = child;
                }
                None => return path,
            }
        }
    }

    /// The node at the end of a path of child indices, as
    /// [`RegionNode::deepest_path_containing`] produces them.  An empty path is
    /// `self`.  `None` when a step does not exist, which a stale path from a
    /// rebuilt tree can be.
    pub fn at_path(&self, path: &[usize]) -> Option<&RegionNode> {
        let mut node = self;
        for &index in path {
            node = node.children.get(index)?;
        }
        Some(node)
    }

    /// Drops the last step, which is the wheel-up gesture: sibling → parent.
    /// Empty at the root, where there is nothing above.
    pub fn without_last(path: &[usize]) -> Vec<usize> {
        path.iter().copied().take(path.len().saturating_sub(1)).collect()
    }
}

/// One level of the tree, asked for as the picker needs it.
///
/// Windows are known up front; a window's elements only matter once that window
/// is the one under the pointer.  So the sources are functions rather than one
/// trait: each is called at a different moment, and none is called for a level
/// the user never reaches.
pub struct RegionTree {
    /// The windows, as the compositor (or the pixel fallback) listed them.
    pub windows: Vec<RegionNode>,
}

impl RegionTree {
    /// A tree of windows with no elements filled in yet.
    pub fn new(windows: Vec<RegionNode>) -> Self {
        Self { windows }
    }

    /// The node a path names, where the path's first step is a window index.
    pub fn at_path(&self, path: &[usize]) -> Option<&RegionNode> {
        let (window, rest) = path.split_first()?;
        self.windows.get(*window)?.at_path(rest)
    }

    /// The deepest node containing `point`, as a path whose first step is a
    /// window index, plus the node itself.  `None` when the pointer is on no
    /// window.
    ///
    /// The last window containing the point wins, so an overlapping window on
    /// top of another is the one picked — the rule the existing picker has
    /// always used.
    pub fn deepest_containing(&self, point: Point) -> Option<(Vec<usize>, &RegionNode)> {
        let window = self
            .windows
            .iter()
            .enumerate()
            .rev()
            .find(|(_, window)| window.rect.contains(point))?
            .0;
        let mut path = self.windows[window].deepest_path_containing(point);
        path.insert(0, window);
        let node = self.at_path(&path)?;
        Some((path, node))
    }
}

/// Fills one window's level from a source: the elements of `window`, or `None`
/// when this source cannot answer.
///
/// `None` is not an error.  A window whose application exposes no accessibility
/// tree — a terminal, a browser without the bridge enabled — has no elements,
/// and picking has to fall back to the window itself rather than fail.
pub type ElementSource = dyn Fn(&RegionNode) -> Option<Vec<RegionNode>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn window(x: i32, y: i32, w: u32, h: u32) -> RegionNode {
        RegionNode::leaf(RegionKind::Window, Rect::new(x, y, w, h), "w")
    }

    #[test]
    fn a_leaf_has_no_path_below_it() {
        let node = window(0, 0, 100, 100);
        assert_eq!(node.deepest_path_containing(Point::new(50, 50)), Vec::<usize>::new());
    }

    #[test]
    fn a_point_outside_is_not_contained() {
        let node = window(0, 0, 100, 100);
        assert!(node.deepest_path_containing(Point::new(200, 50)).is_empty());
    }

    #[test]
    fn the_path_names_the_deepest_node_containing_the_point() {
        // window
        //   panel          0..100 x 0..100
        //     button        10..50 x 10..50
        let button = RegionNode::leaf(RegionKind::Element, Rect::new(10, 10, 40, 40), "button");
        let panel = RegionNode::leaf(RegionKind::Element, Rect::new(0, 0, 100, 100), "panel")
            .with_children(vec![button]);
        let root = window(0, 0, 200, 200).with_children(vec![panel]);

        assert_eq!(
            root.deepest_path_containing(Point::new(20, 20)),
            vec![0, 0],
            "inside the button names panel then button"
        );
        assert_eq!(
            root.deepest_path_containing(Point::new(80, 80)),
            vec![0],
            "in the panel but not the button stops at the panel"
        );
        assert_eq!(
            root.deepest_path_containing(Point::new(150, 150)),
            Vec::<usize>::new(),
            "in the window but no child is the window itself"
        );
    }

    #[test]
    fn a_later_sibling_covering_the_point_wins() {
        let first = RegionNode::leaf(RegionKind::Element, Rect::new(0, 0, 100, 100), "under");
        let second = RegionNode::leaf(RegionKind::Element, Rect::new(0, 0, 100, 100), "over");
        let root = window(0, 0, 100, 100).with_children(vec![first, second]);

        assert_eq!(
            root.deepest_path_containing(Point::new(50, 50)),
            vec![1],
            "the sibling later in the list is the one on top"
        );
    }

    #[test]
    fn a_path_can_be_retraced_after_climbing() {
        let button = RegionNode::leaf(RegionKind::Element, Rect::new(10, 10, 40, 40), "button");
        let panel = RegionNode::leaf(RegionKind::Element, Rect::new(0, 0, 100, 100), "panel")
            .with_children(vec![button]);
        let root = window(0, 0, 200, 200).with_children(vec![panel]);

        let path = vec![0, 0];
        // Wheel up: button -> panel.  Wheel down again: back to the button,
        // along the path already taken.
        let climbed = RegionNode::without_last(&path);
        assert_eq!(root.at_path(&climbed).unwrap().label, "panel");
        assert_eq!(root.at_path(&path).unwrap().label, "button");
    }

    #[test]
    fn climbing_off_the_top_reaches_the_window() {
        let button = RegionNode::leaf(RegionKind::Element, Rect::new(10, 10, 40, 40), "button");
        let root = window(0, 0, 200, 200).with_children(vec![button]);

        let mut path = vec![0];
        while !path.is_empty() {
            path = RegionNode::without_last(&path);
        }
        assert_eq!(
            root.at_path(&path).unwrap().kind,
            RegionKind::Window,
            "an empty path is the window itself"
        );
    }

    #[test]
    fn a_stale_path_answers_nothing_rather_than_panicking() {
        let root = window(0, 0, 100, 100);
        assert!(root.at_path(&[7]).is_none());
        assert!(root.at_path(&[0, 0]).is_none());
    }

    #[test]
    fn the_tree_picks_the_window_on_top_and_the_element_inside_it() {
        // The button lives inside `over`, which starts at 50,50.
        let button = RegionNode::leaf(RegionKind::Element, Rect::new(60, 60, 20, 20), "button");
        let under = window(0, 0, 300, 300);
        let over = window(50, 50, 100, 100).with_children(vec![button]);
        let tree = RegionTree::new(vec![under, over]);

        let (path, node) = tree.deepest_containing(Point::new(65, 65)).unwrap();
        assert_eq!(path, vec![1, 0], "the later window is on top");
        assert_eq!(node.label, "button");

        let (path, node) = tree.deepest_containing(Point::new(20, 20)).unwrap();
        assert_eq!(path, vec![0], "a point only the lower window covers");
        assert_eq!(node.kind, RegionKind::Window);

        assert!(tree.deepest_containing(Point::new(900, 900)).is_none());
    }
}
