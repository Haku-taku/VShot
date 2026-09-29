// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#pragma once

#include "session_protocol.hpp"

#include <QByteArray>
#include <QChar>
#include <QString>
#include <QStringList>
#include <QVector>

#include <optional>

namespace vshot {

/// One unit of recognized text and the box the engine put it in.
///
/// A unit is one character in every ordinary run.  It carries a string rather
/// than a `QChar` because the engine can answer with a line and no character
/// boxes at all -- it reports the count of boxes it found and the count of
/// characters it read, and when those disagree every box after the mismatch
/// would point at the wrong glyph, so the whole line comes back as one unit
/// over the line's own rect.  The selection rules are the same either way.
struct TextUnit {
    QString text;
    /// Where the unit sat, in the overlay's own global logical pixels.
    LogicalRect rect;
    /// The line this unit belongs to.  Units of one line are adjacent and share
    /// the value, so a line break is a change in it.
    int line = 0;
};

/// How the engine's pixels map onto the overlay's.
///
/// The engine ran on a crop of one output's captured frame, so its coordinates
/// are device pixels counted from that crop.  `origin` is where that crop's
/// top-left sits in global logical pixels and `scale` is that output's device
/// pixels per logical pixel; the overlay draws in global logical pixels, and
/// this is the only place the two spaces meet.
struct TextLayerPlacement {
    double originX = 0.0;
    double originY = 0.0;
    double scale = 1.0;
};

/// The recognized text of one capture, with its positions when there are any.
///
/// This is the whole of the text-selection model: a flat list of units in
/// reading order, so selecting a range is an index pair and nothing else --
/// which is what makes "drag from here to there" mean the same thing on one
/// line and across five.  It holds no widget and no session, so the rules can
/// be checked without a compositor, an overlay, or a recognition run.
class TextLayer {
public:
    /// Parses what `vshot ocr --json` prints and places it with `placement`.
    ///
    /// `std::nullopt` means the document is not that JSON, and `error` says
    /// what was wrong with it.  A document that parses but reports no geometry
    /// -- an external engine, which hands back text and no positions -- is a
    /// layer all the same, one whose `hasGeometry` is false and whose only use
    /// is `plainText`.
    static std::optional<TextLayer> fromJson(const QByteArray &document,
                                             const TextLayerPlacement &placement, QString *error);

    /// Whether the engine said where the text was.  False leaves nothing to
    /// select: the caller falls back to the whole text.
    bool hasGeometry() const { return !units_.isEmpty(); }

    /// The whole recognized text, one line per line, with no trailing newline.
    /// This is what the layer has to offer when there is no geometry to select
    /// with, and it is the same text `vshot ocr` prints without `--json`.
    QString plainText() const { return lineTexts_.join(QLatin1Char('\n')); }

    int count() const { return units_.size(); }
    int lineCount() const { return lineTexts_.size(); }
    const TextUnit &unit(int index) const { return units_.at(index); }

    /// The text of one line, in reading order, or an empty string out of range.
    ///
    /// A translation replaces `lines[].text` and leaves every character box
    /// alone, so the units still hold the characters the engine read while the
    /// line holds what was put in their place: whoever draws the translation
    /// reads the line's text here rather than rebuilding it from the units.
    QString lineText(int index) const { return lineTexts_.value(index); }

    /// The unit whose box contains `(x, y)`, or -1 when none does.
    int indexAt(int x, int y) const;

    /// The unit on the line `y` falls in that is nearest to `x`.
    ///
    /// A drag leaves the text at the edges -- past the end of a line, above the
    /// first one -- and has to keep meaning something, so a selection dragged
    /// off the right of a line stops at its last character instead of jumping
    /// to the line below.  Returns -1 only for an empty layer.
    int nearestIndex(int x, int y) const;

    /// The text of `anchor`..`focus` inclusive, whichever way round they are.
    ///
    /// A line break in the middle of a range becomes a newline, and the spaces
    /// that padded the line it ends are dropped: they are not part of the text
    /// anyone means to copy.
    QString rangeText(int anchor, int focus) const;

    /// Widens `index` to the word around it and writes its ends to `first` and
    /// `last` (inclusive, and both -1 for an empty layer).
    ///
    /// A word is a run of the same kind of character: text that has spaces is
    /// split on them, so a double click takes `word` out of `a word here`.
    /// Chinese and Japanese are written without spaces, so a run of that script
    /// would be a whole sentence rather than a word, and finding the real word
    /// in it needs a dictionary this does not have: a double click there takes
    /// the one character under the pointer, which is short but never wrong.
    void wordRange(int index, int *first, int *last) const;

    /// Widens `index` to every unit on its line.
    void lineRange(int index, int *first, int *last) const;

private:
    QVector<TextUnit> units_;
    QStringList lineTexts_;
};

} // namespace vshot
