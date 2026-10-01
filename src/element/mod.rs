// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! The sources that find the UI elements inside a window.
//!
//! Picking offers two levels: a window, and the elements within it.  The window
//! comes from the compositor (see [`crate::capture::window`]); the elements come
//! from here.  There is no one way to get them — an accessibility tree is exact
//! where an application exposes one, and useless where it does not, and a
//! program that draws its own UI (Zed's GPUI, a QML scene) exposes nothing at
//! all — so this is a *registry*: each source is tried in turn and the first
//! that answers wins.
//!
//! A source answers with a [`RegionNode`] tree, the same currency the rest of
//! picking uses, whose rects are global logical pixels.  Answering with `None`
//! means "not this window", not "failed": the next source is asked.  That is
//! also why the order matters — see [`sources`].
//!
//! Every source here is best-effort.  A window whose elements cannot be found
//! keeps the whole window as its only level, which is what picking did before
//! elements existed; nothing in this module can fail a session.

use crate::capture::window::WindowCandidate;
use crate::model::SceneSnapshot;
use crate::selection_region::RegionNode;

pub mod accessibility;
pub mod pixels;

/// What a source is given: the window to look inside, and the frozen frame it
/// was captured from.
///
/// Both are needed and neither is enough.  The window says *where* — its
/// geometry is the origin an accessibility tree cannot supply.  The frame says
/// *what is drawn there*, which is the only thing a source that reads pixels
/// has to go on.
pub struct ElementRequest<'a> {
    pub window: &'a WindowCandidate,
    /// The frame the picker froze, which the pixel source reads.  The
    /// accessibility source ignores it: its geometry comes from the toolkit,
    /// not from what is on the screen.
    pub scene: &'a SceneSnapshot,
}

/// One way of finding the elements inside a window.
pub trait ElementSource {
    /// The elements of `request`'s window, or `None` when this source cannot
    /// answer for it.
    ///
    /// `None` is the ordinary case, not an error: most sources decline most
    /// windows.  An `Err` is reserved for something the user should be told
    /// about, and even then the registry goes on to the next source.
    fn elements(&self, request: &ElementRequest<'_>) -> Option<Vec<RegionNode>>;
}

/// The sources, in the order they are asked.
///
/// The order is the whole design.  An accessibility tree is exact — real
/// widgets with real names and real bounds — so it is asked first wherever it
/// can answer.  The pixel source is the fallback: it reads what is drawn, which
/// works for any window at all but only ever guesses, and a guess is worth less
/// than an answer.
pub fn sources() -> Vec<&'static dyn ElementSource> {
    vec![&accessibility::AccessibilitySource, &pixels::Pixels]
}

/// The elements of one window, from the first source that can answer.
///
/// `None` when no source could: the caller then offers the window alone, which
/// is what picking did before there were elements.
pub fn elements_of(request: &ElementRequest<'_>) -> Option<Vec<RegionNode>> {
    for source in sources() {
        if let Some(elements) = source.elements(request) {
            if !elements.is_empty() {
                return Some(elements);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order is the design: the accessibility tree knows the widgets, the
    /// pixel source only guesses at them, and a guess must never pre-empt an
    /// answer.  The pixel source answers for almost any window, so anything
    /// asked after it would never be reached — which is why it has to be last.
    #[test]
    fn there_are_two_sources() {
        assert_eq!(sources().len(), 2, "the tree, then the pixels");
    }
}

