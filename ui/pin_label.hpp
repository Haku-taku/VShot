// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#pragma once

#include <QColor>
#include <QFont>
#include <QFontMetrics>
#include <QPoint>
#include <QRect>
#include <QSize>
#include <QString>

#include <cstdint>

namespace vshot {

// The corner labels a pin carries: the `HDR` tag that says what a capture is,
// and the badge that reports a zoom step, a copy or a save.
//
// They live apart from the surface that draws them because two surfaces draw
// them now.  The pictures are painted by the daemon's own half-float surfaces,
// which have no text in them at all -- a glyph there would have to be laid out
// and rasterised by hand -- so the labels are drawn by a Qt surface above them,
// in the same place and the same font.  Two implementations of "where does the
// tag go" would drift, and the drift would show as a tag that no longer sits on
// the pin it belongs to.

// The translucent box a label is drawn on, large enough to read over any image
// and no larger.
inline constexpr int kLabelPixelSize = 16;
inline constexpr qreal kLabelRadius = 6.0;
inline const QColor kLabelBox(0, 0, 0, 160);

// The HDR tag's two inks.  A pinned HDR capture and the SDR half kept beside it
// are the same picture to the eye -- what differs is light no photograph of a
// screen reproduces -- so the tag is the only thing that says which of the two
// is on screen: white while the pixels are the HDR ones, muted grey while they
// are the capture mapped down for an SDR output.  Either way the pin *is* an
// HDR capture, which is why the tag is up at all.
inline const QColor kHdrTagShown(255, 255, 255);
inline const QColor kHdrTagFallback(192, 192, 192);

// The marker's text: the pin under the pointer is one whose light comes from a
// shape of its own.
inline const QString kHdrTag = QStringLiteral("HDR");

// The corner radius a pin can actually carry: never past half the shorter side
// of the painted image, where a corner would stop being a corner and start
// being a lozenge.  Asked at paint time rather than stored, because the size
// changes with every zoom step.
int paintRadius(std::uint32_t radius, const QSize &size);

// The font every corner label is drawn with.  Fixed rather than the pin's: a
// tag says what the pin *is*, and it has to stay legible on a pin zoomed down
// to a thumbnail.
QFont tagFont();

// The box a label needs: the text's own bounds grown by the same padding on
// every side, so every tag of the same font comes out the same height whatever
// it says.
QRect labelBox(const QFontMetrics &metrics, const QString &text);

// Where a label of `text` lands when it is anchored at `corner`: just inside
// the pin's top-left for the marker, just inside its bottom-right for the
// badge.  One definition, because the painter and the repaint region both ask
// it -- on a pin too small to hold the label, the label reaches past the pin,
// and a repaint region computed without it leaves the label's outer pixels at
// the old position every time the pin is zoomed or dragged.
QRect tagBox(const QString &text, const QPoint &corner, bool atBottomRight, const QRect &bounds);

} // namespace vshot
