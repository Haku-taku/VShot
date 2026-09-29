// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#pragma once

#include "session_protocol.hpp"
#include "text_layer.hpp"

#include <QColor>
#include <QImage>
#include <QRect>
#include <QRectF>
#include <QString>
#include <QVector>

#include <functional>

class QPainter;

namespace vshot {

/// One recognized line and the translation drawn in its place.
///
/// The geometry is settled once, when the translation comes back, so the
/// preview the editor paints and the bitmap it writes are drawn from the very
/// same numbers and cannot drift apart.  A translation replaces the whole line,
/// so the text is the line's own `text` -- the units still hold the characters
/// the engine read.
struct TranslatedLine {
    /// The original line's box: the union of the unit boxes on it, in the
    /// overlay's global logical pixels.
    LogicalRect source;
    /// The box the background is filled and the text drawn in.  It is `source`
    /// grown when the translation is wider than the line it replaces.
    LogicalRect fill;
    /// The translated text, or the source when the engine failed that line.
    QString text;
    /// The family the text is drawn with, chosen for the script it holds.
    QString family;
    /// Glyph height in logical pixels: the line's height, shrunk to fit the
    /// width and never below half of it.
    int fontPixels = 0;
    /// The fill colour, read from the frame just outside the line.
    QColor background;
    /// A colour that reads on `background`.
    QColor textColor;
};

inline bool operator==(const TranslatedLine &first, const TranslatedLine &second)
{
    return first.source.x == second.source.x && first.source.y == second.source.y &&
        first.source.width == second.source.width && first.source.height == second.source.height &&
        first.fill.x == second.fill.x && first.fill.y == second.fill.y &&
        first.fill.width == second.fill.width && first.fill.height == second.fill.height &&
        first.text == second.text && first.family == second.family &&
        first.fontPixels == second.fontPixels && first.background == second.background &&
        first.textColor == second.textColor;
}

inline bool operator!=(const TranslatedLine &first, const TranslatedLine &second)
{
    return !(first == second);
}

/// The lines of a recognized layer: one per line, its box and its own text.
///
/// A line whose translation failed still comes back here -- with the source text
/// the engine left in place -- so it is drawn where the original was rather than
/// vanishing.
QVector<TranslatedLine> translatedLines(const TextLayer &layer);

/// The fill colour for one line: the most common colour of a thin band just
/// above and below the line's own box in `source`, which is the captured frame
/// in its device pixels.
///
/// The colours are quantised before they are counted, so a flat background's
/// own colour wins out over the handful of glyph pixels that bleed into the
/// band; a plain mean would be pulled toward the ink and the fill would not
/// match.
QColor sampleLineBackground(const QImage &source, const QRect &line);

/// How tall the original glyphs in `line` are, in the device pixels of
/// `source`: the rows of the box that hold pixels unlike `background`.
///
/// This is the line's own font size read off the frame rather than guessed from
/// the box.  The engine's box is its detection model's, padded well past the
/// glyphs -- measured on PaddleOCR's own models, a 14 px line came back in a
/// 19 px box and a 48 px one in a 64 px box -- so drawing the translation at the
/// box's height draws it about a third too large, which is what a replaced line
/// reads as sitting on the page instead of belonging to it.
///
/// Zero when nothing in the box stands out from the background: a flat frame, a
/// box the engine placed over its own padding, or a background the sampler
/// could not separate from the ink.  The caller falls back to the box for those.
int sampleLineInk(const QImage &source, const QRect &line, const QColor &background);

/// The family a string is drawn with, in order of preference: the caller's own
/// choice, the desktop's font (the family behind the application font, which is
/// what every other window on the desktop is drawn in), and the built-in lists
/// last -- those cover the script of the text when the desktop's font cannot,
/// which is what would otherwise box every CJK glyph it was handed.
QString translatedFamily(const QString &preferred, const QString &text);

/// The glyph size and the fill box for one line, never clipping the text.
struct LineFit {
    int fontPixels = 0;
    LogicalRect fill;
    QColor textColor;
};

/// Sizes `text` to `line` and widens the fill when the text does not fit at the
/// floor size, so the fill always covers everything drawn.  `limit` is the
/// image area the fill may grow within, in the same pixels as `line`.
///
/// `inkHeight` is how tall the original line's glyphs were, in logical pixels,
/// as `sampleLineInk` read them; the size the translation is drawn at comes from
/// it.  Zero -- the reading the caller could not make -- falls back to a
/// fraction of the line's own box, which is what the box measures on the models
/// the engine ships with.
LineFit fitTranslatedLine(const QString &text, const LogicalRect &line,
                          const QColor &background, const QString &family,
                          const LogicalRect &limit, int inkHeight = 0);

/// Places a whole recognized layer: the line boxes, a background read from the
/// frame under each, and a font that fits.  `geometry` and `scale` map the
/// layer's logical boxes onto `source`'s device pixels; `limit` is the image
/// area the fills may grow within, in global logical pixels.
QVector<TranslatedLine> placedTranslations(const TextLayer &layer, const QImage &source,
                                           const LogicalRect &geometry, double scale,
                                           const LogicalRect &limit, const QString &family);

/// Draws one placed line: fills `target` and draws the text inside it, clipped
/// to `target` so no ink ever leaves the box.  `fontScale` is how many device
/// pixels one logical pixel is under the painter, which already carries the
/// device transform in the editor but not on a plain bitmap.
void paintTranslatedLine(QPainter &painter, const QRectF &target, qreal fontScale,
                         const TranslatedLine &line);

/// Draws a whole placed translation: **every fill first, then every line's
/// text**.
///
/// The order is the point rather than an implementation detail.  A line's fill
/// is its font's metrics box, which stands taller than the ink it holds, so the
/// fills of closely spaced lines overlap -- and a line drawn before its
/// neighbour's fill has its descenders painted over by it.  Measured on a 26 px
/// line sitting 30 px above the next: the later fill took 104 of the upper
/// line's 1108 ink pixels.  Filling everything first makes that impossible at any
/// spacing.
///
/// `target` places one line in the painter's own coordinates.  Every caller
/// places them differently -- relative to an output, an annotation, or a plain
/// bitmap -- but they all owe the same two-pass order, so the order lives here
/// once instead of in each of them.
void paintTranslation(QPainter &painter, const QVector<TranslatedLine> &lines,
                      const std::function<QRectF(const TranslatedLine &)> &target,
                      qreal fontScale = 1.0);

} // namespace vshot
