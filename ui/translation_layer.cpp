// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#include "translation_layer.hpp"

#include <QFont>
#include <QFontMetrics>
#include <QFontMetricsF>
#include <QGuiApplication>
#include <QHash>
#include <QPainter>

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <functional>

namespace vshot {
namespace {

/// How many device pixels either side of a line the background is read from.
constexpr int kSampleBand = 2;

/// How far the fill reaches past the line's own box, to take the antialiased
/// edge of the original glyphs with it.
constexpr int kFillMargin = 1;

/// The light and dark the translated glyphs are drawn in.  One of the two is
/// chosen from the fill, so the text reads whatever the frame under it is.
constexpr int kTextLuminanceThreshold = 140;
const QColor kDarkGlyph(20, 20, 20);
const QColor kLightGlyph(245, 245, 245);

/// How far a pixel has to differ from the line's background before it counts as
/// glyph rather than background noise.  The antialiased edge of a glyph fades
/// into the background, and the faintest pixel worth counting sits about a tenth
/// of the way in, which is this far from it.
constexpr int kInkDistance = 24;

/// How much of its font size a line's glyphs cover, from the top of the tallest
/// ascender to the bottom of the deepest descender.  Measured at 14, 16, 20, 24,
/// 32 and 48 px across Latin, CJK and mixed lines: 0.92 to 0.94 of the pixel
/// size every time, so a font size read back from the ink wants dividing by it.
constexpr double kInkOfFontSize = 0.94;

/// What a line's box is worth as a font size when the frame cannot be read.  The
/// same measurements put the box at 1.33 to 1.47 times the font size, so this
/// lands within a few percent of the ink reading -- and it is also the ceiling
/// on that reading, which is what keeps a background the sampler cannot separate
/// from its text from inflating the translation back to the box's own size.
constexpr double kBoxOfFontSize = 0.72;

QFont fontFor(const QString &family, int pixels)
{
    QFont font;
    if (family.isEmpty()) {
        font = QGuiApplication::font();
    } else {
        font.setFamily(family);
    }
    font.setPixelSize(std::max(1, pixels));
    return font;
}

/// How wide `text` really is at `pixels`: the advance, or the ink's own extent
/// when that is wider (an overhanging last glyph).  Taking the larger is what
/// lets the fill grow to cover exactly what is drawn rather than clipping the
/// overhang the advance does not account for.
int neededWidth(const QFont &font, const QString &text)
{
    if (text.isEmpty()) {
        return 0;
    }
    const QFontMetricsF metrics{font};
    const int advance = static_cast<int>(std::ceil(metrics.horizontalAdvance(text)));
    const int inkRight = static_cast<int>(std::ceil(metrics.boundingRect(text).right()));
    return std::max(advance, inkRight);
}

/// Which writing system a string asks for, from its first non-ASCII character.
enum class Script {
    Latin,
    Cjk,
    Korean,
    Other,
};

Script scriptOf(const QString &text)
{
    for (const QChar &character : text) {
        const ushort code = character.unicode();
        // ASCII and the spaces around it are drawn by any family.
        if (character.isSpace() || code < 0x80) {
            continue;
        }
        // Hangul syllables sit above the Han block, so they are told apart
        // before the CJK range swallows them.
        if (code >= 0xAC00 && code <= 0xD7A3) {
            return Script::Korean;
        }
        if (code >= 0x2E80) {
            return Script::Cjk;
        }
        return Script::Other;
    }
    return Script::Latin;
}

QStringList candidatesFor(Script script)
{
    switch (script) {
    case Script::Cjk:
        return {QStringLiteral("Noto Sans CJK SC"), QStringLiteral("Noto Sans CJK JP"),
                QStringLiteral("Noto Sans SC"), QStringLiteral("Source Han Sans SC"),
                QStringLiteral("WenQuanYi Zen Hei"), QStringLiteral("Microsoft YaHei"),
                QStringLiteral("PingFang SC")};
    case Script::Korean:
        return {QStringLiteral("Noto Sans CJK KR"), QStringLiteral("Noto Sans KR"),
                QStringLiteral("Source Han Sans KR"), QStringLiteral("Malgun Gothic")};
    case Script::Other:
        return {QStringLiteral("Noto Sans"), QStringLiteral("DejaVu Sans"),
                QStringLiteral("Liberation Sans")};
    case Script::Latin:
        break;
    }
    return {QStringLiteral("Noto Sans"), QStringLiteral("DejaVu Sans"),
            QStringLiteral("Liberation Sans")};
}

/// Whether a named family can draw every non-ASCII character in `text`.  A
/// family that is missing resolves to some substitute, which is what makes the
/// last-resort step safe rather than a guess at what is installed.
bool familyCovers(const QString &name, const QString &text)
{
    if (name.isEmpty()) {
        return false;
    }
    const QFontMetricsF metrics{QFont(name)};
    for (const QChar &character : text) {
        if (character.isSpace() || character.unicode() < 0x80) {
            continue;
        }
        if (!metrics.inFont(character)) {
            return false;
        }
    }
    return true;
}

LogicalRect unionOf(const LogicalRect &first, const LogicalRect &second)
{
    const std::int64_t left = std::min<std::int64_t>(first.x, second.x);
    const std::int64_t top = std::min<std::int64_t>(first.y, second.y);
    const std::int64_t right =
        std::max<std::int64_t>(first.right(), second.right());
    const std::int64_t bottom =
        std::max<std::int64_t>(first.bottom(), second.bottom());
    return LogicalRect{static_cast<std::int32_t>(left), static_cast<std::int32_t>(top),
                       static_cast<std::uint32_t>(right - left),
                       static_cast<std::uint32_t>(bottom - top)};
}

/// The font size to draw a replacement of a line at, from the ink the original
/// glyphs cover (`inkHeight`, zero when that could not be read) and the height of
/// the box the engine put around them.
///
/// The ink is the direct reading of the line's own size, so it wins; the box is
/// the fallback and the ceiling.  Never below one pixel: a line the engine
/// reported as a sliver still has to be drawn.
int estimatedFontPixels(int inkHeight, int boxHeight)
{
    const int box = std::max(1, boxHeight);
    const int fromBox = std::max(1, static_cast<int>(std::lround(box * kBoxOfFontSize)));
    if (inkHeight < 2) {
        return fromBox;
    }
    const int fromInk =
        std::max(1, static_cast<int>(std::lround(inkHeight / kInkOfFontSize)));
    return std::min(fromInk, fromBox);
}

} // namespace

QVector<TranslatedLine> translatedLines(const TextLayer &layer)
{
    QVector<TranslatedLine> result;
    if (!layer.hasGeometry()) {
        return result;
    }
    int index = 0;
    while (index < layer.count()) {
        const int line = layer.unit(index).line;
        LogicalRect box = layer.unit(index).rect;
        ++index;
        while (index < layer.count() && layer.unit(index).line == line) {
            box = unionOf(box, layer.unit(index).rect);
            ++index;
        }
        TranslatedLine entry;
        entry.source = box;
        entry.fill = box;
        entry.text = layer.lineText(line);
        result.append(entry);
    }
    return result;
}

QColor sampleLineBackground(const QImage &source, const QRect &line)
{
    if (source.isNull() || line.isEmpty()) {
        return QColor(255, 255, 255);
    }
    struct Bucket {
        qint64 red = 0;
        qint64 green = 0;
        qint64 blue = 0;
        qint64 count = 0;
    };
    QHash<int, Bucket> buckets;
    qint64 total = 0;
    const auto addPixel = [&](int x, int y) {
        if (x < 0 || y < 0 || x >= source.width() || y >= source.height()) {
            return;
        }
        const QRgb pixel = source.pixel(x, y);
        const int key = ((qRed(pixel) >> 4) << 8) | ((qGreen(pixel) >> 4) << 4) |
            (qBlue(pixel) >> 4);
        Bucket &bucket = buckets[key];
        bucket.red += qRed(pixel);
        bucket.green += qGreen(pixel);
        bucket.blue += qBlue(pixel);
        ++bucket.count;
        ++total;
    };

    const int left = line.x();
    const int right = line.x() + line.width() - 1;
    // The band just outside the glyph box, above and below it.  Reading through
    // the box's own pixels would catch the glyphs, which is exactly the colour
    // the fill has to avoid.
    for (int y = line.y() - kSampleBand; y < line.y(); ++y) {
        for (int x = left; x <= right; ++x) {
            addPixel(x, y);
        }
    }
    for (int y = line.y() + line.height(); y < line.y() + line.height() + kSampleBand; ++y) {
        for (int x = left; x <= right; ++x) {
            addPixel(x, y);
        }
    }
    // A line pressed against both edges of the frame has no band outside it, so
    // its own box is the only thing left to read.
    if (total == 0) {
        for (int y = line.y(); y < line.y() + line.height(); ++y) {
            for (int x = left; x <= right; ++x) {
                addPixel(x, y);
            }
        }
    }
    if (total == 0) {
        return QColor(255, 255, 255);
    }

    // The most common quantised colour, averaged over the pixels in its own
    // bucket so a flat background comes back as itself rather than the bucket's
    // centre.  Ties fall to the lower key, which keeps the choice stable.
    int bestKey = -1;
    qint64 bestCount = 0;
    for (auto it = buckets.constBegin(); it != buckets.constEnd(); ++it) {
        if (it.value().count > bestCount ||
            (it.value().count == bestCount && (bestKey < 0 || it.key() < bestKey))) {
            bestCount = it.value().count;
            bestKey = it.key();
        }
    }
    const Bucket &best = buckets.value(bestKey);
    return QColor(static_cast<int>(best.red / best.count),
                  static_cast<int>(best.green / best.count),
                  static_cast<int>(best.blue / best.count));
}

int sampleLineInk(const QImage &source, const QRect &line, const QColor &background)
{
    if (source.isNull() || line.isEmpty()) {
        return 0;
    }
    const int left = std::max(0, line.x());
    const int right = std::min(source.width(), line.x() + line.width());
    const int top = std::max(0, line.y());
    const int bottom = std::min(source.height(), line.y() + line.height());
    if (left >= right || top >= bottom) {
        return 0;
    }
    // A row counts when enough of it stands out from the background: a single
    // pixel is noise from a photograph or a compression artifact, while a row
    // through a glyph has a run of ink in it.  Two percent of the line's width
    // is that line between the two, and a narrow line still needs one pixel.
    const int needed = std::max(1, (right - left) / 50);
    int first = -1;
    int last = -1;
    for (int y = top; y < bottom; ++y) {
        int differing = 0;
        for (int x = left; x < right && differing < needed; ++x) {
            const QRgb pixel = source.pixel(x, y);
            const int distance = std::max({std::abs(qRed(pixel) - background.red()),
                                           std::abs(qGreen(pixel) - background.green()),
                                           std::abs(qBlue(pixel) - background.blue())});
            if (distance > kInkDistance) {
                ++differing;
            }
        }
        if (differing >= needed) {
            if (first < 0) {
                first = y;
            }
            last = y;
        }
    }
    // Only the rows are of interest, so gaps between them -- the space between
    // two words, a hole in a wide glyph -- still count as ink.  What the height
    // is for is the size the glyphs were drawn at, and that is the outer edge.
    return first < 0 ? 0 : last - first + 1;
}

QString translatedFamily(const QString &preferred, const QString &text)
{
    // The desktop's own font comes second, right after the caller's preference:
    // a translation has to read like the rest of the desktop, and the family
    // behind the application font is the one every other window is drawn in --
    // often one the built-in lists below do not even name (a rounded CJK face
    // set as the desktop font, say).  The lists stay behind it for the desktop
    // font that cannot draw the text's own script, which would otherwise leave
    // a line of boxes.
    const QString wanted = preferred.trimmed();
    QStringList candidates;
    if (!wanted.isEmpty()) {
        candidates << wanted;
    }
    candidates << QGuiApplication::font().family();
    candidates << candidatesFor(scriptOf(text));
    for (const QString &name : candidates) {
        if (familyCovers(name, text)) {
            return name;
        }
    }
    return QGuiApplication::font().family();
}

LineFit fitTranslatedLine(const QString &text, const LogicalRect &line,
                          const QColor &background, const QString &family,
                          const LogicalRect &limit, int inkHeight)
{
    LineFit fit;
    fit.textColor =
        background.lightness() > kTextLuminanceThreshold ? kDarkGlyph : kLightGlyph;
    const QString resolved = translatedFamily(family, text);

    // The line's own size, from the frame when it could be read and from the
    // engine's box when it could not.  Not the box itself: it is the detection
    // model's, and it stands about a third taller than the glyphs inside it.
    const int start = estimatedFontPixels(inkHeight, static_cast<int>(line.height));
    // Below half the size the line would be drawn at the translation stops
    // reading as a replacement of it, so the fill grows instead.
    const int floorPixels = std::max(1, start / 2);
    int pixels = start;
    int width = neededWidth(fontFor(resolved, pixels), text);
    while (pixels > floorPixels && width > static_cast<int>(line.width)) {
        --pixels;
        width = neededWidth(fontFor(resolved, pixels), text);
    }

    const int lineWidth = static_cast<int>(line.width);
    int need = std::max(lineWidth, width);
    const LogicalRect image = limit.isEmpty() ? line : limit;
    std::int64_t left = line.x;
    if (left + need > image.right()) {
        // Keep the text on the image by pulling the fill left rather than
        // letting it run off the right edge.
        left = std::max<std::int64_t>(image.x, image.right() - need);
    }
    if (need > static_cast<int>(image.width)) {
        // The image is narrower than the translation: there is nowhere to grow,
        // so the fill is the image and the text is clipped by it.
        need = static_cast<int>(image.width);
        left = image.x;
    }

    // The fill does two jobs at once: it has to cover every pixel of the
    // *original* line -- leave a rim of the old glyphs behind and the
    // translation reads as sitting on top of them rather than replacing them --
    // and every pixel of the *translation*, or the clip that keeps the mark
    // inside its box cuts the new glyphs instead.  So it is the taller of the
    // line's own box and the text's real ink, and never the font's nominal
    // metrics box: that one stands about 1.45 times the glyph size, which
    // reaches into the lines above and below and paints their backgrounds into
    // this one.
    const QFontMetricsF metrics{fontFor(resolved, pixels)};
    const QRectF ink = text.isEmpty() ? QRectF() : metrics.tightBoundingRect(text);
    const int drawnInk = std::max(1, static_cast<int>(std::ceil(ink.height())));
    // The engine's box bounds the glyphs, but their antialiased edge sits just
    // outside it; a pixel of margin takes that with the fill.
    const int sourceHeight = static_cast<int>(line.height) + 2 * kFillMargin;
    const int glyphHeight = std::max(drawnInk, sourceHeight);
    const std::int64_t center =
        static_cast<std::int64_t>(line.y) + static_cast<std::int64_t>(line.height) / 2;
    std::int64_t top = center - glyphHeight / 2;
    // Stay on the image vertically too: a line at the very top or bottom keeps
    // its own edge rather than growing past it.
    top = std::max<std::int64_t>(top, image.y);
    if (top + glyphHeight > image.bottom()) {
        top = std::max<std::int64_t>(static_cast<std::int64_t>(image.y),
                                     image.bottom() - glyphHeight);
    }

    fit.fontPixels = pixels;
    fit.fill = LogicalRect{static_cast<std::int32_t>(left), static_cast<std::int32_t>(top),
                           static_cast<std::uint32_t>(std::max(0, need)),
                           static_cast<std::uint32_t>(glyphHeight)};
    return fit;
}

QVector<TranslatedLine> placedTranslations(const TextLayer &layer, const QImage &source,
                                           const LogicalRect &geometry, double scale,
                                           const LogicalRect &limit, const QString &family)
{
    QVector<TranslatedLine> lines = translatedLines(layer);
    const double ratio = scale > 0.0 ? scale : 1.0;
    for (TranslatedLine &line : lines) {
        const double x = (static_cast<double>(line.source.x) - geometry.x) * ratio;
        const double y = (static_cast<double>(line.source.y) - geometry.y) * ratio;
        const QRect device(static_cast<int>(std::lround(x)), static_cast<int>(std::lround(y)),
                           static_cast<int>(std::lround(line.source.width * ratio)),
                           static_cast<int>(std::lround(line.source.height * ratio)));
        line.background = sampleLineBackground(source, device);
        // The size the translation is drawn at comes from the glyphs that were
        // there, read off the frame in its device pixels and brought back to the
        // overlay's logical ones, which is what the fit is written in.
        const int ink = sampleLineInk(source, device, line.background);
        const int logicalInk =
            ink > 0 ? std::max(1, static_cast<int>(std::lround(ink / ratio))) : 0;
        const LineFit fit =
            fitTranslatedLine(line.text, line.source, line.background, family, limit, logicalInk);
        line.fill = fit.fill;
        line.fontPixels = fit.fontPixels;
        line.textColor = fit.textColor;
        line.family = translatedFamily(family, line.text);
    }
    return lines;
}

namespace {

/// Lays the line's own background down over everything the original glyphs
/// touched.  The clip is what keeps the promise that the fill covers exactly
/// what was drawn: a line grown against the image edge cannot paint past the
/// box.
void fillLine(QPainter &painter, const QRectF &target, const TranslatedLine &line)
{
    if (target.isEmpty()) {
        return;
    }
    painter.save();
    painter.setClipRect(target);
    painter.fillRect(target, line.background);
    painter.restore();
}

/// Writes the line's text into its fill.
void drawLineText(QPainter &painter, const QRectF &target, qreal fontScale,
                  const TranslatedLine &line)
{
    if (target.isEmpty() || line.text.isEmpty()) {
        return;
    }
    painter.save();
    painter.setClipRect(target);
    const int pixels = std::max(1, static_cast<int>(std::lround(line.fontPixels * fontScale)));
    const QFont font = fontFor(line.family, pixels);
    painter.setFont(font);
    painter.setPen(line.textColor);
    // The baseline is chosen so the *ink* is centred in the box, not the font's
    // nominal metrics box: the two can sit a few pixels apart, and centring the
    // wrong one is what pushes the descenders past the clip.
    const QFontMetricsF metrics{font};
    const QRectF ink = metrics.boundingRect(line.text);
    const qreal baseline = target.center().y() - (ink.top() + ink.bottom()) / 2.0;
    painter.drawText(QPointF(target.left(), baseline), line.text);
    painter.restore();
}

} // namespace

void paintTranslatedLine(QPainter &painter, const QRectF &target, qreal fontScale,
                         const TranslatedLine &line)
{
    fillLine(painter, target, line);
    drawLineText(painter, target, fontScale, line);
}

void paintTranslation(QPainter &painter, const QVector<TranslatedLine> &lines,
                      const std::function<QRectF(const TranslatedLine &)> &target, qreal fontScale)
{
    // Every fill first, then every text.  A line's fill is its font's metrics
    // box, which stands taller than the ink inside it -- 77 px of glyphs came
    // out of a 112 px box on a real frame -- so the fills of closely spaced
    // lines overlap, and painting a line's text before its neighbour's fill
    // lets that fill repaint the neighbour's descenders.  Measured on a 26 px
    // line 30 px above the next: the later fill took 104 of the upper line's
    // 1108 ink pixels.  Two passes make that impossible whatever the spacing.
    for (const TranslatedLine &line : lines) {
        fillLine(painter, target(line), line);
    }
    for (const TranslatedLine &line : lines) {
        drawLineText(painter, target(line), fontScale, line);
    }
}

} // namespace vshot
