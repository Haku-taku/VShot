// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#include "text_layer.hpp"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonParseError>
#include <QJsonValue>

#include <algorithm>
#include <cmath>
#include <cstdint>

namespace vshot {
namespace {

/// A rectangle as the engine reports it: `x`/`y`/`width`/`height` in the device
/// pixels of the image it was handed.  Deliberately not `LogicalRect`, which is
/// what the caller gets back: this is the far side of the placement.
bool parseEngineRect(const QJsonValue &value, LogicalRect *rect)
{
    if (!value.isObject()) {
        return false;
    }
    const QJsonObject object = value.toObject();
    for (const char *key : {"x", "y", "width", "height"}) {
        if (!object.value(QLatin1String(key)).isDouble()) {
            return false;
        }
    }
    const double x = object.value(QLatin1String("x")).toDouble();
    const double y = object.value(QLatin1String("y")).toDouble();
    const double width = object.value(QLatin1String("width")).toDouble();
    const double height = object.value(QLatin1String("height")).toDouble();
    if (width < 0.0 || height < 0.0) {
        return false;
    }
    rect->x = static_cast<std::int32_t>(std::lround(x));
    rect->y = static_cast<std::int32_t>(std::lround(y));
    rect->width = static_cast<std::uint32_t>(std::lround(width));
    rect->height = static_cast<std::uint32_t>(std::lround(height));
    return true;
}

/// The engine's rectangle in the overlay's own global logical pixels.
///
/// Each edge is mapped and only then rounded, rather than rounding the width on
/// its own: at a fractional scale the two differ by a pixel, and an edge that
/// disagrees with its neighbour's by one pixel shows up as a seam between two
/// adjacent characters.
LogicalRect placed(const LogicalRect &engine, const TextLayerPlacement &placement)
{
    const double scale = placement.scale > 0.0 ? placement.scale : 1.0;
    const double left = placement.originX + engine.x / scale;
    const double top = placement.originY + engine.y / scale;
    const double right = placement.originX + (engine.x + static_cast<double>(engine.width)) / scale;
    const double bottom =
        placement.originY + (engine.y + static_cast<double>(engine.height)) / scale;
    LogicalRect result;
    result.x = static_cast<std::int32_t>(std::lround(left));
    result.y = static_cast<std::int32_t>(std::lround(top));
    result.width = static_cast<std::uint32_t>(std::max(0L, std::lround(right - left)));
    result.height = static_cast<std::uint32_t>(std::max(0L, std::lround(bottom - top)));
    return result;
}

/// What kind of text a unit holds, for the double-click rule.
enum class UnitKind {
    Space,
    /// Written without spaces between words, which is the whole difficulty.
    Ideograph,
    Word,
};

UnitKind kindOf(const QString &text)
{
    if (text.isEmpty()) {
        return UnitKind::Space;
    }
    const QChar first = text.at(0);
    if (first.isSpace()) {
        return UnitKind::Space;
    }
    // From the CJK radicals upward: Han, kana, Hangul and the punctuation that
    // travels with them.  Everything below is text that separates its words
    // with spaces, so the spaces are enough to find them.
    if (first.unicode() >= 0x2E80) {
        return UnitKind::Ideograph;
    }
    return UnitKind::Word;
}

/// The half-open horizontal span of a unit, in logical pixels.  A box the
/// engine could not separate still has to be reachable, so an empty one is
/// treated as one pixel wide.
void horizontalSpan(const LogicalRect &rect, std::int64_t *left, std::int64_t *right)
{
    *left = rect.x;
    *right = *left + std::max<std::uint32_t>(rect.width, 1);
}

/// How far `value` sits outside `[low, high)`, and zero inside it.
std::int64_t distanceOutside(std::int64_t value, std::int64_t low, std::int64_t high)
{
    if (value < low) {
        return low - value;
    }
    if (value >= high) {
        return value - high + 1;
    }
    return 0;
}

} // namespace

std::optional<TextLayer> TextLayer::fromJson(const QByteArray &document,
                                             const TextLayerPlacement &placement, QString *error)
{
    // The messages here are diagnostics for whoever is looking at stderr, not
    // text for the user: the caller turns a rejection into its own message and
    // this one says what was actually wrong with the document.
    const auto reject = [error](const QString &message) -> std::optional<TextLayer> {
        if (error != nullptr) {
            *error = message;
        }
        return std::nullopt;
    };

    QJsonParseError parseError{};
    const QJsonDocument parsed = QJsonDocument::fromJson(document, &parseError);
    if (parseError.error != QJsonParseError::NoError) {
        return reject(QStringLiteral("the text layer is not JSON: %1").arg(parseError.errorString()));
    }
    if (!parsed.isObject()) {
        return reject(QStringLiteral("the text layer is not a JSON object"));
    }
    const QJsonObject root = parsed.object();
    if (root.value(QStringLiteral("version")).toInt() != 1) {
        return reject(QStringLiteral("the text layer is not version 1"));
    }
    const QJsonValue geometryValue = root.value(QStringLiteral("geometry"));
    if (!geometryValue.isBool()) {
        return reject(QStringLiteral("the text layer does not say whether it has geometry"));
    }
    const bool geometry = geometryValue.toBool();
    const QJsonValue lines = root.value(QStringLiteral("lines"));
    if (!lines.isArray()) {
        return reject(QStringLiteral("the text layer has no lines"));
    }

    TextLayer layer;
    for (const QJsonValue &entry : lines.toArray()) {
        if (!entry.isObject()) {
            return reject(QStringLiteral("a text layer line is not an object"));
        }
        const QJsonObject line = entry.toObject();
        const QJsonValue text = line.value(QStringLiteral("text"));
        if (!text.isString()) {
            return reject(QStringLiteral("a text layer line has no text"));
        }
        layer.lineTexts_.append(text.toString());
        if (!geometry) {
            // An external engine: the text is all there is, and the caller
            // falls back to handing it over whole.
            continue;
        }
        const int lineIndex = layer.lineTexts_.size() - 1;
        LogicalRect lineRect;
        if (!parseEngineRect(line.value(QStringLiteral("rect")), &lineRect)) {
            return reject(QStringLiteral("a text layer line has no rectangle"));
        }
        const QJsonValue characters = line.value(QStringLiteral("chars"));
        // A missing list means the same thing an empty one does: no
        // per-character boxes.  The recognition engine sends `[]` when its
        // boxes disagreed with its text; the translation step drops the list
        // altogether, because the boxes exist to select the source and a
        // translation is not selected.  Both leave the line placeable as one
        // unit over its own rect, which is exactly what a translated line
        // needs.  A list that is there but is not a list is still malformed.
        if (!characters.isUndefined() && !characters.isNull() && !characters.isArray()) {
            return reject(QStringLiteral("a text layer line's characters are not a list"));
        }
        const QJsonArray charactersArray = characters.toArray();
        if (charactersArray.isEmpty()) {
            // The engine read the line but not the position of each character
            // in it, so the line stays selectable as one piece rather than
            // becoming unselectable.
            TextUnit unit;
            unit.text = text.toString();
            unit.rect = placed(lineRect, placement);
            unit.line = lineIndex;
            layer.units_.append(unit);
            continue;
        }
        for (const QJsonValue &character : charactersArray) {
            if (!character.isObject()) {
                return reject(QStringLiteral("a text layer character is not an object"));
            }
            const QJsonObject object = character.toObject();
            const QJsonValue glyph = object.value(QStringLiteral("ch"));
            LogicalRect rect;
            if (!glyph.isString() || !parseEngineRect(object.value(QStringLiteral("rect")), &rect)) {
                return reject(QStringLiteral("a text layer character has no glyph or rectangle"));
            }
            TextUnit unit;
            unit.text = glyph.toString();
            unit.rect = placed(rect, placement);
            unit.line = lineIndex;
            layer.units_.append(unit);
        }
    }
    return layer;
}

int TextLayer::indexAt(int x, int y) const
{
    for (int index = 0; index < units_.size(); ++index) {
        const LogicalRect &rect = units_.at(index).rect;
        std::int64_t left = 0;
        std::int64_t right = 0;
        horizontalSpan(rect, &left, &right);
        const std::int64_t top = rect.y;
        const std::int64_t bottom = top + std::max<std::uint32_t>(rect.height, 1);
        if (distanceOutside(x, left, right) == 0 && distanceOutside(y, top, bottom) == 0) {
            return index;
        }
    }
    return -1;
}

int TextLayer::nearestIndex(int x, int y) const
{
    if (units_.isEmpty()) {
        return -1;
    }
    // The lines are contiguous runs of units, so each is a scan of the list
    // rather than a second index that could fall out of step with it.  Pick the
    // line the pointer is closest to vertically, then the unit on that line it
    // is closest to horizontally: a drag that has left the text still has an
    // obvious meaning, and it does not leak onto the line below.
    int bestLineFirst = 0;
    int bestLineLast = 0;
    std::int64_t bestVertical = -1;
    int index = 0;
    while (index < units_.size()) {
        const int first = index;
        const int line = units_.at(first).line;
        int last = first;
        while (last + 1 < units_.size() && units_.at(last + 1).line == line) {
            ++last;
        }
        // Every box on a line spans the line's full height, so either end of it
        // gives the same band.
        const std::int64_t top = units_.at(first).rect.y;
        const std::int64_t bottom = top + std::max<std::uint32_t>(units_.at(first).rect.height, 1);
        const std::int64_t distance = distanceOutside(y, top, bottom);
        if (bestVertical < 0 || distance < bestVertical) {
            bestVertical = distance;
            bestLineFirst = first;
            bestLineLast = last;
        }
        index = last + 1;
    }

    int best = bestLineFirst;
    std::int64_t bestHorizontal = -1;
    for (int candidate = bestLineFirst; candidate <= bestLineLast; ++candidate) {
        std::int64_t left = 0;
        std::int64_t right = 0;
        horizontalSpan(units_.at(candidate).rect, &left, &right);
        const std::int64_t distance = distanceOutside(x, left, right);
        if (bestHorizontal < 0 || distance < bestHorizontal) {
            bestHorizontal = distance;
            best = candidate;
        }
    }
    return best;
}

QString TextLayer::rangeText(int anchor, int focus) const
{
    if (units_.isEmpty()) {
        return QString();
    }
    const int lastIndex = units_.size() - 1;
    anchor = std::clamp(anchor, 0, lastIndex);
    focus = std::clamp(focus, 0, lastIndex);
    if (anchor > focus) {
        std::swap(anchor, focus);
    }

    QString result;
    int currentLine = units_.at(anchor).line;
    for (int index = anchor; index <= focus; ++index) {
        const TextUnit &unit = units_.at(index);
        if (unit.line != currentLine) {
            // The spaces that padded the line being left are not part of the
            // text: they were the engine's way of reaching the next one.
            while (result.endsWith(QLatin1Char(' '))) {
                result.chop(1);
            }
            result.append(QLatin1Char('\n'));
            currentLine = unit.line;
        }
        result.append(unit.text);
    }
    return result;
}

void TextLayer::wordRange(int index, int *first, int *last) const
{
    if (first != nullptr) {
        *first = -1;
    }
    if (last != nullptr) {
        *last = -1;
    }
    if (index < 0 || index >= units_.size()) {
        return;
    }
    const int line = units_.at(index).line;
    const UnitKind kind = kindOf(units_.at(index).text);
    if (kind == UnitKind::Ideograph) {
        // Where one word ends and the next begins in Chinese is not visible in
        // the characters, and a run of them is a sentence rather than a word.
        // One character is short, but it is never wrong.
        if (first != nullptr) {
            *first = index;
        }
        if (last != nullptr) {
            *last = index;
        }
        return;
    }

    int start = index;
    while (start > 0 && units_.at(start - 1).line == line &&
           kindOf(units_.at(start - 1).text) == kind) {
        --start;
    }
    int end = index;
    while (end + 1 < units_.size() && units_.at(end + 1).line == line &&
           kindOf(units_.at(end + 1).text) == kind) {
        ++end;
    }
    if (first != nullptr) {
        *first = start;
    }
    if (last != nullptr) {
        *last = end;
    }
}

void TextLayer::lineRange(int index, int *first, int *last) const
{
    if (first != nullptr) {
        *first = -1;
    }
    if (last != nullptr) {
        *last = -1;
    }
    if (index < 0 || index >= units_.size()) {
        return;
    }
    const int line = units_.at(index).line;
    int start = index;
    while (start > 0 && units_.at(start - 1).line == line) {
        --start;
    }
    int end = index;
    while (end + 1 < units_.size() && units_.at(end + 1).line == line) {
        ++end;
    }
    if (first != nullptr) {
        *first = start;
    }
    if (last != nullptr) {
        *last = end;
    }
}

} // namespace vshot
