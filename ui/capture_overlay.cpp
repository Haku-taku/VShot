// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#include "capture_overlay.hpp"
#include "config.hpp"
#include "i18n.hpp"
#include "text_size.hpp"

#include <LayerShellQt/Window>

#include <QAbstractButton>
#include <QApplication>
#include <QCloseEvent>
#include <QConicalGradient>
#include <QCoreApplication>
#include <QDataStream>
#include <QDir>
#include <QEventLoop>
#include <QFile>
#include <QFont>
#include <QFontDatabase>
#include <QFontMetrics>
#include <QFrame>
#include <QHBoxLayout>
#include <QHash>
#include <QIcon>
#include <QImage>
#include <QImageReader>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QKeyEvent>
#include <QLabel>
#include <QLinearGradient>
#include <QLineEdit>
#include <QLineF>
#include <QListWidget>
#include <QLocalSocket>
#include <QMouseEvent>
#include <QPainter>
#include <QPainterPath>
#include <QPolygonF>
#include <QProcess>
#include <QPushButton>
#include <QScreen>
#include <QSignalBlocker>
#include <QSizePolicy>
#include <QSlider>
#include <QSpinBox>
#include <QStyle>
#include <QStyledItemDelegate>
#include <QTemporaryDir>
#include <QTimer>
#include <QToolButton>
#include <QStringList>
#include <QUrl>
#include <QWindow>
#include <QSocketNotifier>

#include <algorithm>
#include <cerrno>
#include <cmath>
#include <cstdio>
#include <fcntl.h>
#include <functional>
#include <limits>
#include <unistd.h>

namespace vshot {
namespace {

constexpr int kHandleRadius = 6;
constexpr int kMinimumSelection = 5;
// Widest window label the picker's size pill shows before eliding it.
constexpr int kPickerLabelWidth = 360;
// How often the picker may ask the CLI for a fresh candidate list while the
// pointer travels.  Fast enough that a workspace switch is reflected by the
// time the pointer reaches the window it is heading for, slow enough that a
// drag across the screen does not turn into a stream of compositor queries.
constexpr int kCandidateRefreshIntervalMs = 150;
// And how often it asks when the pointer is not moving at all: a window can
// move, or another monitor's workspace can be switched, without this surface
// seeing an event, and the highlight must not keep describing what used to be
// there.  Only the picker's lifetime pays for this.
constexpr int kCandidateRefreshPollMs = 300;
constexpr int kMaxUndoSteps = 100;
constexpr int kLoupeRadius = 7;
constexpr int kLoupeZoom = 8;
constexpr int kLoupeDiameter = (2 * kLoupeRadius + 1) * kLoupeZoom;
constexpr int kLoupeMargin = 10;

// The text-selection colour and the outline that shows what was recognized: a
// translucent blue fill over the characters in the range and a lighter blue
// line around each recognized line.  Both are legible over an undimmed
// screenshot without hiding the text underneath.
const QColor kTextSelectionFill(64, 132, 240, 110);
const QColor kTextOutline(150, 195, 255, 140);

std::int64_t right(const LogicalRect &rect)
{
    return rect.right();
}

std::int64_t bottom(const LogicalRect &rect)
{
    return rect.bottom();
}

bool intersection(const LogicalRect &first, const LogicalRect &second, LogicalRect *result)
{
    const std::int64_t left = std::max<std::int64_t>(first.x, second.x);
    const std::int64_t top = std::max<std::int64_t>(first.y, second.y);
    const std::int64_t rightEdge = std::min(right(first), right(second));
    const std::int64_t bottomEdge = std::min(bottom(first), bottom(second));
    if (rightEdge <= left || bottomEdge <= top || result == nullptr) {
        return false;
    }
    result->x = static_cast<std::int32_t>(left);
    result->y = static_cast<std::int32_t>(top);
    result->width = static_cast<std::uint32_t>(rightEdge - left);
    result->height = static_cast<std::uint32_t>(bottomEdge - top);
    return true;
}

LogicalRect rectFromEdges(std::int64_t left, std::int64_t top, std::int64_t rightEdge,
                          std::int64_t bottomEdge)
{
    LogicalRect result;
    result.x = static_cast<std::int32_t>(left);
    result.y = static_cast<std::int32_t>(top);
    result.width = static_cast<std::uint32_t>(rightEdge - left);
    result.height = static_cast<std::uint32_t>(bottomEdge - top);
    return result;
}

// Surface a given output's overlay canvas covers, in global logical pixels.
// Region capture and plain pin editing cover exactly the output; the pin
// editor widens the surface to its whole screen, so every local<->global
// conversion keys off this rect rather than the output geometry.
const LogicalRect &surfaceOf(const OutputSession &output)
{
    return output.surface.width > 0 && output.surface.height > 0 ? output.surface
                                                                 : output.geometry;
}

QRectF localRect(const OutputSession &output, const LogicalRect &rect, const QSize &size)
{
    const LogicalRect &surface = surfaceOf(output);
    const double sx = surface.width == 0
        ? 1.0
        : static_cast<double>(size.width()) / static_cast<double>(surface.width);
    const double sy = surface.height == 0
        ? 1.0
        : static_cast<double>(size.height()) / static_cast<double>(surface.height);
    return QRectF((static_cast<double>(rect.x) - surface.x) * sx,
                  (static_cast<double>(rect.y) - surface.y) * sy,
                  static_cast<double>(rect.width) * sx,
                  static_cast<double>(rect.height) * sy);
}

QRect sourceRect(const OutputSession &output, const LogicalRect &rect)
{
    const std::int64_t x = (static_cast<std::int64_t>(rect.x) - output.geometry.x) * output.scale;
    const std::int64_t y = (static_cast<std::int64_t>(rect.y) - output.geometry.y) * output.scale;
    const std::int64_t width = static_cast<std::int64_t>(rect.width) * output.scale;
    const std::int64_t height = static_cast<std::int64_t>(rect.height) * output.scale;
    return QRect(static_cast<int>(x), static_cast<int>(y), static_cast<int>(width),
                 static_cast<int>(height));
}

// The exact inverse of `sourceRect`: a rect in one output's captured device
// pixels becomes the global logical rect the overlay draws in.  The engine's
// coordinates are counted from a crop of that frame, so this is also the one
// place a placement's origin comes from.  The division stays in `double` and
// each edge is rounded only at the end, the same way the text layer places a
// box, so a rect that was mapped out and back comes home.
LogicalRect logicalFromSource(const OutputSession &output, const QRect &rect)
{
    const double scale = output.scale > 0 ? static_cast<double>(output.scale) : 1.0;
    const double x = static_cast<double>(output.geometry.x) + static_cast<double>(rect.x()) / scale;
    const double y = static_cast<double>(output.geometry.y) + static_cast<double>(rect.y()) / scale;
    const double width = static_cast<double>(rect.width()) / scale;
    const double height = static_cast<double>(rect.height()) / scale;
    LogicalRect result;
    result.x = static_cast<std::int32_t>(std::lround(x));
    result.y = static_cast<std::int32_t>(std::lround(y));
    result.width = static_cast<std::uint32_t>(std::max(0L, std::lround(width)));
    result.height = static_cast<std::uint32_t>(std::max(0L, std::lround(height)));
    return result;
}

QPointF localPoint(const OutputSession &output, const Point &point, const QSize &size)
{
    const LogicalRect &surface = surfaceOf(output);
    const double sx = surface.width == 0
        ? 1.0
        : static_cast<double>(size.width()) / static_cast<double>(surface.width);
    const double sy = surface.height == 0
        ? 1.0
        : static_cast<double>(size.height()) / static_cast<double>(surface.height);
    return QPointF((static_cast<double>(point.x) - surface.x) * sx,
                   (static_cast<double>(point.y) - surface.y) * sy);
}

// Two pi, spelled out rather than read from a platform's `M_PI`: the wave the
// preview draws and the one the Rust renderer bakes into the PNG have to be the
// same curve, and the constant is the one place that could silently differ.
constexpr double kTau = 6.283185307179586476925286766559;

// Samples the sine wave along the segment `start`..`end` as a polyline, exactly
// as the Rust renderer's `wave_polyline` does, so a wave drawn here and the
// same wave baked into the final PNG agree line for line.
//
// `start` and `end` are overlay-local logical pixels.  `widthLogical` is the
// annotation's width in logical pixels; the amplitude and the wavelength are
// its `max(width * 2, 4)` and `max(width * 6, 18)`, in logical pixels, so the
// shape does not depend on the output's scale.  `scale` is that output's device
// scale and sets only the sampling distance: one sample per *device* pixel
// means a step of `1 / scale` logical pixels, which is what the Rust side's
// `n = ceil(length_device) + 1` produces.
//
// The phase finishes on a whole number of cycles -- `cycles = max(1, round(L /
// wavelength))`, with the wavelength actually used being `L / cycles` -- so both
// ends come back onto the centre line.  Without that step the far end is left
// wherever the phase happened to be, off to one side, and the wave stops
// reading as one drawn from A to B.
//
// A zero-length segment returns the single `start` point, which the callers
// turn into a dot.
QVector<QPointF> wavePolyline(const QPointF &start, const QPointF &end, int widthLogical,
                              double scale)
{
    const QPointF delta = end - start;
    const double length = std::hypot(delta.x(), delta.y());
    if (length <= 0.0) {
        return {start};
    }
    const double amplitude = std::max(widthLogical * 2, 4);
    const double wavelength = std::max(widthLogical * 6, 18);
    // One sample per device pixel, plus both endpoints.
    const double step = 1.0 / std::max(1.0, scale);
    const int n = std::max(2, static_cast<int>(std::ceil(length / step)) + 1);
    const QPointF dir(delta.x() / length, delta.y() / length);
    // The 90-degree rotation of `dir`: the direction the wave deviates in.
    const QPointF normal(-dir.y(), dir.x());
    const double cycles = std::max(1.0, std::round(length / wavelength));
    const double radiansPerPixel = kTau / (length / cycles);
    QVector<QPointF> points;
    points.reserve(n);
    const double last = static_cast<double>(n - 1);
    for (int i = 0; i < n; ++i) {
        const double u = (static_cast<double>(i) / last) * length;
        const double offset = amplitude * std::sin(radiansPerPixel * u);
        points.append(start + dir * u + normal * offset);
    }
    return points;
}

// The pen tool's geometry.  The model stores one handle per anchor, interleaved
// [anchor0, handleOut0, anchor1, handleOut1, ...] -- exactly the shape the wire
// format carries -- and derives the incoming side by mirroring, so a handle is
// symmetric by construction and the preview, the cached raster and the JSON
// cannot disagree about the curve.

// One point on the cubic whose control points are `p0`..`p3`, at parameter `t`.
// The Bernstein form is the one `QPainterPath::cubicTo` evaluates, so a sample
// taken here lies on the very curve the painter draws.
QPointF cubicPoint(const QPointF &p0, const QPointF &p1, const QPointF &p2, const QPointF &p3,
                   double t)
{
    const double u = 1.0 - t;
    const double a = u * u * u;
    const double b = 3.0 * u * u * t;
    const double c = 3.0 * u * t * t;
    const double d = t * t * t;
    return QPointF(a * p0.x() + b * p1.x() + c * p2.x() + d * p3.x(),
                   a * p0.y() + b * p1.y() + c * p2.y() + d * p3.y());
}

// The incoming control point of an anchor: the mirror of its outgoing handle
// through the anchor itself.
QPointF mirrorHandle(const QPointF &anchor, const QPointF &handleOut)
{
    return QPointF(2.0 * anchor.x() - handleOut.x(), 2.0 * anchor.y() - handleOut.y());
}

// How many anchors a pen path has: the interleaved list is always even, and the
// anchors are half of it.
int bezierAnchors(const QVector<Point> &points)
{
    return static_cast<int>(points.size() / 2);
}

// The same list in the floating-point coordinates the painter works in.
QVector<QPointF> pointFs(const QVector<Point> &points)
{
    QVector<QPointF> result;
    result.reserve(points.size());
    for (const Point &point : points) {
        result.append(QPointF(point.x, point.y));
    }
    return result;
}

// The pen path as a QPainterPath, from points already in the painter's own
// coordinates.  The live preview, the cached raster and the hit test all build
// it here, so the three cannot disagree about the curve.  A closed path joins
// the last anchor back to the first through both their handles.
QPainterPath bezierPathAt(const QVector<QPointF> &at, bool closed)
{
    QPainterPath path;
    const int anchors = static_cast<int>(at.size() / 2);
    if (anchors <= 0) {
        return path;
    }
    path.moveTo(at.at(0));
    const int segments = closed ? anchors : anchors - 1;
    for (int index = 0; index < segments; ++index) {
        const int next = (index + 1) % anchors;
        path.cubicTo(at.at(2 * index + 1), mirrorHandle(at.at(2 * next), at.at(2 * next + 1)),
                     at.at(2 * next));
    }
    if (closed) {
        path.closeSubpath();
    }
    return path;
}

// The same path from the session-space points the model stores.
QPainterPath bezierPath(const QVector<Point> &points, bool closed)
{
    return bezierPathAt(pointFs(points), closed);
}

// The pen path's ink, in whatever coordinate space the caller has converted its
// points into.  The preview and the committed mark both draw through here, so a
// closed path cannot end up filled in one place and only stroked in another.
void paintBezierInk(QPainter *painter, const QVector<QPointF> &at, bool closed, const QColor &color,
                    int width)
{
    if (at.isEmpty()) {
        return;
    }
    if (at.size() < 4) {
        // A click that was never dragged past its own anchor is a dot, the same
        // ink the freehand pen gives one.
        painter->setPen(QPen(color, width, Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
        painter->setBrush(Qt::NoBrush);
        painter->drawPoint(at.constFirst());
        return;
    }
    const QPainterPath path = bezierPathAt(at, closed);
    if (!closed) {
        painter->setPen(QPen(color, width, Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
        painter->setBrush(Qt::NoBrush);
        painter->drawPath(path);
        return;
    }
    // Fill first and stroke second, the order the Rust renderer bakes in.  The
    // fill is the stroke's own colour at half its alpha, floored: that is what
    // "a translucent fill under a solid outline" means for a colour the user
    // picked an opacity for.
    QColor fill = color;
    fill.setAlpha(color.alpha() / 2);
    painter->fillPath(path, fill);
    painter->strokePath(path, QPen(color, width, Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
}

// The same path sampled into a polyline, about one sample per two logical
// pixels of control polygon so the sample follows the curve's own turning.  The
// samples lie on the curve, which is what the tight bounding box and the hit
// test need: a box over the control points alone would be far larger than the
// ink, and one over the anchors alone would clip the very bulge the handles pull
// out.
QVector<QPointF> bezierPolyline(const QVector<Point> &points, bool closed)
{
    QVector<QPointF> samples;
    const int anchors = bezierAnchors(points);
    if (anchors <= 0) {
        return samples;
    }
    const QVector<QPointF> at = pointFs(points);
    samples.append(at.at(0));
    const int segments = closed ? anchors : anchors - 1;
    for (int index = 0; index < segments; ++index) {
        const int next = (index + 1) % anchors;
        const QPointF &start = at.at(2 * index);
        const QPointF &handle = at.at(2 * index + 1);
        const QPointF &end = at.at(2 * next);
        const QPointF incoming = mirrorHandle(end, at.at(2 * next + 1));
        const double span = QLineF(start, handle).length() + QLineF(handle, incoming).length() +
            QLineF(incoming, end).length();
        const int steps = std::clamp(static_cast<int>(std::ceil(span / 2.0)), 8, 96);
        for (int step = 1; step <= steps; ++step) {
            samples.append(cubicPoint(start, handle, incoming, end,
                                      static_cast<double>(step) / steps));
        }
    }
    return samples;
}

// The tight box of a sampled polyline in session coordinates, rounded outward,
// or `false` when there is nothing to bound.  Folded by hand rather than with
// `QRectF::united`: uniting onto a null rect returns the other operand, which
// turns a box that was never seeded into whatever the last point happened to be
// -- a bounding box that collapses to a point, and stale ink left on screen.
bool polylineLogicalBounds(const QVector<QPointF> &samples, LogicalRect *bounds)
{
    if (samples.isEmpty()) {
        return false;
    }
    double left = samples.constFirst().x();
    double right = left;
    double top = samples.constFirst().y();
    double bottom = top;
    for (const QPointF &point : samples) {
        left = std::min(left, point.x());
        right = std::max(right, point.x());
        top = std::min(top, point.y());
        bottom = std::max(bottom, point.y());
    }
    const std::int32_t x = static_cast<std::int32_t>(std::floor(left));
    const std::int32_t y = static_cast<std::int32_t>(std::floor(top));
    bounds->x = x;
    bounds->y = y;
    bounds->width = static_cast<std::uint32_t>(
        static_cast<std::int64_t>(std::ceil(right)) - x + 1);
    bounds->height = static_cast<std::uint32_t>(
        static_cast<std::int64_t>(std::ceil(bottom)) - y + 1);
    return true;
}

QString toolName(Tool tool)
{
    switch (tool) {
    case Tool::Rectangle:
        return QStringLiteral("rectangle");
    case Tool::Ellipse:
        return QStringLiteral("ellipse");
    case Tool::Arrow:
        return QStringLiteral("arrow");
    case Tool::Line:
        return QStringLiteral("line");
    case Tool::Wave:
        return QStringLiteral("wave");
    case Tool::Bezier:
        return QStringLiteral("bezier");
    case Tool::Pen:
        return QStringLiteral("pen");
    case Tool::Mosaic:
        return QStringLiteral("mosaic");
    case Tool::Text:
        return QStringLiteral("text");
    case Tool::Number:
        return QStringLiteral("number");
    case Tool::Select:
        return QStringLiteral("select");
    }
    return QStringLiteral("pen");
}

/// The inverse of [`toolName`], for a name that came out of the config file.
/// An unrecognized name is a typo in a file the user can edit, so it falls
/// back to the tool a session has always started with.
Tool toolForName(const QString &name)
{
    if (name == QStringLiteral("rectangle")) {
        return Tool::Rectangle;
    }
    if (name == QStringLiteral("ellipse")) {
        return Tool::Ellipse;
    }
    if (name == QStringLiteral("arrow")) {
        return Tool::Arrow;
    }
    if (name == QStringLiteral("line")) {
        return Tool::Line;
    }
    if (name == QStringLiteral("wave")) {
        return Tool::Wave;
    }
    if (name == QStringLiteral("bezier")) {
        return Tool::Bezier;
    }
    if (name == QStringLiteral("pen")) {
        return Tool::Pen;
    }
    if (name == QStringLiteral("mosaic")) {
        return Tool::Mosaic;
    }
    if (name == QStringLiteral("text")) {
        return Tool::Text;
    }
    return Tool::Select;
}

QFont textFont(const QString &family, int pixelSize)
{
    QFont font = family.isEmpty() ? QApplication::font() : QFont(family);
    font.setPixelSize(std::max(1, pixelSize));
    return font;
}

QFont annotationFont(const Annotation &annotation)
{
    // The stored size is already the pixel height, so it goes straight in.
    return textFont(annotation.font, std::max(1, static_cast<int>(annotation.textPixels)));
}

QSize textMetrics(const Annotation &annotation)
{
    const QFont font = annotationFont(annotation);
    const QFontMetrics metrics(font);
    const QStringList lines = annotation.text.split(QLatin1Char('\n'));
    int width = 1;
    for (const QString &line : lines) {
        width = std::max(width, metrics.horizontalAdvance(line));
    }
    return QSize(width, std::max(1, static_cast<int>(lines.size()) * metrics.lineSpacing()));
}

// A numbered badge is sized from the stroke width rather than from a control of
// its own, so the width slider covers it too: six pen widths across, floored at
// 18 logical pixels so the thinnest pen still draws something legible and capped
// at 96 so the thickest one does not paint a billboard.  The standalone annotate
// surface spells the same two numbers out for itself: the two files share no
// code on purpose, and this is the price of that.
constexpr int kNumberMinDiameter = 18;
constexpr int kNumberMaxDiameter = 96;

int numberDiameter(int width)
{
    return std::clamp(width * 6, kNumberMinDiameter, kNumberMaxDiameter);
}

// The glyphs are a touch over half the badge, bold, so that a two-digit count
// still sits inside the disc.
int numberFontPixels(int diameter)
{
    return std::max(1, static_cast<int>(std::lround(diameter * 0.55)));
}

QFont numberFont(int diameter)
{
    QFont font = QApplication::font();
    font.setPixelSize(numberFontPixels(diameter));
    font.setBold(true);
    return font;
}

// The halo the bare-number style outlines its glyphs with, and the glyph colour
// that reads on top of a filled badge.  Both are chosen from the ink so the
// count stays legible whatever colour the pen is -- the palette carries a yellow
// and a white, and white-on-white would erase the count.
QColor numberHalo(const QColor &ink)
{
    return ink.lightness() > 140 ? QColor(0, 0, 0, 210) : QColor(255, 255, 255, 210);
}

QColor numberOnInk(const QColor &ink)
{
    return ink.lightness() > 160 ? QColor(20, 20, 20) : QColor(255, 255, 255);
}

constexpr qreal kNumberHaloWidth = 2.0;

// The one place a numbered badge is turned into ink.  The overlay's live
// preview, its cached per-mark raster and the bitmap the renderer is handed all
// draw through here, so the four styles cannot drift apart between them -- and
// the bitmap the Rust side composites is exactly what the user saw.
void paintNumberBadge(QPainter &painter, const QRectF &box, const QString &text, NumberStyle style,
                      const QColor &color, int width)
{
    const int diameter = std::max(1, static_cast<int>(std::lround(box.width())));
    painter.setFont(numberFont(diameter));
    switch (style) {
    case NumberStyle::FilledCircle:
        painter.setPen(Qt::NoPen);
        painter.setBrush(color);
        painter.drawEllipse(box);
        painter.setBrush(Qt::NoBrush);
        painter.setPen(numberOnInk(color));
        painter.drawText(box, Qt::AlignCenter, text);
        break;
    case NumberStyle::Ring: {
        const qreal pen = std::clamp(static_cast<qreal>(std::max(1, width)), 1.0,
                                     std::max(1.0, box.width() / 4.0));
        const qreal inset = pen / 2.0;
        painter.setBrush(Qt::NoBrush);
        painter.setPen(QPen(color, pen, Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
        painter.drawEllipse(box.adjusted(inset, inset, -inset, -inset));
        painter.setPen(color);
        painter.drawText(box, Qt::AlignCenter, text);
        break;
    }
    case NumberStyle::Square: {
        const qreal radius = box.width() * 0.22;
        painter.setPen(Qt::NoPen);
        painter.setBrush(color);
        painter.drawRoundedRect(box, radius, radius);
        painter.setBrush(Qt::NoBrush);
        painter.setPen(numberOnInk(color));
        painter.drawText(box, Qt::AlignCenter, text);
        break;
    }
    case NumberStyle::Plain: {
        const QFontMetricsF metrics(painter.font());
        const QRectF glyph = metrics.boundingRect(text);
        const QPointF origin(box.center().x() - glyph.width() / 2.0 - glyph.left(),
                             box.center().y() + metrics.capHeight() / 2.0);
        QPainterPath path;
        path.addText(origin, painter.font(), text);
        painter.setBrush(Qt::NoBrush);
        painter.setPen(QPen(numberHalo(color), kNumberHaloWidth, Qt::SolidLine, Qt::RoundCap,
                            Qt::RoundJoin));
        painter.drawPath(path);
        painter.setPen(Qt::NoPen);
        painter.setBrush(color);
        painter.drawPath(path);
        break;
    }
    }
}

// The badge style's name, for the style row's segment tooltips and the number
// tool's own tooltip.
QString numberStyleName(NumberStyle style)
{
    switch (style) {
    case NumberStyle::FilledCircle:
        return uiTr("Filled circle");
    case NumberStyle::Ring:
        return uiTr("Ring");
    case NumberStyle::Square:
        return uiTr("Square");
    case NumberStyle::Plain:
        return uiTr("Plain");
    }
    return QString();
}

// The tag the four number-style segments carry.  It never leaves this file: the
// segment row is a Qt-side control and nothing about it is serialized.
QString numberStyleValue(NumberStyle style)
{
    switch (style) {
    case NumberStyle::FilledCircle:
        return QStringLiteral("filled_circle");
    case NumberStyle::Ring:
        return QStringLiteral("ring");
    case NumberStyle::Square:
        return QStringLiteral("square");
    case NumberStyle::Plain:
        break;
    }
    return QStringLiteral("plain");
}

NumberStyle numberStyleForName(const QString &value)
{
    if (value == QStringLiteral("ring")) {
        return NumberStyle::Ring;
    }
    if (value == QStringLiteral("square")) {
        return NumberStyle::Square;
    }
    if (value == QStringLiteral("plain")) {
        return NumberStyle::Plain;
    }
    return NumberStyle::FilledCircle;
}

// Whether an annotation is the number tool's badge rather than a typed label.
// Both travel as text annotations with a bitmap, and the difference is the one
// name -- which is exactly why the renderer never has to know about it.
bool isNumberAnnotation(const Annotation &annotation)
{
    return annotation.kind == Annotation::Kind::Text &&
        annotation.tool == QStringLiteral("number");
}

// The point a badge is centred on, recovered from its own box.
Point numberCenter(const Annotation &annotation)
{
    return Point{annotation.rect.x + static_cast<std::int32_t>(annotation.rect.width / 2),
                 annotation.rect.y + static_cast<std::int32_t>(annotation.rect.height / 2)};
}

// Lays a badge's box out around `center` for the width it currently carries.
//
// The box is not decoration: the hit test, the drag clamp, the raster cache and
// the bitmap the renderer is handed are all sized from it, so it is re-derived
// wherever the width it comes from changes -- at placement, and when the width
// control restyles the badge under the selection.
void layoutNumberBox(Annotation &annotation, Point center)
{
    const int diameter = numberDiameter(static_cast<int>(annotation.width));
    const Point origin{center.x - diameter / 2, center.y - diameter / 2};
    annotation.origin = origin;
    annotation.rect = LogicalRect{origin.x, origin.y, static_cast<std::uint32_t>(diameter),
                                  static_cast<std::uint32_t>(diameter)};
    // The legacy glyph multiple the protocol derives from this is only ever read
    // by the renderer's no-bitmap fallback, so the count it means is the badge's
    // own diameter: the fallback then draws glyphs about as tall as the bitmap
    // the helper ships, rather than a label at some unrelated size.
    annotation.textPixels = static_cast<std::uint32_t>(diameter);
}

// The logical rect an annotation occupies, whatever its kind.  Shared by the
// hit test, the drag clamps and the render cache so all three agree on what a
// mark covers; `false` means there is nothing to draw or hit.
bool annotationLogicalBounds(const Annotation &annotation, LogicalRect *bounds)
{
    switch (annotation.kind) {
    case Annotation::Kind::Shape:
        *bounds = annotation.rect;
        return !bounds->isEmpty();
    case Annotation::Kind::Image:
        // The rect is where the pixels were placed, which is not the image's
        // own size: the paste fits it to the canvas and the handles resize it.
        *bounds = annotation.rect;
        return !annotation.pixels.isNull() && !bounds->isEmpty();
    case Annotation::Kind::Text: {
        // A numbered badge's box is the badge itself, recorded on the
        // annotation when it was placed.  The hit test, the drag clamp, the
        // raster cache and the paint all read it from here, so filling `rect`
        // at placement is what makes a badge selectable, draggable and
        // deletable rather than only visible.
        if (isNumberAnnotation(annotation)) {
            *bounds = annotation.rect;
            return !bounds->isEmpty();
        }
        if (annotation.text.isEmpty()) {
            return false;
        }
        const QSize metrics = textMetrics(annotation);
        *bounds = LogicalRect{annotation.origin.x, annotation.origin.y,
                              static_cast<std::uint32_t>(metrics.width()),
                              static_cast<std::uint32_t>(metrics.height())};
        return true;
    }
    case Annotation::Kind::Stroke:
        break;
    }
    if (annotation.points.isEmpty()) {
        return false;
    }
    if (annotation.tool == QStringLiteral("bezier")) {
        // A pen path is a cubic per segment and a cubic leaves the box of its
        // own anchors, so the box has to come from the sampled curve.  Taking
        // the control points instead would report a rect -- and so a raster --
        // far larger than the ink, and taking the anchors alone would clip the
        // bulge and leave a stale arc on screen.
        return polylineLogicalBounds(bezierPolyline(annotation.points, annotation.closed),
                                     bounds);
    }
    // The box of a stroke's own points.  For a wave those are its two ends and
    // the box is the segment between them; the crests that reach off it are
    // accounted for by the raster's padding (`StrokeRaster::padding`, read here
    // through `annotationReach`) rather than by this box -- the same split the
    // arrow's head already uses.
    std::int32_t minX = annotation.points.constFirst().x;
    std::int32_t maxX = minX;
    std::int32_t minY = annotation.points.constFirst().y;
    std::int32_t maxY = minY;
    for (const Point &point : annotation.points) {
        minX = std::min(minX, point.x);
        maxX = std::max(maxX, point.x);
        minY = std::min(minY, point.y);
        maxY = std::max(maxY, point.y);
    }
    bounds->x = minX;
    bounds->y = minY;
    bounds->width = static_cast<std::uint32_t>(static_cast<std::int64_t>(maxX) - minX + 1);
    bounds->height = static_cast<std::uint32_t>(static_cast<std::int64_t>(maxY) - minY + 1);
    return true;
}

QCursor cursorForHandle(int handle)
{
    switch (handle) {
    case 1:
    case 5:
        return Qt::SizeFDiagCursor;
    case 2:
    case 6:
        return Qt::SizeVerCursor;
    case 3:
    case 7:
        return Qt::SizeBDiagCursor;
    case 4:
    case 8:
        return Qt::SizeHorCursor;
    case 9:
        return Qt::SizeAllCursor;
    default:
        return Qt::CrossCursor;
    }
}
QIcon toolbarIcon(Tool tool, const QColor &color = QColor(230, 225, 229),
                  qreal devicePixelRatio = 1.0)
{
    const qreal ratio = std::max(1.0, devicePixelRatio);
    QPixmap pixmap(qRound(24 * ratio), qRound(24 * ratio));
    pixmap.setDevicePixelRatio(ratio);
    pixmap.fill(Qt::transparent);
    QPainter painter(&pixmap);
    painter.setRenderHint(QPainter::Antialiasing, true);
    painter.setPen(QPen(color, 2.0, Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
    painter.setBrush(Qt::NoBrush);
    switch (tool) {
    case Tool::Select:
        painter.setBrush(color);
        painter.drawPolygon(QPolygonF{QPointF(5, 3), QPointF(18, 14), QPointF(12, 15),
                                      QPointF(9, 21), QPointF(6, 19), QPointF(9, 14),
                                      QPointF(5, 3)});
        break;
    case Tool::Rectangle:
        painter.drawRoundedRect(QRectF(4, 5, 16, 14), 2, 2);
        break;
    case Tool::Ellipse:
        painter.drawEllipse(QRectF(4, 5, 16, 14));
        break;
    case Tool::Arrow:
        painter.drawLine(QPointF(4, 19), QPointF(18, 5));
        painter.drawLine(QPointF(11, 5), QPointF(18, 5));
        painter.drawLine(QPointF(18, 5), QPointF(18, 12));
        break;
    case Tool::Line:
        // The arrow's shaft without its head: a plain straight segment.
        painter.drawLine(QPointF(4, 19), QPointF(20, 5));
        break;
    case Tool::Wave: {
        QPainterPath wave;
        wave.moveTo(3.0, 12.0);
        for (int step = 1; step <= 18; ++step) {
            wave.lineTo(3.0 + step, 12.0 + std::sin(step * kTau / 9.0) * 5.0);
        }
        painter.drawPath(wave);
        break;
    }
    case Tool::Bezier: {
        // A curve with both its end anchors: the one icon in the row that says
        // "this one bends between the points you click".
        QPainterPath curve;
        curve.moveTo(4.0, 19.0);
        curve.cubicTo(4.0, 8.0, 20.0, 16.0, 20.0, 5.0);
        painter.drawPath(curve);
        painter.setPen(Qt::NoPen);
        painter.setBrush(color);
        painter.drawRect(QRectF(2.0, 17.0, 4.0, 4.0));
        painter.drawRect(QRectF(18.0, 3.0, 4.0, 4.0));
        painter.setBrush(Qt::NoBrush);
        break;
    }
    case Tool::Pen:
        painter.drawLine(QPointF(4, 17), QPointF(8, 12));
        painter.drawLine(QPointF(8, 12), QPointF(12, 15));
        painter.drawLine(QPointF(12, 15), QPointF(20, 7));
        break;
    case Tool::Text:
        painter.setFont(QFont(QStringLiteral("Sans"), 15, QFont::Bold));
        painter.drawText(QRectF(3, 2, 18, 20), Qt::AlignCenter, QStringLiteral("T"));
        break;
    case Tool::Number:
        // A miniature of the badge itself, drawn with the same painter as the
        // mark so the button and the click agree on the look.
        paintNumberBadge(painter, QRectF(4.0, 4.0, 16.0, 16.0), QStringLiteral("1"),
                         NumberStyle::FilledCircle, color, 2);
        break;
    case Tool::Mosaic:
        painter.setPen(Qt::NoPen);
        painter.setBrush(color);
        for (int row = 0; row < 3; ++row) {
            for (int column = 0; column < 3; ++column) {
                if ((row + column) % 2 == 0) {
                    painter.drawRoundedRect(QRectF(4 + column * 6, 4 + row * 6, 5, 5),
                                            1, 1);
                }
            }
        }
        break;
    }
    return QIcon(pixmap);
}

QIcon historyIcon(bool redo, const QColor &color = QColor(67, 72, 84),
                  qreal devicePixelRatio = 1.0)
{
    const qreal ratio = std::max(1.0, devicePixelRatio);
    QPixmap pixmap(qRound(24 * ratio), qRound(24 * ratio));
    pixmap.setDevicePixelRatio(ratio);
    pixmap.fill(Qt::transparent);
    QPainter painter(&pixmap);
    painter.setRenderHint(QPainter::Antialiasing, true);
    painter.setPen(QPen(color, 2.0, Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
    const QRectF arc(4.0, 4.0, 16.0, 16.0);
    painter.drawArc(arc, redo ? -45 * 16 : 45 * 16, 285 * 16);
    painter.setBrush(color);
    const QPolygonF arrow = redo
        ? QPolygonF{QPointF(18, 4), QPointF(20, 9), QPointF(15, 8)}
        : QPolygonF{QPointF(6, 4), QPointF(4, 9), QPointF(9, 8)};
    painter.drawPolygon(arrow);
    return QIcon(pixmap);
}
QFont pillFont()
{
    QFont font;
    font.setPixelSize(13);
    font.setBold(true);
    return font;
}

// The paste-image action's icon: a framed picture with a horizon and a sun,
// the shape everyone reads as "an image file".
QIcon pasteIcon(const QColor &color = QColor(230, 225, 229), qreal devicePixelRatio = 1.0)
{
    const qreal ratio = std::max(1.0, devicePixelRatio);
    QPixmap pixmap(qRound(24 * ratio), qRound(24 * ratio));
    pixmap.setDevicePixelRatio(ratio);
    pixmap.fill(Qt::transparent);
    QPainter painter(&pixmap);
    painter.setRenderHint(QPainter::Antialiasing, true);
    painter.setPen(QPen(color, 2.0, Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
    painter.setBrush(Qt::NoBrush);
    painter.drawRoundedRect(QRectF(3.5, 5.5, 17, 13), 2.5, 2.5);
    // The horizon and the peak, drawn as one polyline inside the frame.
    painter.drawPolyline(QPolygonF{QPointF(4, 16), QPointF(9, 11), QPointF(13, 15), QPointF(16, 12),
                                   QPointF(20, 16)});
    painter.setBrush(color);
    painter.setPen(Qt::NoPen);
    painter.drawEllipse(QPointF(8.5, 9.0), 1.4, 1.4);
    return QIcon(pixmap);
}

// The text-recognition action's icon: corner brackets around two text lines,
// which is what "read the text in this box" looks like.
QIcon recognizeTextIcon(const QColor &color = QColor(230, 225, 229),
                        qreal devicePixelRatio = 1.0)
{
    const qreal ratio = std::max(1.0, devicePixelRatio);
    QPixmap pixmap(qRound(24 * ratio), qRound(24 * ratio));
    pixmap.setDevicePixelRatio(ratio);
    pixmap.fill(Qt::transparent);
    QPainter painter(&pixmap);
    painter.setRenderHint(QPainter::Antialiasing, true);
    painter.setPen(QPen(color, 2.0, Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
    painter.setBrush(Qt::NoBrush);
    constexpr qreal inset = 3.5;
    constexpr qreal arm = 4.0;
    const qreal right = 24.0 - inset;
    const qreal bottom = 24.0 - inset;
    painter.drawPolyline(QPolygonF{QPointF(inset + arm, inset), QPointF(inset, inset),
                                   QPointF(inset, inset + arm)});
    painter.drawPolyline(QPolygonF{QPointF(right - arm, inset), QPointF(right, inset),
                                   QPointF(right, inset + arm)});
    painter.drawPolyline(QPolygonF{QPointF(inset, bottom - arm), QPointF(inset, bottom),
                                   QPointF(inset + arm, bottom)});
    painter.drawPolyline(QPolygonF{QPointF(right, bottom - arm), QPointF(right, bottom),
                                   QPointF(right - arm, bottom)});
    painter.drawLine(QPointF(8.5, 10.5), QPointF(15.5, 10.5));
    painter.drawLine(QPointF(8.5, 14.0), QPointF(13.0, 14.0));
    return QIcon(pixmap);
}

// The scrolling-capture action's icon: a downward arrow over stacked rows,
// which is what "scroll this and stitch what comes back" looks like.
QIcon scrollIcon(const QColor &color = QColor(230, 225, 229), qreal devicePixelRatio = 1.0)
{
    const qreal ratio = std::max(1.0, devicePixelRatio);
    QPixmap pixmap(qRound(24 * ratio), qRound(24 * ratio));
    pixmap.setDevicePixelRatio(ratio);
    pixmap.fill(Qt::transparent);
    QPainter painter(&pixmap);
    painter.setRenderHint(QPainter::Antialiasing, true);
    painter.setPen(QPen(color, 2.0, Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
    painter.setBrush(Qt::NoBrush);
    // The arrow points down at the rows the scroll will stack underneath.
    painter.drawLine(QPointF(12.0, 3.5), QPointF(12.0, 13.5));
    painter.drawPolyline(QPolygonF{QPointF(8.0, 9.5), QPointF(12.0, 13.5), QPointF(16.0, 9.5)});
    // The stacked rows below: the tall image the stitch builds.
    painter.drawLine(QPointF(6.0, 17.0), QPointF(18.0, 17.0));
    painter.drawLine(QPointF(6.0, 20.5), QPointF(18.0, 20.5));
    return QIcon(pixmap);
}

// Draws a dark rounded label (dimensions, pixel coordinates) anchored at `anchor`
// inside `bounds`; flips above the anchor when there is no room below.
void drawInfoPill(QPainter *painter, const QPointF &anchor, const QString &text,
                  const QRectF &bounds)
{
    const QFontMetrics metrics(pillFont());
    const int textWidth = metrics.horizontalAdvance(text);
    const int pillWidth = textWidth + 16;
    const int pillHeight = metrics.height() + 8;
    qreal x = anchor.x() - pillWidth / 2.0;
    x = std::clamp(x, bounds.left() + 2.0, std::max(bounds.left() + 2.0, bounds.right() - pillWidth - 2.0));
    qreal y = anchor.y() + 10.0;
    if (y + pillHeight > bounds.bottom()) {
        y = anchor.y() - pillHeight - 10.0;
    }
    y = std::clamp(y, bounds.top() + 2.0, std::max(bounds.top() + 2.0, bounds.bottom() - pillHeight - 2.0));
    const QRectF pill(x, y, pillWidth, pillHeight);
    painter->setFont(pillFont());
    painter->setPen(Qt::NoPen);
    painter->setBrush(QColor(20, 20, 20, 225));
    painter->drawRoundedRect(pill, 4, 4);
    painter->setPen(QPen(QColor(120, 120, 120), 1.0));
    painter->drawRoundedRect(pill.adjusted(0.5, 0.5, -0.5, -0.5), 4, 4);
    painter->setPen(Qt::white);
    painter->drawText(pill, Qt::AlignCenter, text);
}

// Mosaic strength levels: block size in device pixels for a given output
// scale (P1 fine, P2 standard, P3 coarse) — mirrors edit::mosaic_block_size.
int mosaicBlockForStrength(std::uint32_t strength, int scale)
{
    const int base = std::max(1, 12 * scale);
    switch (strength) {
    case 1:
        return std::max(4, base / 2);
    case 3:
        return base * 2;
    default:
        return base;
    }
}

// Mosaic strength for the freehand brush: smear radius factor — mirrors
// edit::mosaic_brush_radius.
int brushRadiusForStrength(std::uint32_t strength, int radius)
{
    switch (strength) {
    case 1:
        return std::max(1, radius / 2);
    case 3:
        return radius * 2;
    default:
        return radius;
    }
}

// Builds the pen for an annotation, translating the wire line style into a
// dash pattern that approximates the Rust renderer (dashes of 3w with 2w gaps,
// width-wide dots with 2w gaps, in device pixels).
QPen penForAnnotation(const Annotation &annotation)
{
    const bool solid = annotation.dash == QStringLiteral("solid");
    QPen pen(annotation.color, static_cast<double>(annotation.width), Qt::SolidLine,
             solid ? Qt::RoundCap : Qt::FlatCap, Qt::RoundJoin);
    if (annotation.dash == QStringLiteral("dashed")) {
        pen.setDashPattern({3.0, 2.0});
    } else if (annotation.dash == QStringLiteral("dotted")) {
        pen.setDashPattern({0.001, 2.0});
        pen.setCapStyle(Qt::RoundCap);
    }
    return pen;
}

// The pen a wave is drawn with: always solid, whatever the style says.  The
// Rust renderer's wave operation carries no dash -- it samples a solid sine --
// so a dashed wave here would preview one thing and bake another.
QPen wavePen(const Annotation &annotation)
{
    return QPen(annotation.color, static_cast<double>(annotation.width), Qt::SolidLine,
                Qt::RoundCap, Qt::RoundJoin);
}

// Dashed rectangles are drawn along the stroke band centerline in the final
// renderer, so the preview walks the same inset path instead of drawRect.
QPolygonF insetRectPolygon(const QRectF &rect, double width)
{
    const double inset = std::max(0.0, (width - 1.0) / 2.0);
    const double outer = std::max(0.0, width / 2.0);
    const double left = rect.left() + inset;
    const double top = rect.top() + inset;
    const double right = std::max(left + 0.5, rect.right() - outer);
    const double bottom = std::max(top + 0.5, rect.bottom() - outer);
    QPolygonF polygon;
    polygon << QPointF(left, top) << QPointF(right, top) << QPointF(right, bottom)
            << QPointF(left, bottom) << QPointF(left, top);
    return polygon;
}

// Computes the average color of one device-pixel block, mirroring the Rust
// block averaging (4x4 subsampling, round-half-up per channel).
QColor averageBlockColor(const uchar *bits, qsizetype bytesPerLine,
                         int x, int y, int width, int height)
{
    std::int64_t sums[4] = {0, 0, 0, 0};
    std::int64_t count = 0;
    const int stepX = std::max(1, width / 4);
    const int stepY = std::max(1, height / 4);
    for (int yy = 0; yy < height; yy += stepY) {
        for (int xx = 0; xx < width; xx += stepX) {
            const uchar *pixel = bits + static_cast<qsizetype>(y + yy) * bytesPerLine +
                static_cast<qsizetype>(x + xx) * 4;
            for (int channel = 0; channel < 4; ++channel) {
                sums[channel] += pixel[channel];
            }
            ++count;
        }
    }
    if (count == 0) {
        return QColor(0, 0, 0);
    }
    return QColor(static_cast<int>(sums[0] / count), static_cast<int>(sums[1] / count),
                  static_cast<int>(sums[2] / count), static_cast<int>(sums[3] / count));
}

// Blits one solid block straight into the destination image when the painter is
// drawing onto an ARGB32-premultiplied QImage under a pure scale-and-translate
// transform with no clip.  `fillLogicalBlock` runs once per mosaic block, and a
// 1080p area mosaic at the standard 12-device-pixel block is about 14k calls,
// each of which the QPainter path pays a call and a clip test for.  The direct
// write reproduces `QPainter::fillRect(QRectF, color)` exactly: the logical rect
// is mapped through the painter transform and aligned outward to whole device
// pixels (the raster engine's fill for a solid, unantialiased colour), and a
// fully opaque colour written over the destination is the same as a source-over
// fill of it.  Anything else -- a translucent block, a widget or pixmap target,
// a rotation, an active clip -- returns false and lets the caller fall back.
bool fillRectDirect(QPainter *painter, const QRectF &logical, const QColor &color)
{
    if (color.alpha() != 255 || painter->hasClipping()) {
        return false;
    }
    QPaintDevice *device = painter->device();
    if (device == nullptr || device->devType() != QInternal::Image) {
        return false;
    }
    QImage *image = static_cast<QImage *>(device);
    if (image->format() != QImage::Format_ARGB32_Premultiplied) {
        return false;
    }
    const QTransform transform = painter->combinedTransform();
    if (transform.type() > QTransform::TxScale) {
        return false;
    }
    // The raster engine antialiases a rect whose edges land between device
    // pixels, so only an exactly pixel-aligned block may be written directly;
    // anything fractional (a scaled painter, a non-integer device ratio) falls
    // back to QPainter.
    const QRectF mapped = transform.mapRect(logical);
    const QRect deviceRect = mapped.toAlignedRect();
    if (mapped != QRectF(deviceRect)) {
        return false;
    }
    const QRect clipped = deviceRect.intersected(QRect(0, 0, image->width(), image->height()));
    if (clipped.isEmpty()) {
        return true;
    }
    const QRgb value = qRgba(color.red(), color.green(), color.blue(), 255);
    for (int row = clipped.top(); row <= clipped.bottom(); ++row) {
        QRgb *line = reinterpret_cast<QRgb *>(image->scanLine(row)) + clipped.left();
        for (int col = 0; col < clipped.width(); ++col) {
            line[col] = value;
        }
    }
    return true;
}

void fillLogicalBlock(QPainter *painter, const OutputSession &output, const LogicalRect &bounds,
                      const QSize &size, const QRect &source, int scale, int x, int y, int width,
                      int height, const QColor &color)
{
    LogicalRect blockLogical;
    blockLogical.x = bounds.x + static_cast<std::int32_t>((x - source.x()) / scale);
    blockLogical.y = bounds.y + static_cast<std::int32_t>((y - source.y()) / scale);
    blockLogical.width = static_cast<std::uint32_t>(width / scale);
    blockLogical.height = static_cast<std::uint32_t>(height / scale);
    const QRectF logical = localRect(output, blockLogical, size);
    if (fillRectDirect(painter, logical, color)) {
        return;
    }
    painter->fillRect(logical, color);
}

// Renders a real pixelation mosaic over the annotation bounds, matching the
// Rust renderer: blocks of 12 * scale device pixels averaged independently and
// aligned to the bounds origin.  `mask` picks a rectangular or elliptical area;
// ellipse boundary blocks are averaged and filled per pixel so the edge stays
// as smooth as the final render.
void drawMosaicAnnotation(QPainter *painter, const OutputSession &output,
                          const LogicalRect &bounds, const QString &mask,
                          std::uint32_t strength, const QSize &size)
{
    const QImage &image = output.image;
    if (image.isNull() || bounds.isEmpty()) {
        return;
    }
    const QRect source = sourceRect(output, bounds);
    const QRect clipped = source.intersected(QRect(0, 0, image.width(), image.height()));
    if (clipped.isEmpty()) {
        return;
    }
    const int scale = static_cast<int>(output.scale > 0 ? output.scale : 1);
    const int block = mosaicBlockForStrength(strength, scale);
    const uchar *bits = image.constBits();
    const qsizetype bytesPerLine = image.bytesPerLine();
    const bool ellipse = mask == QStringLiteral("ellipse");
    // Mirrors the Rust integer midline ellipse (center = left + size/2).
    const std::int64_t left = source.x();
    const std::int64_t top = source.y();
    const std::int64_t rectWidth = source.width();
    const std::int64_t rectHeight = source.height();
    const std::int64_t centerX = left + rectWidth / 2;
    const std::int64_t centerY = top + rectHeight / 2;
    const std::int64_t a = std::max<std::int64_t>(2, rectWidth) / 2;
    const std::int64_t b = std::max<std::int64_t>(2, rectHeight) / 2;
    const std::int64_t aSquared = a * a;
    const std::int64_t bSquared = b * b;
    const std::int64_t threshold = aSquared * bSquared;
    auto insideEllipse = [&](std::int64_t px, std::int64_t py) {
        const std::int64_t dx = px - centerX;
        const std::int64_t dy = py - centerY;
        return dx * dx * bSquared + dy * dy * aSquared <= threshold;
    };
    for (int y = clipped.top(); y <= clipped.bottom(); y += block) {
        for (int x = clipped.left(); x <= clipped.right(); x += block) {
            const int width = std::min(block, clipped.right() - x + 1);
            const int height = std::min(block, clipped.bottom() - y + 1);
            if (!ellipse) {
                fillLogicalBlock(painter, output, bounds, size, source, scale, x, y, width,
                                 height,
                                 averageBlockColor(bits, bytesPerLine, x, y, width,
                                                   height));
                continue;
            }
            // The ellipse interior is convex: a block whose corners are all
            // inside is fully inside and skips the per-pixel pass.
            const bool fullyInside = insideEllipse(x, y) && insideEllipse(x + width - 1, y) &&
                insideEllipse(x, y + height - 1) && insideEllipse(x + width - 1, y + height - 1);
            if (fullyInside) {
                fillLogicalBlock(painter, output, bounds, size, source, scale, x, y, width,
                                 height,
                                 averageBlockColor(bits, bytesPerLine, x, y, width,
                                                   height));
                continue;
            }
            std::int64_t sums[4] = {0, 0, 0, 0};
            std::int64_t count = 0;
            for (int yy = 0; yy < height; ++yy) {
                for (int xx = 0; xx < width; ++xx) {
                    if (!insideEllipse(x + xx, y + yy)) {
                        continue;
                    }
                    const uchar *pixel = bits + static_cast<qsizetype>(y + yy) * bytesPerLine +
                        static_cast<qsizetype>(x + xx) * 4;
                    for (int channel = 0; channel < 4; ++channel) {
                        sums[channel] += pixel[channel];
                    }
                    ++count;
                }
            }
            if (count == 0) {
                continue;
            }
            const QColor average(static_cast<int>(sums[0] / count),
                                 static_cast<int>(sums[1] / count),
                                 static_cast<int>(sums[2] / count),
                                 static_cast<int>(sums[3] / count));
            for (int yy = 0; yy < height; ++yy) {
                for (int xx = 0; xx < width; ++xx) {
                    if (!insideEllipse(x + xx, y + yy)) {
                        continue;
                    }
                    fillLogicalBlock(painter, output, bounds, size, source, scale, x + xx,
                                     y + yy, scale, scale, average);
                }
            }
        }
    }
}

// Appends the device-space stamp centers of one segment, plus the segment's own
// start when it is the first one, to `centers`.  Each segment's stamps depend
// only on its two endpoints, which is what lets a growing freehand stroke stamp
// a segment once and never touch it again.
void appendMosaicCenters(QVector<QPointF> &centers, const OutputSession &output,
                         const Point &first, const Point &second, bool includeFirst, double scale,
                         double step)
{
    const QPointF a((first.x - output.geometry.x) * scale, (first.y - output.geometry.y) * scale);
    const QPointF b((second.x - output.geometry.x) * scale,
                    (second.y - output.geometry.y) * scale);
    if (includeFirst) {
        centers.append(a);
    }
    const double length = std::hypot(b.x() - a.x(), b.y() - a.y());
    const int count = std::max(1, static_cast<int>(std::ceil(length / step)));
    for (int k = 1; k <= count; ++k) {
        const double t = static_cast<double>(k) / count;
        centers.append(a + (b - a) * t);
    }
}

// Averages the source image under each stamp center and paints the disc.  The
// whole-path brush and the incremental one share this so both smear alike.
void stampMosaicDiscs(QPainter *painter, const OutputSession &output,
                      const QVector<QPointF> &centers, int radius, double scale, const QSize &size)
{
    const QImage &image = output.image;
    if (image.isNull() || centers.isEmpty()) {
        return;
    }
    const uchar *bits = image.constBits();
    const qsizetype bytesPerLine = image.bytesPerLine();
    const double radiusSquared = static_cast<double>(radius) * radius;
    const bool antialiased = painter->testRenderHint(QPainter::Antialiasing);
    painter->setRenderHint(QPainter::Antialiasing, false);
    for (const QPointF &center : centers) {
        std::int64_t sums[4] = {0, 0, 0, 0};
        std::int64_t count = 0;
        const int centerX = static_cast<int>(std::lround(center.x()));
        const int centerY = static_cast<int>(std::lround(center.y()));
        for (int dy = -radius; dy <= radius; ++dy) {
            for (int dx = -radius; dx <= radius; ++dx) {
                if (static_cast<double>(dx * dx + dy * dy) > radiusSquared) {
                    continue;
                }
                const int px = centerX + dx;
                const int py = centerY + dy;
                if (px < 0 || py < 0 || px >= image.width() || py >= image.height()) {
                    continue;
                }
                const uchar *pixel = bits + static_cast<qsizetype>(py) * bytesPerLine +
                    static_cast<qsizetype>(px) * 4;
                for (int channel = 0; channel < 4; ++channel) {
                    sums[channel] += pixel[channel];
                }
                ++count;
            }
        }
        if (count == 0) {
            continue;
        }
        const QColor average(static_cast<int>(sums[0] / count), static_cast<int>(sums[1] / count),
                             static_cast<int>(sums[2] / count),
                             static_cast<int>(sums[3] / count));
        const Point global{static_cast<std::int32_t>(std::lround(output.geometry.x +
                                                                 centerX / scale)),
                           static_cast<std::int32_t>(std::lround(output.geometry.y +
                                                                 centerY / scale))};
        const QPointF local = localPoint(output, global, size);
        painter->setPen(Qt::NoPen);
        painter->setBrush(average);
        painter->drawEllipse(local, radius / scale, radius / scale);
    }
    painter->setRenderHint(QPainter::Antialiasing, antialiased);
}

// Stamps one segment of the brush, the unit the incremental freehand raster
// bakes one at a time.
void stampMosaicSegment(QPainter *painter, const OutputSession &output, const Point &first,
                        const Point &second, bool includeFirst, int radius, double scale,
                        double step, const QSize &size)
{
    QVector<QPointF> centers;
    appendMosaicCenters(centers, output, first, second, includeFirst, scale, step);
    stampMosaicDiscs(painter, output, centers, radius, scale, size);
}

// Smears mosaic discs along the path, mirroring Frame::mosaic_brush: device
// space, radius = width/2, stamps every radius/2 pixels, each stamp averaged
// from the pristine source image.
void drawMosaicBrush(QPainter *painter, const OutputSession &output, const QVector<Point> &points,
                     std::uint32_t widthLogical, std::uint32_t strength, const QSize &size)
{
    if (output.image.isNull() || points.isEmpty()) {
        return;
    }
    const std::uint32_t scaleValue = output.scale > 0 ? output.scale : 1;
    const double scale = static_cast<double>(scaleValue);
    const int baseRadius = std::clamp(static_cast<int>(widthLogical * scale / 2.0), 1, 512);
    const int radius = std::clamp(brushRadiusForStrength(strength, baseRadius), 1, 512);
    // The same spacing the live preview uses (`paintLiveStroke`), so a previewed
    // mosaic brush stamps the same discs as the mark it commits.
    const double step = std::max(1.0, radius / 2.0);
    QVector<QPointF> centers;
    centers.reserve(points.size() + 8);
    if (points.size() == 1) {
        centers.append(QPointF((points.constFirst().x - output.geometry.x) * scale,
                               (points.constFirst().y - output.geometry.y) * scale));
    } else {
        for (int index = 0; index + 1 < points.size(); ++index) {
            appendMosaicCenters(centers, output, points.at(index), points.at(index + 1),
                                index == 0, scale, step);
        }
    }
    stampMosaicDiscs(painter, output, centers, radius, scale, size);
}

// Superellipse (squircle) outline path, matching the ProcessManager cards
// (corner radius with exponent 5 instead of a plain circular corner).
QPainterPath superellipsePathCorners(const QRectF &bounds, qreal topLeft, qreal topRight,
                                     qreal bottomRight, qreal bottomLeft, qreal exponent)
{
    QPainterPath path;
    const qreal maxRadius = std::min(bounds.width(), bounds.height()) / 2.0;
    topLeft = std::clamp(topLeft, 0.0, maxRadius);
    topRight = std::clamp(topRight, 0.0, maxRadius);
    bottomRight = std::clamp(bottomRight, 0.0, maxRadius);
    bottomLeft = std::clamp(bottomLeft, 0.0, maxRadius);
    const int steps = 12;
    bool first = true;
    // Each corner quadrant runs from an edge midside point to the next; the
    // straight edges close the gaps between consecutive quadrants.
    const auto corner = [&](qreal radius, qreal centerX, qreal centerY, qreal startDegrees,
                            qreal endDegrees) {
        if (radius <= 0.0) {
            if (first) {
                path.moveTo(centerX, centerY);
                first = false;
            } else {
                path.lineTo(centerX, centerY);
            }
            return;
        }
        for (int i = 0; i <= steps; ++i) {
            const qreal angle = qDegreesToRadians(startDegrees +
                                                  (endDegrees - startDegrees) * i / steps);
            const qreal cosine = std::cos(angle);
            const qreal sine = std::sin(angle);
            const qreal x = centerX + std::copysign(std::pow(std::abs(cosine), 2.0 / exponent),
                                                    cosine) * radius;
            const qreal y = centerY + std::copysign(std::pow(std::abs(sine), 2.0 / exponent),
                                                    sine) * radius;
            if (first) {
                path.moveTo(x, y);
                first = false;
            } else {
                path.lineTo(x, y);
            }
        }
    };
    corner(topLeft, bounds.left() + topLeft, bounds.top() + topLeft, 180.0, 270.0);
    corner(topRight, bounds.right() - topRight, bounds.top() + topRight, 270.0, 360.0);
    corner(bottomRight, bounds.right() - bottomRight, bounds.bottom() - bottomRight, 0.0, 90.0);
    corner(bottomLeft, bounds.left() + bottomLeft, bounds.bottom() - bottomLeft, 90.0, 180.0);
    path.closeSubpath();
    return path;
}

QPainterPath superellipsePath(const QRectF &bounds, qreal radius, qreal exponent)
{
    radius = std::clamp(radius, 0.0, std::min(bounds.width(), bounds.height()) / 2.0);
    return superellipsePathCorners(bounds, radius, radius, radius, radius, exponent);
}

// One separable 3x3 box-blur pass; averaging premultiplied channels stays valid.
QImage boxBlurImage(const QImage &source)
{
    if (source.isNull()) {
        return {};
    }
    const QImage input = source.convertToFormat(QImage::Format_ARGB32_Premultiplied);
    const int width = input.width();
    const int height = input.height();
    QImage horizontal(width, height, QImage::Format_ARGB32_Premultiplied);
    for (int y = 0; y < height; ++y) {
        const QRgb *row = reinterpret_cast<const QRgb *>(input.constScanLine(y));
        QRgb *out = reinterpret_cast<QRgb *>(horizontal.scanLine(y));
        for (int x = 0; x < width; ++x) {
            const QRgb left = row[std::max(0, x - 1)];
            const QRgb center = row[x];
            const QRgb right = row[std::min(width - 1, x + 1)];
            out[x] = qRgba((qRed(left) + qRed(center) + qRed(right)) / 3,
                           (qGreen(left) + qGreen(center) + qGreen(right)) / 3,
                           (qBlue(left) + qBlue(center) + qBlue(right)) / 3,
                           (qAlpha(left) + qAlpha(center) + qAlpha(right)) / 3);
        }
    }
    QImage result(width, height, QImage::Format_ARGB32_Premultiplied);
    for (int y = 0; y < height; ++y) {
        const QRgb *up =
            reinterpret_cast<const QRgb *>(horizontal.constScanLine(std::max(0, y - 1)));
        const QRgb *middle =
            reinterpret_cast<const QRgb *>(horizontal.constScanLine(y));
        const QRgb *down = reinterpret_cast<const QRgb *>(
            horizontal.constScanLine(std::min(height - 1, y + 1)));
        QRgb *out = reinterpret_cast<QRgb *>(result.scanLine(y));
        for (int x = 0; x < width; ++x) {
            out[x] = qRgba((qRed(up[x]) + qRed(middle[x]) + qRed(down[x])) / 3,
                           (qGreen(up[x]) + qGreen(middle[x]) + qGreen(down[x])) / 3,
                           (qBlue(up[x]) + qBlue(middle[x]) + qBlue(down[x])) / 3,
                           (qAlpha(up[x]) + qAlpha(middle[x]) + qAlpha(down[x])) / 3);
        }
    }
    return result;
}

// Frosted backdrop: strong downscale + box blur of the frozen frame region
// behind the panel, upscaled with a smooth transform at draw time.
QImage frostedBackdrop(const QImage &frame, const QRect &deviceRect)
{
    if (frame.isNull() || deviceRect.isEmpty()) {
        return {};
    }
    const QImage region = frame.copy(deviceRect);
    const QSize smallSize(std::max(1, region.width() / 14), std::max(1, region.height() / 14));
    const QImage small =
        region.scaled(smallSize, Qt::IgnoreAspectRatio, Qt::SmoothTransformation);
    return boxBlurImage(small);
}

// Squircle color chip for the swatch row: QSS backgrounds only produce
// circular corners, so the chip and its selection ring are painted.
QIcon swatchIcon(const QColor &color, bool selected, qreal devicePixelRatio)
{
    const qreal ratio = std::max(1.0, devicePixelRatio);
    QPixmap pixmap(qRound(20 * ratio), qRound(20 * ratio));
    pixmap.setDevicePixelRatio(ratio);
    pixmap.fill(Qt::transparent);
    QPainter painter(&pixmap);
    painter.setRenderHint(QPainter::Antialiasing, true);
    painter.setPen(Qt::NoPen);
    const QPainterPath chip = superellipsePath(QRectF(0.5, 0.5, 19.0, 19.0), 7.0, 5.0);
    // A translucent chip shows a checkerboard through it, the way the settings
    // window's color button does, so "this swatch carries alpha" is visible at a
    // glance rather than reading as a slightly darker opaque colour.
    if (color.alpha() < 255) {
        painter.save();
        painter.setClipPath(chip);
        painter.fillRect(QRectF(0.0, 0.0, 20.0, 20.0), QColor(0x6f, 0x76, 0x80));
        painter.fillRect(QRectF(0.0, 0.0, 10.0, 10.0), QColor(0x9a, 0xa3, 0xae));
        painter.fillRect(QRectF(10.0, 10.0, 10.0, 10.0), QColor(0x9a, 0xa3, 0xae));
        painter.restore();
    }
    painter.setBrush(color);
    painter.drawPath(chip);
    painter.setBrush(Qt::NoBrush);
    painter.setPen(selected ? QPen(QColor(233, 236, 255), 2.0)
                            : QPen(QColor(86, 93, 104), 1.0));
    painter.drawPath(superellipsePath(QRectF(1.5, 1.5, 17.0, 17.0), 6.0, 5.0));
    return QIcon(pixmap);
}

// Rainbow squircle for the custom color swatch.
QIcon colorPickerIcon(bool selected, qreal devicePixelRatio)
{
    const qreal ratio = std::max(1.0, devicePixelRatio);
    QPixmap pixmap(qRound(20 * ratio), qRound(20 * ratio));
    pixmap.setDevicePixelRatio(ratio);
    pixmap.fill(Qt::transparent);
    QPainter painter(&pixmap);
    painter.setRenderHint(QPainter::Antialiasing, true);
    QConicalGradient gradient(10.0, 10.0, 90.0);
    for (int index = 0; index <= 6; ++index) {
        gradient.setColorAt(index / 6.0,
                            QColor::fromHsvF(index == 6 ? 0.0 : index / 6.0, 0.75, 1.0));
    }
    painter.setPen(Qt::NoPen);
    painter.setBrush(gradient);
    painter.drawPath(superellipsePath(QRectF(0.5, 0.5, 19.0, 19.0), 7.0, 5.0));
    painter.setBrush(Qt::NoBrush);
    painter.setPen(selected ? QPen(QColor(233, 236, 255), 2.0)
                            : QPen(QColor(86, 93, 104), 1.0));
    painter.drawPath(superellipsePath(QRectF(1.5, 1.5, 17.0, 17.0), 6.0, 5.0));
    return QIcon(pixmap);
}

// Frame for a segmented control (Solid/Dash/Dot, Open V/Filled, Rect/Ellip/Brush).
// The outline, separators, active and hover cards are painted here because
// QSS borders with partial per-button radii render broken (disconnected
// border segments around the rounded ends).
class SegmentFrame final : public QWidget {
public:
    explicit SegmentFrame(QWidget *parent)
        : QWidget(parent)
    {
        setAttribute(Qt::WA_StyledBackground, false);
    }

    void trackButton(QPushButton *button)
    {
        button->installEventFilter(this);
        buttons_.push_back(button);
    }

protected:
    bool eventFilter(QObject *watched, QEvent *event) override
    {
        if (event->type() == QEvent::Enter || event->type() == QEvent::Leave) {
            hovered_ = event->type() == QEvent::Enter ? static_cast<QWidget *>(watched) : nullptr;
            update();
        }
        return QWidget::eventFilter(watched, event);
    }

    void paintEvent(QPaintEvent *event) override
    {
        Q_UNUSED(event);
        QPainter painter(this);
        painter.setRenderHint(QPainter::Antialiasing, true);
        constexpr qreal radius = 8.0;
        constexpr qreal cardRadius = 9.0;
        constexpr qreal exponent = 5.0;
        const auto ends = [](const QWidget *button) {
            const QString position = button->property("segmentPosition").toString();
            return std::pair<bool, bool>{position == QStringLiteral("first"),
                                         position == QStringLiteral("last")};
        };
        const auto cardPath = [&ends](QWidget *button, qreal expand) {
            const auto [leftEnd, rightEnd] = ends(button);
            return superellipsePathCorners(
                QRectF(button->geometry()).adjusted(-expand, -expand, expand, expand),
                leftEnd ? cardRadius : 0.0, rightEnd ? cardRadius : 0.0,
                rightEnd ? cardRadius : 0.0, leftEnd ? cardRadius : 0.0, exponent);
        };
        // Inactive hover card, kept inside the frame.
        painter.setPen(Qt::NoPen);
        for (QWidget *button : buttons_) {
            if (!button->isVisibleTo(this) || button->property("active").toBool() ||
                hovered_ != button) {
                continue;
            }
            painter.setBrush(QColor(44, 50, 59));
            painter.drawPath(cardPath(button, 0.0));
        }
        QRect outline;
        for (QWidget *button : buttons_) {
            if (button->isVisibleTo(this)) {
                outline = outline.isNull() ? button->geometry()
                                           : outline.united(button->geometry());
            }
        }
        if (!outline.isNull()) {
            painter.setBrush(Qt::NoBrush);
            painter.setPen(QPen(QColor(86, 93, 104), 1.0));
            painter.drawPath(superellipsePathCorners(
                QRectF(outline).adjusted(0.5, 0.5, -0.5, -0.5), radius, radius, radius,
                radius, exponent));
            for (int index = 0; index + 1 < buttons_.size(); ++index) {
                QWidget *left = buttons_.at(index);
                QWidget *right = buttons_.at(index + 1);
                if (!left->isVisibleTo(this) || !right->isVisibleTo(this)) {
                    continue;
                }
                const int x = right->geometry().left();
                painter.drawLine(x, 5, x, height() - 6);
            }
        }
        // The active card paints last, expanded by 1px so it covers the frame
        // and the adjacent separators: a selected segment reads as a card on
        // top of the control instead of a dark-rimmed cell.
        painter.setPen(Qt::NoPen);
        painter.setBrush(QColor(221, 225, 255));
        for (QWidget *button : buttons_) {
            if (button->isVisibleTo(this) && button->property("active").toBool()) {
                painter.drawPath(cardPath(button, 1.0));
            }
        }
    }

private:
    QVector<QPushButton *> buttons_;
    QWidget *hovered_ = nullptr;
};

// Saturation/value square for a fixed hue.
class SatValPane final : public QWidget {
public:
    using Picked = std::function<void(qreal, qreal)>;

    SatValPane(Picked picked, QWidget *parent)
        : QWidget(parent)
        , picked_(std::move(picked))
    {
        setFixedSize(184, 116);
        setCursor(Qt::CrossCursor);
    }

    void setHue(qreal hue) { hue_ = hue; update(); }
    void setSatVal(qreal sat, qreal val) { sat_ = sat; val_ = val; update(); }

protected:
    void paintEvent(QPaintEvent *event) override
    {
        Q_UNUSED(event);
        QPainter painter(this);
        painter.setRenderHint(QPainter::Antialiasing, true);
        painter.setClipPath(superellipsePathCorners(QRectF(rect()), 6.0, 6.0, 6.0, 6.0, 5.0));
        QLinearGradient across(0, 0, width(), 0);
        across.setColorAt(0.0, QColor(255, 255, 255));
        across.setColorAt(1.0, QColor::fromHsvF(hue_, 1.0, 1.0));
        painter.fillRect(rect(), across);
        QLinearGradient down(0, 0, 0, height());
        down.setColorAt(0.0, QColor(0, 0, 0, 0));
        down.setColorAt(1.0, QColor(0, 0, 0));
        painter.fillRect(rect(), down);
        painter.setClipping(false);
        const QPointF marker(std::clamp(sat_ * (width() - 1), 6.0, width() - 7.0),
                             std::clamp((1.0 - val_) * (height() - 1), 6.0, height() - 7.0));
        painter.setPen(QPen(QColor(0, 0, 0, 160), 4.0));
        painter.setBrush(Qt::NoBrush);
        painter.drawEllipse(marker, 5.0, 5.0);
        painter.setPen(QPen(Qt::white, 2.0));
        painter.drawEllipse(marker, 5.0, 5.0);
    }

    void mousePressEvent(QMouseEvent *event) override { pick(event->position()); }

    void mouseMoveEvent(QMouseEvent *event) override
    {
        if (event->buttons() & Qt::LeftButton) {
            pick(event->position());
        }
    }

private:
    void pick(const QPointF &position)
    {
        sat_ = std::clamp(position.x() / std::max(1, width() - 1), 0.0, 1.0);
        val_ = std::clamp(1.0 - position.y() / std::max(1, height() - 1), 0.0, 1.0);
        update();
        if (picked_) {
            picked_(sat_, val_);
        }
    }

    Picked picked_;
    qreal hue_ = 0.0;
    qreal sat_ = 1.0;
    qreal val_ = 1.0;
};

// Horizontal hue strip.
class HuePane final : public QWidget {
public:
    using Picked = std::function<void(qreal)>;

    HuePane(Picked picked, QWidget *parent)
        : QWidget(parent)
        , picked_(std::move(picked))
    {
        setFixedSize(184, 14);
        setCursor(Qt::CrossCursor);
    }

    void setHue(qreal hue) { hue_ = hue; update(); }

protected:
    void paintEvent(QPaintEvent *event) override
    {
        Q_UNUSED(event);
        QPainter painter(this);
        painter.setRenderHint(QPainter::Antialiasing, true);
        painter.setClipPath(superellipsePathCorners(QRectF(rect()), 7.0, 7.0, 7.0, 7.0, 5.0));
        QLinearGradient across(0, 0, width(), 0);
        for (int index = 0; index <= 6; ++index) {
            across.setColorAt(index / 6.0, QColor::fromHsvF(index == 6 ? 0.0 : index / 6.0, 1.0, 1.0));
        }
        painter.fillRect(rect(), across);
        painter.setClipping(false);
        const QPointF marker(std::clamp(hue_ * (width() - 1), 6.0, width() - 7.0),
                             height() / 2.0);
        painter.setPen(QPen(QColor(0, 0, 0, 160), 4.0));
        painter.setBrush(Qt::NoBrush);
        painter.drawEllipse(marker, 5.0, 5.0);
        painter.setPen(QPen(Qt::white, 2.0));
        painter.drawEllipse(marker, 5.0, 5.0);
    }

    void mousePressEvent(QMouseEvent *event) override { pick(event->position()); }

    void mouseMoveEvent(QMouseEvent *event) override
    {
        if (event->buttons() & Qt::LeftButton) {
            pick(event->position());
        }
    }

private:
    void pick(const QPointF &position)
    {
        hue_ = std::clamp(position.x() / std::max(1, width() - 1), 0.0, 1.0);
        update();
        if (picked_) {
            picked_(hue_);
        }
    }

    Picked picked_;
    qreal hue_ = 0.0;
};

// In-panel HSV color picker. A native QColorDialog is a regular top-level
// window and would open underneath the layer-shell overlay, so the picker is
// a child widget of the toolbar styled like the panel itself.
class ColorPickerPopup final : public QWidget {
public:
    ColorPickerPopup(std::function<void(const QColor &)> apply, QWidget *parent)
        : QWidget(parent)
        , apply_(std::move(apply))
    {
        setObjectName(QStringLiteral("vshotColorPicker"));
        setAttribute(Qt::WA_StyledBackground, false);
        setCursor(Qt::ArrowCursor);
        auto *layout = new QVBoxLayout(this);
        layout->setContentsMargins(8, 8, 8, 8);
        layout->setSpacing(6);
        satVal_ = new SatValPane([this](qreal sat, qreal val) { sat_ = sat; val_ = val; syncPreview(); }, this);
        layout->addWidget(satVal_);
        huePane_ = new HuePane([this](qreal value) { hue_ = value; satVal_->setHue(value); syncPreview(); }, this);
        layout->addWidget(huePane_);
        // Opacity, as a 0-255 slider under the hue strip.  Its value rides on
        // the picker's result, so a colour chosen here can be translucent.
        auto *alphaRow = new QHBoxLayout;
        alphaRow->setSpacing(6);
        auto *alphaLabel = new QLabel(uiTr("Alpha"), this);
        alphaLabel->setObjectName(QStringLiteral("alphaLabel"));
        alphaRow->addWidget(alphaLabel);
        alphaSlider_ = new QSlider(Qt::Horizontal, this);
        alphaSlider_->setObjectName(QStringLiteral("alphaSlider"));
        alphaSlider_->setRange(0, 255);
        alphaSlider_->setValue(255);
        alphaSlider_->setFocusPolicy(Qt::NoFocus);
        alphaSlider_->setCursor(Qt::PointingHandCursor);
        alphaSlider_->setToolTip(uiTr("Opacity (0-255)"));
        connect(alphaSlider_, &QSlider::valueChanged, this, [this](int) { syncPreview(); });
        alphaRow->addWidget(alphaSlider_, 1);
        alphaValue_ = new QLabel(this);
        alphaValue_->setObjectName(QStringLiteral("alphaValue"));
        alphaValue_->setFixedWidth(34);
        alphaValue_->setAlignment(Qt::AlignRight | Qt::AlignVCenter);
        alphaRow->addWidget(alphaValue_);
        layout->addLayout(alphaRow);
        auto *row = new QHBoxLayout;
        row->setSpacing(4);
        preview_ = new QLabel(this);
        preview_->setObjectName(QStringLiteral("colorPreview"));
        preview_->setFixedSize(20, 20);
        row->addWidget(preview_);
        hexEdit_ = new QLineEdit(this);
        hexEdit_->setObjectName(QStringLiteral("colorHexEdit"));
        hexEdit_->setFixedWidth(72);
        hexEdit_->setMaxLength(9);
        hexEdit_->setToolTip(uiTr("Hex color (#rrggbb or #rrggbbaa)"));
        connect(hexEdit_, &QLineEdit::editingFinished, this, [this] { applyHex(); });
        row->addWidget(hexEdit_);
        row->addStretch(1);
        auto *cancel = new QPushButton(uiTr("Cancel"), this);
        cancel->setObjectName(QStringLiteral("cancelButton"));
        cancel->setCursor(Qt::PointingHandCursor);
        cancel->setFocusPolicy(Qt::NoFocus);
        connect(cancel, &QPushButton::clicked, this, [this] { hide(); });
        row->addWidget(cancel);
        auto *ok = new QPushButton(uiTr("OK"), this);
        ok->setObjectName(QStringLiteral("confirmButton"));
        ok->setCursor(Qt::PointingHandCursor);
        ok->setFocusPolicy(Qt::NoFocus);
        connect(ok, &QPushButton::clicked, this, [this] {
            if (apply_) {
                apply_(pickedColor());
            }
            hide();
        });
        row->addWidget(ok);
        layout->addLayout(row);
        setStyleSheet(QStringLiteral(
            "QLineEdit { color: #e6e1e5; background: #2a303a; border: 1px solid #565d68; "
            "border-radius: 7px; padding: 0 5px; min-height: 22px; "
            "selection-color: #00145c; selection-background-color: #dde1ff; } "
            "QLineEdit:focus { border-color: #c7d7f5; } "
            "QLabel#alphaLabel { color: #c3c9d1; font-size: 11px; } "
            "QLabel#alphaValue { color: #e6e1e5; font-size: 11px; } "
            "QSlider { min-height: 20px; background: transparent; } "
            "QSlider::groove:horizontal { height: 4px; background: #4a515c; "
            "border-radius: 2px; } "
            "QSlider::sub-page:horizontal { background: #dde1ff; border-radius: 2px; } "
            "QSlider::add-page:horizontal { background: #4a515c; border-radius: 2px; } "
            "QSlider::handle:horizontal { width: 14px; margin: -5px 0; "
            "background: #dde1ff; border: 0; border-radius: 7px; } "
            "QSlider::handle:horizontal:hover { background: #e9ecff; } "
            "QPushButton { color: #e6e1e5; background: #333a45; "
            "border: 1px solid transparent; border-radius: 7px; padding: 0 8px; "
            "min-height: 22px; font-size: 12px; } "
            "QPushButton:hover { background: #3c4450; } "
            "QPushButton#confirmButton { background: #dde1ff; color: #00145c; "
            "font-weight: 600; } "
            "QPushButton#confirmButton:hover { background: #e9ecff; } "
            "QPushButton#cancelButton { color: #ffdad6; border-color: #5f3b38; "
            "background: transparent; } "
            "QPushButton#cancelButton:hover { background: #3a2725; }"));
    }

    void setOpener(QWidget *opener) { opener_ = opener; }

    void openAt(const QColor &color, const QPoint &topLeft)
    {
        setHsvFrom(color);
        move(topLeft);
        show();
        raise();
    }

    QColor pickedColor() const
    {
        qreal hue = std::fmod(hue_, 1.0);
        if (hue < 0.0) {
            hue += 1.0;
        }
        const qreal alpha = alphaSlider_ != nullptr ? alphaSlider_->value() / 255.0 : 1.0;
        return QColor::fromHsvF(hue, std::clamp(sat_, 0.0, 1.0), std::clamp(val_, 0.0, 1.0),
                                alpha);
    }

protected:
    void paintEvent(QPaintEvent *event) override
    {
        Q_UNUSED(event);
        QPainter painter(this);
        painter.setRenderHint(QPainter::Antialiasing, true);
        const QPainterPath shape = superellipsePath(QRectF(rect()), 18.0, 5.0);
        painter.setPen(Qt::NoPen);
        painter.setBrush(QColor(30, 34, 41, 236));
        painter.drawPath(shape);
        painter.setBrush(Qt::NoBrush);
        painter.setPen(QPen(QColor(64, 71, 82), 1.0));
        painter.drawPath(superellipsePath(QRectF(rect()).adjusted(0.5, 0.5, -0.5, -0.5),
                                          17.5, 5.0));
    }

    void keyPressEvent(QKeyEvent *event) override
    {
        if (event->key() == Qt::Key_Escape) {
            hide();
            return;
        }
        QWidget::keyPressEvent(event);
    }

    void showEvent(QShowEvent *event) override
    {
        QWidget::showEvent(event);
        if (QCoreApplication *application = QCoreApplication::instance()) {
            application->installEventFilter(this);
        }
    }

    void hideEvent(QHideEvent *event) override
    {
        QWidget::hideEvent(event);
        if (QCoreApplication *application = QCoreApplication::instance()) {
            application->removeEventFilter(this);
        }
    }

    // Click-outside dismissal: any press that lands outside the popup (and on
    // anything but the swatch that toggles it) closes the picker.
    bool eventFilter(QObject *watched, QEvent *event) override
    {
        if (event->type() == QEvent::MouseButtonPress && watched->isWidgetType() &&
            isVisible()) {
            auto *widget = static_cast<QWidget *>(watched);
            if (!isAncestorOf(widget) && widget != opener_) {
                hide();
            }
        }
        return QWidget::eventFilter(watched, event);
    }

private:
    void setHsvFrom(const QColor &color)
    {
        float hue = -1.0f;
        float sat = 1.0f;
        float val = 1.0f;
        color.getHsvF(&hue, &sat, &val);
        if (hue >= 0.0f) {
            hue_ = hue;
        }
        sat_ = std::clamp(sat, 0.0f, 1.0f);
        val_ = std::clamp(val, 0.0f, 1.0f);
        if (alphaSlider_ != nullptr) {
            // Blocked so loading a colour does not echo back as a user edit;
            // the preview is refreshed below either way.
            const QSignalBlocker blocker(alphaSlider_);
            alphaSlider_->setValue(color.alpha());
        }
        satVal_->setHue(hue_);
        satVal_->setSatVal(sat_, val_);
        huePane_->setHue(hue_);
        syncPreview();
    }

    void applyHex()
    {
        // Parsed through the config spelling rather than `QColor(QString)`:
        // Qt reads an eight-digit literal as `#aarrggbb`, so `#ff880080` --
        // which this field and the config file both call semi-transparent
        // orange -- would come out the wrong way round.
        const QColor parsed = parseColorText(hexEdit_->text());
        if (parsed.isValid()) {
            setHsvFrom(parsed);
        } else {
            syncPreview();
        }
    }

    void syncPreview()
    {
        const QColor color = pickedColor();
        preview_->setPixmap(swatchIcon(color, false, devicePixelRatioF())
                                .pixmap(QSize(20, 20), devicePixelRatioF()));
        if (alphaValue_ != nullptr) {
            alphaValue_->setText(
                QStringLiteral("%1%").arg(qRound(color.alpha() * 100.0 / 255.0)));
        }
        const QSignalBlocker blocker(hexEdit_);
        // The same spelling the protocol carries and the field accepts:
        // `#rrggbb` while opaque, `#rrggbbaa` once alpha is lifted off full.
        hexEdit_->setText(colorText(color).toUpper());
    }

    std::function<void(const QColor &)> apply_;
    SatValPane *satVal_ = nullptr;
    HuePane *huePane_ = nullptr;
    QLabel *preview_ = nullptr;
    QLineEdit *hexEdit_ = nullptr;
    QSlider *alphaSlider_ = nullptr;
    QLabel *alphaValue_ = nullptr;
    QWidget *opener_ = nullptr;
    qreal hue_ = 0.0;
    qreal sat_ = 1.0;
    qreal val_ = 1.0;
};

// Font previews are applied only while visible rows are painted. Applying a
// different QFont to every QListWidgetItem makes Qt measure every installed
// family when the list is shown, which can block the overlay for seconds on
// systems with large font collections.
class FontPreviewDelegate final : public QStyledItemDelegate {
public:
    using QStyledItemDelegate::QStyledItemDelegate;

    QSize sizeHint(const QStyleOptionViewItem &, const QModelIndex &) const override
    {
        return QSize(1, 26);
    }

    void paint(QPainter *painter, const QStyleOptionViewItem &option,
               const QModelIndex &index) const override
    {
        QStyleOptionViewItem previewOption(option);
        const QString family = index.data(Qt::DisplayRole).toString();
        QFont face(family);
        face.setPointSize(11);
        previewOption.font = face;
        previewOption.fontMetrics = QFontMetrics(face);
        QStyledItemDelegate::paint(painter, previewOption, index);
    }
};

// In-panel font picker. Native font dropdowns are separate top-level windows
// and would open underneath the layer-shell overlay, so the list is a child
// widget of the toolbar styled like the panel itself.
class FontPickerPopup final : public QWidget {
public:
    FontPickerPopup(std::function<void(const QString &)> apply, QWidget *parent)
        : QWidget(parent)
        , apply_(std::move(apply))
    {
        setObjectName(QStringLiteral("vshotFontPicker"));
        setAttribute(Qt::WA_StyledBackground, false);
        setCursor(Qt::ArrowCursor);
        auto *layout = new QVBoxLayout(this);
        layout->setContentsMargins(6, 6, 6, 6);
        list_ = new QListWidget(this);
        list_->setObjectName(QStringLiteral("fontList"));
        list_->setCursor(Qt::ArrowCursor);
        list_->setFocusPolicy(Qt::NoFocus);
        list_->setHorizontalScrollBarPolicy(Qt::ScrollBarAlwaysOff);
        list_->setVerticalScrollMode(QAbstractItemView::ScrollPerPixel);
        list_->setUniformItemSizes(true);
        list_->setItemDelegate(new FontPreviewDelegate(list_));
        for (const QString &family : QFontDatabase::families()) {
            new QListWidgetItem(family, list_);
        }
        connect(list_, &QListWidget::itemClicked, this, [this](QListWidgetItem *item) {
            if (apply_ && item != nullptr) {
                apply_(item->text());
            }
            hide();
        });
        layout->addWidget(list_);
        setStyleSheet(QStringLiteral(
            "QListWidget { background: transparent; border: 0; color: #e6e1e5; "
            "font-size: 12px; outline: 0; } "
            "QListWidget::item { padding: 3px 8px; border-radius: 6px; } "
            "QListWidget::item:hover { background: #2c323b; } "
            "QListWidget::item:selected { background: #dde1ff; color: #00145c; } "
            "QScrollBar:vertical { background: transparent; width: 6px; margin: 2px; } "
            "QScrollBar::handle:vertical { background: #565d68; border-radius: 3px; "
            "min-height: 24px; } "
            "QScrollBar::add-line:vertical, QScrollBar::sub-line:vertical { height: 0; } "
            "QScrollBar::add-page:vertical, QScrollBar::sub-page:vertical { background: none; }"));
    }

    void setOpener(QWidget *opener) { opener_ = opener; }

    void openAt(const QString &currentFamily, const QPoint &topLeft)
    {
        const QString family = currentFamily.isEmpty() ? QApplication::font().family() : currentFamily;
        const QList<QListWidgetItem *> matches = list_->findItems(family, Qt::MatchExactly);
        if (matches.isEmpty()) {
            list_->clearSelection();
            list_->setCurrentItem(nullptr);
        } else {
            list_->setCurrentItem(matches.constFirst());
            list_->scrollToItem(matches.constFirst(), QAbstractItemView::PositionAtCenter);
        }
        move(topLeft);
        show();
        raise();
    }

protected:
    void paintEvent(QPaintEvent *event) override
    {
        Q_UNUSED(event);
        QPainter painter(this);
        painter.setRenderHint(QPainter::Antialiasing, true);
        painter.setPen(Qt::NoPen);
        painter.setBrush(QColor(30, 34, 41, 236));
        painter.drawPath(superellipsePath(QRectF(rect()), 18.0, 5.0));
        painter.setBrush(Qt::NoBrush);
        painter.setPen(QPen(QColor(64, 71, 82), 1.0));
        painter.drawPath(superellipsePath(QRectF(rect()).adjusted(0.5, 0.5, -0.5, -0.5),
                                          17.5, 5.0));
    }

    void keyPressEvent(QKeyEvent *event) override
    {
        if (event->key() == Qt::Key_Escape) {
            hide();
            return;
        }
        QWidget::keyPressEvent(event);
    }

    void showEvent(QShowEvent *event) override
    {
        QWidget::showEvent(event);
        if (QCoreApplication *application = QCoreApplication::instance()) {
            application->installEventFilter(this);
        }
    }

    void hideEvent(QHideEvent *event) override
    {
        QWidget::hideEvent(event);
        if (QCoreApplication *application = QCoreApplication::instance()) {
            application->removeEventFilter(this);
        }
    }

    bool eventFilter(QObject *watched, QEvent *event) override
    {
        if (event->type() == QEvent::MouseButtonPress && watched->isWidgetType() &&
            isVisible()) {
            auto *widget = static_cast<QWidget *>(watched);
            if (!isAncestorOf(widget) && widget != opener_) {
                hide();
            }
        }
        return QWidget::eventFilter(watched, event);
    }

private:
    QListWidget *list_ = nullptr;
    std::function<void(const QString &)> apply_;
    QWidget *opener_ = nullptr;
};

// Paints squircle highlight cards behind the command-row tool buttons; QSS
// backgrounds are limited to circular corners.
class ToolCardFrame final : public QWidget {
public:
    explicit ToolCardFrame(QWidget *parent)
        : QWidget(parent)
    {
        setAttribute(Qt::WA_StyledBackground, false);
    }

protected:
    bool eventFilter(QObject *watched, QEvent *event) override
    {
        switch (event->type()) {
        case QEvent::Enter:
            hovered_ = static_cast<QWidget *>(watched);
            update();
            break;
        case QEvent::Leave:
            if (hovered_ == watched) {
                hovered_ = nullptr;
            }
            update();
            break;
        case QEvent::MouseButtonPress:
            pressed_ = static_cast<QWidget *>(watched);
            update();
            break;
        case QEvent::MouseButtonRelease:
            pressed_ = nullptr;
            update();
            break;
        default:
            break;
        }
        return QWidget::eventFilter(watched, event);
    }

    void paintEvent(QPaintEvent *event) override
    {
        Q_UNUSED(event);
        QPainter painter(this);
        painter.setRenderHint(QPainter::Antialiasing, true);
        const auto buttons = findChildren<QToolButton *>();
        QToolButton *active = nullptr;
        for (QToolButton *button : buttons) {
            if (button->isVisibleTo(this) && button->property("active").toBool()) {
                active = button;
                break;
            }
        }
        painter.setPen(Qt::NoPen);
        for (QToolButton *button : buttons) {
            if (!button->isVisibleTo(this) || button == active ||
                (hovered_ != button && pressed_ != button)) {
                continue;
            }
            painter.setBrush(pressed_ == button ? QColor(53, 60, 70) : QColor(44, 50, 59));
            painter.drawPath(superellipsePath(QRectF(button->geometry()), 14.0, 5.0));
        }
        if (active != nullptr) {
            painter.setBrush(QColor(221, 225, 255));
            painter.drawPath(superellipsePath(QRectF(active->geometry()), 14.0, 5.0));
        }
    }

private:
    QWidget *hovered_ = nullptr;
    QWidget *pressed_ = nullptr;
};

// An image read out of the clipboard, plus where it came from when the
// clipboard named a file rather than carrying pixels.
struct ClipboardImage {
    bool installed = true; // `wl-paste` could be run at all
    bool offered = false;  // something is copied
    QImage image;
    QString source;
};

// How long a clipboard helper is given to start and to answer.  Both `wl-paste`
// and `wl-copy` are small programs that either answer at once or not at all.
constexpr int kClipboardProcessTimeoutMs = 5000;

// One `wl-paste` run. `false` means the program could not be started at all,
// which is a different failure from an empty clipboard; `ok` says whether the
// request itself succeeded.
bool runWlPaste(const QStringList &arguments, QByteArray *bytes, bool *ok)
{
    constexpr int kTimeoutMs = 5000;
    QProcess process;
    process.setProgram(QStringLiteral("wl-paste"));
    process.setArguments(arguments);
    process.setStandardInputFile(QProcess::nullDevice());
    process.start();
    if (!process.waitForStarted(kTimeoutMs)) {
        return false;
    }
    const bool finished = process.waitForFinished(kTimeoutMs);
    if (!finished) {
        // A clipboard owner that never answers must not hold the editor's
        // event loop any longer than this.
        process.kill();
        process.waitForFinished(kTimeoutMs);
    }
    *bytes = process.readAllStandardOutput();
    *ok = finished && process.exitStatus() == QProcess::NormalExit && process.exitCode() == 0;
    return true;
}

// Puts `text` on the clipboard through `wl-copy`, the writing counterpart of
// the `wl-paste` above -- and, like it, used instead of Qt's own clipboard,
// which is unreliable under this compositor setup.  The daemon has its own
// copy of this; the two processes share no code.
//
// `wl-copy` forks and the child stays alive as the selection owner, so only
// the short-lived process started here is waited for: the copy outlives the
// call.
bool runWlCopy(const QString &text)
{
    constexpr int kTimeoutMs = 5000;
    QProcess process;
    process.setProgram(QStringLiteral("wl-copy"));
    // `--` ends the options: a value is content, never a switch.
    process.setArguments({QStringLiteral("--")});
    process.start();
    if (!process.waitForStarted(kTimeoutMs)) {
        return false;
    }
    process.write(text.toUtf8());
    process.closeWriteChannel();
    const bool finished = process.waitForFinished(kTimeoutMs);
    if (!finished) {
        process.kill();
        process.waitForFinished(kTimeoutMs);
        return false;
    }
    return process.exitStatus() == QProcess::NormalExit && process.exitCode() == 0;
}

// The image encodings worth asking for, best first; any other `image/*` the
// clipboard offers is taken after these.
constexpr const char *kClipboardImageTypes[] = {
    "image/png", "image/jpeg", "image/webp", "image/bmp", "image/tiff",
};
// Reads an image out of the clipboard the way the pin daemon does: through
// `wl-paste`, not Qt's own clipboard. Qt implements only the wlroots
// `zwlr_data_control_v1`, which a compositor offering the standardized
// `ext_data_control_manager_v1` instead (KWin) leaves empty.
//
// Resolution order: image data first, then a copied file -- as a URI list, then
// as a plain path. Copying a file in a file manager is the ordinary way to say
// "this picture", and it puts a path on the clipboard rather than pixels.
ClipboardImage readClipboardImage()
{
    ClipboardImage result;
    QByteArray listed;
    bool ok = false;
    if (!runWlPaste({QStringLiteral("--list-types")}, &listed, &ok)) {
        result.installed = false;
        return result;
    }
    if (!ok) {
        return result; // nothing is copied
    }
    result.offered = true;
    QStringList types;
    for (const QByteArray &line : listed.split('\n')) {
        const QString type = QString::fromUtf8(line).trimmed();
        if (!type.isEmpty() && !types.contains(type)) {
            types.append(type);
        }
    }
    const auto fetch = [&types](const QString &type) -> QByteArray {
        if (!types.contains(type)) {
            return QByteArray();
        }
        QByteArray bytes;
        bool fetched = false;
        if (!runWlPaste({QStringLiteral("--type"), type, QStringLiteral("--no-newline")}, &bytes,
                        &fetched)
            || !fetched) {
            return QByteArray();
        }
        return bytes;
    };

    QString imageType;
    for (const char *candidate : kClipboardImageTypes) {
        const QString type = QLatin1String(candidate);
        if (types.contains(type)) {
            imageType = type;
            break;
        }
    }
    if (imageType.isEmpty()) {
        for (const QString &type : types) {
            if (type.startsWith(QLatin1String("image/"))) {
                imageType = type;
                break;
            }
        }
    }
    if (!imageType.isEmpty()) {
        const QByteArray bytes = fetch(imageType);
        if (!bytes.isEmpty()) {
            result.image = QImage::fromData(bytes);
            if (!result.image.isNull()) {
                return result;
            }
        }
    }

    // A copied file: a file manager offers `text/uri-list`, and a terminal
    // that copies a path offers plain text.
    QStringList candidates;
    for (const QUrl &url : QUrl::fromStringList(
             QString::fromUtf8(fetch(QStringLiteral("text/uri-list")))
                 .split(QLatin1Char('\n'), Qt::SkipEmptyParts))) {
        if (url.isLocalFile()) {
            candidates.append(url.toLocalFile());
        }
    }
    for (const char *type : {"text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING"}) {
        const QString text = QString::fromUtf8(fetch(QLatin1String(type))).trimmed();
        if (!text.isEmpty() && !text.contains(QLatin1Char('\n'))) {
            candidates.append(text);
            break;
        }
    }
    for (const QString &path : candidates) {
        QImageReader reader(path);
        if (!reader.canRead()) {
            continue;
        }
        const QImage image = reader.read();
        if (!image.isNull()) {
            result.image = image;
            result.source = path;
            return result;
        }
    }
    return result;
}

} // namespace

struct OverlayController::Gesture {
    enum class Type {
        None,
        Selecting,
        Moving,
        Resizing,
        MovingAnnotation,
        ResizingAnnotation,
        Drawing,
        // The pen path.  It is the one gesture that outlives a release: a path
        // spans as many presses as it has anchors, so `release` leaves this
        // state standing until the path is closed or double-clicked.
        Bezier,
    };

    Type type = Type::None;
    Point anchor;
    Point current;
    LogicalRect origin;
    int handle = 0;
    QVector<Point> points;
    // The in-progress freehand stroke, rasterized incrementally.  Re-stroking
    // the whole path on every paint is O(points) each time -- quadratic over a
    // long scribble -- so each paint adds only the points appended since the
    // last one and blits the accumulated image instead.
    QImage liveRaster;
    QPoint liveOrigin;
    int liveOutput = -1;
    int liveBaked = 0;     // points already in liveRaster
    double liveLength = 0.0; // local path length up to the last baked point
    QByteArray liveKey;    // style/output/size the raster was built for
};

class OverlayController::FloatingToolbar final : public QWidget {
public:
    explicit FloatingToolbar(OverlayController *controller, QWidget *parent)
        : QWidget(parent)
        , controller_(controller)
    {
        setObjectName(QStringLiteral("vshotToolbar"));
        setAttribute(Qt::WA_TranslucentBackground);
        setAttribute(Qt::WA_StyledBackground, false);
        setAutoFillBackground(false);
        // Blank panel areas are the drag grip; interactive children override.
        setCursor(Qt::SizeAllCursor);
        setStyleSheet(QStringLiteral(
            "QWidget#vshotToolbar { color: #e6e1e5; } "
            "QLabel { color: #c3c9d1; font-size: 11px; } "
            "QPushButton { color: #e6e1e5; background: transparent; "
            "border: 1px solid transparent; border-radius: 8px; padding: 0 10px; "
            "min-height: 28px; font-size: 12px; } "
            "QPushButton:hover { background: #2c323b; } "
            "QPushButton:pressed { background: #353c46; } "
            "QPushButton:focus { border-color: #c7d7f5; } "
            "QPushButton:disabled { color: #6f7680; background: transparent; "
            "border-color: transparent; } "
            "QToolButton { color: #dfe4ec; background: transparent; border: 0; "
            "border-radius: 10px; padding: 2px; font-size: 10px; } "
            "QToolButton[active=\"true\"] { color: #00145c; } "
            "QToolButton:disabled { background: transparent; color: #6f7680; } "
            "QPushButton#undoButton, QPushButton#redoButton { "
            "border-radius: 10px; padding: 0; } "
            "QPushButton#confirmButton { background: #dde1ff; color: #00145c; "
            "font-weight: 600; } "
            "QPushButton#confirmButton:hover { background: #e9ecff; } "
            "QPushButton#confirmButton:pressed { background: #c3cafb; } "
            "QPushButton#cancelButton { color: #ffdad6; border-color: #5f3b38; } "
            "QPushButton#cancelButton:hover { background: #3a2725; "
            "border-color: #8c5650; } "
            "QFrame#toolbarDivider { background: #3a414b; max-width: 1px; "
            "border: 0; } "
            "QFrame#styleDivider { background: #343b45; max-height: 1px; "
            "border: 0; } "
            "QPushButton[segment=\"true\"] { border: 0; background: transparent; "
            "border-radius: 0; padding: 0 9px; min-height: 22px; color: #d9dde3; } "
            "QPushButton[segment=\"true\"]:hover { color: #ffffff; } "
            "QPushButton[segment=\"true\"]:disabled { color: #6f7680; } "
            "QPushButton[segment=\"true\"][active=\"true\"] { color: #00145c; "
            "font-weight: 600; } "
            "QPushButton[segment=\"true\"][active=\"true\"]:hover { color: #00145c; } "
            "QSlider { min-height: 20px; background: transparent; } "
            "QSlider::groove:horizontal { height: 4px; background: #4a515c; "
            "border-radius: 2px; } "
            "QSlider::sub-page:horizontal { background: #dde1ff; "
            "border-radius: 2px; } "
            "QSlider::add-page:horizontal { background: #4a515c; "
            "border-radius: 2px; } "
            "QSlider::handle:horizontal { width: 14px; margin: -5px 0; "
            "background: #dde1ff; border: 0; border-radius: 7px; } "
            "QSlider::handle:horizontal:hover { background: #e9ecff; } "
            "QSpinBox { min-height: 22px; color: #e6e1e5; background: #2a303a; "
            "border: 1px solid #565d68; border-radius: 8px; padding: 0 5px; "
            "selection-color: #00145c; selection-background-color: #dde1ff; } "
            "QSpinBox:hover { border-color: #7b8290; } "
            "QSpinBox:focus { border-color: #c7d7f5; } "
            "QSpinBox:disabled { color: #6f7680; background: #242933; } "
            "QAbstractSpinBox::up-button, QAbstractSpinBox::down-button { "
            "width: 14px; border: 0; background: transparent; } "
            "QAbstractSpinBox::up-button:hover, QAbstractSpinBox::down-button:hover { "
            "background: #3a414b; border-radius: 4px; }"));

        auto *rootLayout = new QVBoxLayout(this);
        rootLayout->setContentsMargins(6, 5, 6, 6);
        rootLayout->setSpacing(4);

        auto *toolSurface = new ToolCardFrame(this);
        toolSurface->setObjectName(QStringLiteral("toolbarCommandSurface"));
        toolSurface->setCursor(Qt::ArrowCursor);
        commandSurface_ = toolSurface;
        auto *toolLayout = new QHBoxLayout(toolSurface);
        toolLayout->setContentsMargins(2, 2, 2, 2);
        toolLayout->setSpacing(2);
        addTool(toolLayout, uiTr("Select"), Tool::Select);
        addTool(toolLayout, uiTr("Rect"), Tool::Rectangle);
        addTool(toolLayout, uiTr("Ellipse"), Tool::Ellipse);
        addTool(toolLayout, uiTr("Arrow"), Tool::Arrow);
        addTool(toolLayout, uiTr("Line"), Tool::Line);
        addTool(toolLayout, uiTr("Wave"), Tool::Wave);
        addTool(toolLayout, uiTr("Bezier"), Tool::Bezier);
        addTool(toolLayout, uiTr("Draw"), Tool::Pen);
        addTool(toolLayout, uiTr("Text"), Tool::Text);
        addTool(toolLayout, uiTr("Number"), Tool::Number);
        addTool(toolLayout, uiTr("Mosaic"), Tool::Mosaic);
        // The two one-shot actions sit at the end of the tool row, drawn the
        // same way: they are the same kind of thing to click, and a second row
        // of text buttons beside them only made the bar taller.  They are not
        // modes -- nothing stays selected -- so they are kept out of
        // `toolButtons_`, which is what the active-state pass walks.
        //
        // Paste takes an image off disk through the file dialog; Ctrl+V takes
        // whatever is on the clipboard.  Both land in the same paste.
        auto *paste = addToolAction(toolLayout, uiTr("Image"), pasteIcon(QColor(230, 225, 229),
                                                                       devicePixelRatioF()),
                                    uiTr("Paste an image onto the capture (Ctrl+V for the "
                                         "clipboard)"),
                                    QStringLiteral("pasteButton"));
        connect(paste, &QToolButton::clicked, [controller = controller_] {
            QString error;
            if (!controller->pasteFromFile(&error)) {
                std::fprintf(stderr, "vshot-qt-ui: %s\n", error.toUtf8().constData());
                std::fflush(stderr);
            }
        });
        // Text selection: the recognized characters of the selection are drawn
        // where they were and the pointer selects a range of them, then the
        // range is copied.  The mode lives in the controller -- the toolbar has
        // no selection to work on -- so the button only asks for it, and the
        // controller reports the result back through the callback below.
        auto *text = addToolAction(toolLayout, uiTr("Text+"),
                                   recognizeTextIcon(QColor(230, 225, 229), devicePixelRatioF()),
                                   uiTr("Select the text in the selection and copy what you "
                                        "select"),
                                   QStringLiteral("ocrButton"),
                                   {uiTr("OCR…"), uiTr("Copied"), uiTr("Failed")});
        textButton_ = text;
        connect(text, &QToolButton::clicked, [controller = controller_] {
            controller->beginTextSelection(nullptr);
        });
        // The result is reported where the user is looking: the button itself,
        // which is the thing they just clicked.  The copy can be triggered by a
        // key, which the controller sees and this toolbar does not, so the
        // controller reports through this callback rather than the handler
        // above.  The width was settled for every label it can show when it was
        // built, so the word is not elided now.  `Busy` and `Idle` are the two
        // reports that do not expire: the first is replaced by the outcome that
        // follows it, the second is the button's own label coming back.
        controller_->setTextResultCallback([this](TextOutcome outcome, const QString &) {
            if (textButton_ == nullptr) {
                return;
            }
            switch (outcome) {
            case TextOutcome::Busy:
                // Short on purpose: the button is sized once, for every label
                // it can ever show, and a long word here would widen it past
                // every other button in the row for good.
                textButton_->setText(uiTr("OCR…"));
                return;
            case TextOutcome::Idle:
                textButton_->setText(uiTr("Text+"));
                return;
            case TextOutcome::Copied:
                textButton_->setText(uiTr("Copied"));
                break;
            case TextOutcome::Failed:
                textButton_->setText(uiTr("Failed"));
                break;
            }
            QTimer::singleShot(1200, textButton_, [this] {
                if (controller_->isFinished() || controller_->isCancelled()) {
                    return;
                }
                textButton_->setText(uiTr("Text+"));
            });
        });
        // Scrolling capture: the region the user drew is scrolled with
        // synthetic wheels and stitched into one tall image.  It is not a mode
        // -- nothing stays selected -- and it is not the ordinary confirmation
        // either: it ends the session, and the CLI reads the answer as "scroll
        // this, do not keep this frame".  Only the region editor is offered
        // it: window editing and the pin editor reuse this toolbar on a
        // picture that has nothing to scroll, and a button that can never be
        // pressed is worse than no button at all.  Whether it can be pressed
        // right now -- the selection has to fit in one output -- is
        // `syncState`'s to say.
        if (controller_->longAllowed_) {
            longButton_ = addToolAction(
                toolLayout, uiTr("Scroll"),
                scrollIcon(QColor(230, 225, 229), devicePixelRatioF()),
                uiTr("Scroll the selection and stitch it into one tall image"),
                QStringLiteral("longButton"));
            connect(longButton_, &QToolButton::clicked,
                    [controller = controller_] { controller->requestLongCapture(); });
        }
        // Every button in this row, the two above included, gets the frame's
        // hover and press painting; it finds them by type, so this has to run
        // after the last one was added.
        for (QToolButton *button : toolSurface->findChildren<QToolButton *>()) {
            button->installEventFilter(toolSurface);
        }
        toolLayout->addSpacing(5);
        auto *historyDivider = new QFrame(toolSurface);
        historyDivider->setObjectName(QStringLiteral("toolbarDivider"));
        historyDivider->setFrameShape(QFrame::VLine);
        historyDivider->setFrameShadow(QFrame::Plain);
        historyDivider->setFixedHeight(20);
        historyDivider->setCursor(Qt::ArrowCursor);
        toolLayout->addWidget(historyDivider);
        toolLayout->addSpacing(3);
        undo_ = addActionButton(toolLayout, uiTr("Undo"));
        undo_->setObjectName(QStringLiteral("undoButton"));
        undo_->setText(QString());
        undo_->setIcon(historyIcon(false, QColor(QStringLiteral("#dfe4ec"))));
        undo_->setIconSize(QSize(18, 18));
        undo_->setFixedSize(32, 28);
        undo_->setToolTip(uiTr("Undo last change (Ctrl+Z)"));
        connect(undo_, &QPushButton::clicked, [controller = controller_] { controller->undo(); });
        redo_ = addActionButton(toolLayout, uiTr("Redo"));
        redo_->setObjectName(QStringLiteral("redoButton"));
        redo_->setText(QString());
        redo_->setIcon(historyIcon(true, QColor(QStringLiteral("#dfe4ec"))));
        redo_->setIconSize(QSize(18, 18));
        redo_->setFixedSize(32, 28);
        redo_->setToolTip(uiTr("Redo last change (Ctrl+Y)"));
        connect(redo_, &QPushButton::clicked, [controller = controller_] { controller->redo(); });
        toolLayout->addSpacing(5);
        auto *actionDivider = new QFrame(toolSurface);
        actionDivider->setObjectName(QStringLiteral("toolbarDivider"));
        actionDivider->setFrameShape(QFrame::VLine);
        actionDivider->setFrameShadow(QFrame::Plain);
        actionDivider->setFixedHeight(20);
        actionDivider->setCursor(Qt::ArrowCursor);
        toolLayout->addWidget(actionDivider);
        toolLayout->addSpacing(3);
        auto *ok = addActionButton(toolLayout, uiTr("OK"));
        ok->setObjectName(QStringLiteral("confirmButton"));
        ok->setToolTip(uiTr("Confirm capture (Enter)"));
        connect(ok, &QPushButton::clicked, [controller = controller_] { controller->confirm(); });
        auto *cancel = addActionButton(toolLayout, uiTr("Cancel"));
        cancel->setObjectName(QStringLiteral("cancelButton"));
        cancel->setToolTip(uiTr("Discard capture (Esc)"));
        connect(cancel, &QPushButton::clicked, [controller = controller_] { controller->cancel(); });
        rootLayout->addWidget(toolSurface);

        styleDivider_ = new QFrame(this);
        styleDivider_->setObjectName(QStringLiteral("styleDivider"));
        styleDivider_->setCursor(Qt::ArrowCursor);
        rootLayout->addWidget(styleDivider_);

        styleRow_ = new QWidget(this);
        styleRow_->setObjectName(QStringLiteral("toolbarStyleRow"));
        styleRow_->setCursor(Qt::ArrowCursor);
        styleRow_->setSizePolicy(QSizePolicy::Fixed, QSizePolicy::Fixed);
        auto *styleLayout = new QVBoxLayout(styleRow_);
        styleLayout->setContentsMargins(2, 2, 2, 2);
        styleLayout->setSpacing(4);
        styleLayout->setAlignment(Qt::AlignLeft);
        optionsRow_ = new QWidget(styleRow_);
        optionsRow_->setObjectName(QStringLiteral("toolbarOptionsRow"));
        optionsRow_->setCursor(Qt::ArrowCursor);
        auto *optionLayout = new QHBoxLayout(optionsRow_);
        optionLayout->setContentsMargins(0, 0, 0, 0);
        optionLayout->setSpacing(4);
        optionLayout->setAlignment(Qt::AlignLeft);
        numericRow_ = new QWidget(styleRow_);
        numericRow_->setObjectName(QStringLiteral("toolbarNumericRow"));
        numericRow_->setCursor(Qt::ArrowCursor);
        auto *numericLayout = new QHBoxLayout(numericRow_);
        numericLayout->setContentsMargins(0, 0, 0, 0);
        numericLayout->setSpacing(4);
        numericLayout->setAlignment(Qt::AlignLeft);

        // Color swatches.
        colorGroup_ = addGroup(optionLayout);
        colorGroup_->setObjectName(QStringLiteral("colorGroup"));
        static const QColor palette[] = {
            QColor(255, 64, 64),   QColor(255, 165, 0), QColor(255, 225, 53),
            QColor(46, 204, 64),   QColor(61, 111, 214), QColor(255, 255, 255),
            QColor(17, 17, 17),
        };
        for (const QColor &color : palette) {
            auto *swatch = new QPushButton(colorGroup_);
            swatch->setObjectName(QStringLiteral("colorSwatch"));
            swatch->setFixedSize(20, 20);
            swatch->setIconSize(QSize(20, 20));
            // The chip is painted by swatchIcon(); keep every QSS background
            // off the button.
            swatch->setStyleSheet(QStringLiteral(
                "QPushButton#colorSwatch { background: transparent; border: 0; "
                "padding: 0; min-height: 0px; } "
                "QPushButton#colorSwatch:hover { background: transparent; } "
                "QPushButton#colorSwatch:pressed { background: transparent; }"));
            swatch->setCursor(Qt::PointingHandCursor);
            swatch->setFocusPolicy(Qt::NoFocus);
            swatch->setAccessibleName(uiTr("Annotation color %1").arg(color.name()));
            swatchColors_.push_back(color);
            swatchButtons_.push_back(swatch);
            colorGroup_->layout()->addWidget(swatch);
            const QColor swatchColor = color;
            // A swatch carries no alpha of its own: clicking one changes the
            // RGB and leaves the user's opacity where they set it, so picking a
            // translucent pen and then a colour does not silently make it opaque.
            connect(swatch, &QPushButton::clicked, [controller = controller_, swatchColor] {
                QColor chosen = swatchColor;
                chosen.setAlpha(controller->currentColor_.alpha());
                controller->setCurrentColor(chosen);
            });
        }

        // Custom color entry point: opens the HSV picker popup.
        auto *picker = new QPushButton(colorGroup_);
        picker->setObjectName(QStringLiteral("colorPickerButton"));
        picker->setFixedSize(20, 20);
        picker->setIconSize(QSize(20, 20));
        picker->setIcon(colorPickerIcon(false, devicePixelRatioF()));
        picker->setStyleSheet(QStringLiteral(
            "QPushButton#colorPickerButton { background: transparent; border: 0; "
            "padding: 0; min-height: 0px; } "
            "QPushButton#colorPickerButton:hover { background: transparent; } "
            "QPushButton#colorPickerButton:pressed { background: transparent; }"));
        picker->setCursor(Qt::PointingHandCursor);
        picker->setFocusPolicy(Qt::NoFocus);
        picker->setToolTip(uiTr("Custom color"));
        picker->setAccessibleName(uiTr("Custom color"));
        connect(picker, &QPushButton::clicked, this, [this] { togglePickerPopup(); });
        colorGroup_->layout()->addWidget(picker);
        pickerButton_ = picker;

        // Text font family picker. It is only visible while the text tool or a
        // text annotation is active, but the selected family is retained for
        // the next text label.
        fontGroup_ = addGroup(optionLayout);
        fontGroup_->setObjectName(QStringLiteral("fontGroup"));
        fontButton_ = new QPushButton(fontGroup_);
        fontButton_->setObjectName(QStringLiteral("fontButton"));
        fontButton_->setFixedWidth(150);
        fontButton_->setSizePolicy(QSizePolicy::Fixed, QSizePolicy::Fixed);
        fontButton_->setCursor(Qt::PointingHandCursor);
        fontButton_->setFocusPolicy(Qt::NoFocus);
        fontButton_->setToolTip(uiTr("Text font family"));
        fontButton_->setAccessibleName(uiTr("Text font family"));
        fontGroup_->layout()->addWidget(fontButton_);
        connect(fontButton_, &QPushButton::clicked, this, [this] { toggleFontPopup(); });

        // Line styles.
        dashGroup_ = addGroup(optionLayout, true);
        dashGroup_->setObjectName(QStringLiteral("dashGroup"));
        addStyleButtons(dashGroup_, {uiTr("Solid"), uiTr("Dash"),
                                     uiTr("Dot")},
                        uiTr("Line style"),
                        {QStringLiteral("solid"), QStringLiteral("dashed"),
                         QStringLiteral("dotted")},
                        &dashButtons_, &dashValues_, [controller = controller_](QString value) {
                            controller->setDash(value);
                        });

        // Arrow head styles.
        arrowStyleGroup_ = addGroup(optionLayout, true);
        arrowStyleGroup_->setObjectName(QStringLiteral("arrowStyleGroup"));
        addStyleButtons(arrowStyleGroup_, {uiTr("Open V"), uiTr("Filled")},
                        uiTr("Arrow head style"),
                        {QStringLiteral("open"), QStringLiteral("filled")},
                        &arrowStyleButtons_, &arrowStyleValues_,
                        [controller = controller_](QString value) {
                            controller->setArrowStyle(value);
                        });

        // Stroke widths (logical pixels).
        widthGroup_ = addGroup(numericLayout);
        widthGroup_->setObjectName(QStringLiteral("widthGroup"));
        widthGroup_->setProperty("toolbarGroupKind", "numeric");
        widthLabel_ = new QLabel(uiTr("Width"), widthGroup_);
        widthLabel_->setFixedWidth(
            QFontMetrics(widthLabel_->font()).horizontalAdvance(uiTr("Width %1").arg(64)));
        widthSlider_ = new QSlider(Qt::Horizontal, widthGroup_);
        widthSlider_->setObjectName(QStringLiteral("widthSlider"));
        widthSlider_->setRange(1, 64);
        widthSlider_->setSingleStep(1);
        widthSlider_->setPageStep(4);
        widthSlider_->setFixedWidth(110);
        widthSlider_->setValue(static_cast<int>(controller_->currentWidth_));
        widthSlider_->setToolTip(uiTr("Stroke width (1-64 logical pixels)"));
        widthGroup_->layout()->addWidget(widthLabel_);
        widthGroup_->layout()->addWidget(widthSlider_);

        // Arrow head size.
        arrowGroup_ = addGroup(numericLayout);
        arrowGroup_->setObjectName(QStringLiteral("arrowSizeGroup"));
        arrowGroup_->setProperty("toolbarGroupKind", "numeric");
        arrowLabel_ = new QLabel(uiTr("Arrow"), arrowGroup_);
        arrowLabel_->setFixedWidth(
            QFontMetrics(arrowLabel_->font()).horizontalAdvance(uiTr("Arrow %1").arg(8)));
        arrowSlider_ = new QSlider(Qt::Horizontal, arrowGroup_);
        arrowSlider_->setObjectName(QStringLiteral("arrowSizeSlider"));
        arrowSlider_->setRange(1, 8);
        arrowSlider_->setSingleStep(1);
        arrowSlider_->setFixedWidth(80);
        arrowSlider_->setValue(static_cast<int>(controller_->arrowSize_));
        arrowSlider_->setToolTip(uiTr("Arrow head size (1-8)"));
        arrowGroup_->layout()->addWidget(arrowLabel_);
        arrowGroup_->layout()->addWidget(arrowSlider_);

        // Text size as a bounded numeric input.
        textGroup_ = addGroup(numericLayout);
        textGroup_->setObjectName(QStringLiteral("textGroup"));
        textGroup_->setProperty("toolbarGroupKind", "numeric");
        textLabel_ = new QLabel(uiTr("Text"), textGroup_);
        textSpin_ = new QSpinBox(textGroup_);
        textSpin_->setObjectName(QStringLiteral("textSizeSpinBox"));
        textSpin_->setRange(kMinTextPixels, kMaxTextPixels);
        textSpin_->setSingleStep(1);
        textSpin_->setKeyboardTracking(false);
        textSpin_->setFixedSize(64, 22);
        textSpin_->setValue(clampTextPixels(static_cast<int>(controller_->textSize_)));
        textSpin_->setToolTip(uiTr("Text size in pixels (7-448)"));
        textGroup_->layout()->addWidget(textLabel_);
        textGroup_->layout()->addWidget(textSpin_);

        // Mosaic area shapes.
        mosaicGroup_ = addGroup(optionLayout, true);
        mosaicGroup_->setObjectName(QStringLiteral("mosaicGroup"));
        addStyleButtons(mosaicGroup_,
                        {uiTr("Rect"), uiTr("Ellip"),
                         uiTr("Brush")},
                        uiTr("Mosaic shape"),
                        {QStringLiteral("rect"), QStringLiteral("ellipse"),
                         QStringLiteral("brush")},
                        &mosaicButtons_, &mosaicValues_,
                        [controller = controller_](QString value) {
                            controller->setMosaicShape(value);
                        });

        // Number badge styles: the four looks the number tool can place.  One
        // segment per look, shown only while the number tool or a placed badge
        // is the style target.
        numberGroup_ = addGroup(optionLayout, true);
        numberGroup_->setObjectName(QStringLiteral("numberGroup"));
        addStyleButtons(numberGroup_,
                        {uiTr("Fill"), uiTr("Ring"), uiTr("Square"), uiTr("Plain")},
                        uiTr("Number style"),
                        {numberStyleValue(NumberStyle::FilledCircle),
                         numberStyleValue(NumberStyle::Ring),
                         numberStyleValue(NumberStyle::Square),
                         numberStyleValue(NumberStyle::Plain)},
                        &numberButtons_, &numberValues_,
                        [controller = controller_](QString value) {
                            controller->setNumberStyle(numberStyleForName(value));
                        });

        // Mosaic strength.
        strengthGroup_ = addGroup(numericLayout);
        strengthGroup_->setObjectName(QStringLiteral("strengthGroup"));
        strengthGroup_->setProperty("toolbarGroupKind", "numeric");
        strengthLabel_ = new QLabel(uiTr("Mosaic"), strengthGroup_);
        strengthLabel_->setFixedWidth(
            QFontMetrics(strengthLabel_->font()).horizontalAdvance(uiTr("Mosaic %1").arg(3)));
        strengthSlider_ = new QSlider(Qt::Horizontal, strengthGroup_);
        strengthSlider_->setObjectName(QStringLiteral("mosaicStrengthSlider"));
        strengthSlider_->setRange(1, 3);
        strengthSlider_->setSingleStep(1);
        strengthSlider_->setFixedWidth(64);
        strengthSlider_->setValue(static_cast<int>(controller_->mosaicStrength_));
        strengthSlider_->setToolTip(uiTr("Mosaic strength (1-3)"));
        strengthGroup_->layout()->addWidget(strengthLabel_);
        strengthGroup_->layout()->addWidget(strengthSlider_);

        styleLayout->addWidget(optionsRow_);
        styleLayout->addWidget(numericRow_);
        rootLayout->addWidget(styleRow_);

        connect(widthSlider_, &QSlider::sliderPressed, this,
                [controller = controller_] { controller->beginStyleAdjustment(); });
        connect(widthSlider_, &QSlider::sliderReleased, this,
                [controller = controller_] { controller->endStyleAdjustment(); });
        connect(widthSlider_, &QSlider::valueChanged, this,
                [controller = controller_](int value) {
                    controller->setWidth(static_cast<std::uint32_t>(value));
                });
        connect(arrowSlider_, &QSlider::sliderPressed, this,
                [controller = controller_] { controller->beginStyleAdjustment(); });
        connect(arrowSlider_, &QSlider::sliderReleased, this,
                [controller = controller_] { controller->endStyleAdjustment(); });
        connect(arrowSlider_, &QSlider::valueChanged, this,
                [controller = controller_](int value) {
                    controller->setArrowSize(static_cast<std::uint32_t>(value));
                });
        connect(strengthSlider_, &QSlider::sliderPressed, this,
                [controller = controller_] { controller->beginStyleAdjustment(); });
        connect(strengthSlider_, &QSlider::sliderReleased, this,
                [controller = controller_] { controller->endStyleAdjustment(); });
        connect(strengthSlider_, &QSlider::valueChanged, this,
                [controller = controller_](int value) {
                    controller->setMosaicStrength(static_cast<std::uint32_t>(value));
                });
        connect(textSpin_, qOverload<int>(&QSpinBox::valueChanged), this,
                [controller = controller_](int value) {
                    controller->beginStyleAdjustment();
                    controller->setTextSize(static_cast<std::uint32_t>(clampTextPixels(value)));
                });
        connect(textSpin_, &QSpinBox::editingFinished, this,
                [controller = controller_] { controller->endStyleAdjustment(); });
        // The popup lives on the overlay, not on the toolbar: Qt clips child
        // widgets to their parent's rect, and the popup must extend past the
        // panel's bounds. It follows the toolbar across overlays below.
        pickerPopup_ = new ColorPickerPopup([controller = controller_](const QColor &color) {
            controller->setCurrentColor(color);
        }, parent);
        pickerPopup_->setOpener(pickerButton_);
        fontPopup_ = new FontPickerPopup([controller = controller_](const QString &family) {
            controller->setCurrentFont(family);
        }, parent);
        fontPopup_->setOpener(fontButton_);
        syncState();
    }

    void syncState()
    {
        const qreal ratio = devicePixelRatioF();
        // The text mode is exclusive: a tool change would drop the recognized
        // layer, so the tools are not offered while it is on.  The Text+ button
        // itself stays enabled and shows the mode is up.
        const bool textMode = controller_->textMode_;
        for (int index = 0; index < toolButtons_.size(); ++index) {
            setToolButtonActive(toolButtons_.at(index), tools_.at(index),
                                tools_.at(index) == controller_->tool_, ratio);
            toolButtons_.at(index)->setEnabled(!textMode);
        }
        if (textButton_ != nullptr) {
            setButtonActive(textButton_, textMode);
        }
        if (longButton_ != nullptr) {
            // The action needs a selection that fits in one output: a scroll
            // container never spans two monitors, and the CLI would refuse the
            // region anyway.  Saying so on the button beats a failure after
            // the fact.
            longButton_->setEnabled(!textMode && controller_->canRequestLongCapture());
        }
        const Annotation *selected = nullptr;
        if (controller_->selectedAnnotation_ >= 0 &&
            controller_->selectedAnnotation_ < controller_->annotations_.size()) {
            selected = &controller_->annotations_[controller_->selectedAnnotation_];
        }
        undo_->setEnabled(!controller_->undoStack_.isEmpty());
        redo_->setEnabled(!controller_->redoStack_.isEmpty());
        undo_->setIcon(historyIcon(false, QColor(QStringLiteral("#dfe4ec")), ratio));
        redo_->setIcon(historyIcon(true, QColor(QStringLiteral("#dfe4ec")), ratio));

        // The style row edits the selected annotation when one is active,
        // otherwise it edits the pending drawing style.
        const QString target = controller_->styleTargetTool();
        const bool shape = target == QStringLiteral("rectangle") ||
            target == QStringLiteral("ellipse");
        // Every tool that paints a stroked shape: the rectangle and ellipse
        // outlines, the arrow, the pen, the two segment tools and the bezier
        // pen.  They share the colour and width controls; the dash is only
        // meaningful to the tools the Rust renderer walks with a dashes
        // pattern -- the wave is sampled as a solid sine and a bezier path is
        // stroked whole, so neither is offered a dash control.
        const bool stroke = shape || target == QStringLiteral("arrow") ||
            target == QStringLiteral("pen") || target == QStringLiteral("line") ||
            target == QStringLiteral("wave") || target == QStringLiteral("bezier");
        const bool text = target == QStringLiteral("text");
        const bool mosaic = target == QStringLiteral("mosaic");
        // A numbered badge is a text annotation by wire but nothing like one on
        // the panel: the number tool shares the colour and width controls, and
        // takes the badge-style segments instead of the font and size boxes.
        const bool number = target == QStringLiteral("number");
        const bool mosaicBrush = mosaic &&
            (selected != nullptr ? selected->kind == Annotation::Kind::Stroke
                                 : controller_->mosaicShape_ == QStringLiteral("brush"));
        const bool selectedMosaicShape = selected != nullptr &&
            selected->kind == Annotation::Kind::Shape && mosaic;
        const bool showColor = stroke || text || number;
        const bool showDash = stroke && target != QStringLiteral("wave") &&
            target != QStringLiteral("bezier");
        const bool showArrowHead = target == QStringLiteral("arrow");
        const bool showWidth = stroke || mosaicBrush || number;
        const bool showTextSize = text;
        const bool showFont = text;
        const bool showArrowSize = target == QStringLiteral("arrow");
        const bool showMosaic = mosaic;
        const bool showStrength = mosaic;
        const bool showNumberStyle = number;
        colorGroup_->setVisible(showColor);
        fontGroup_->setVisible(showFont);
        dashGroup_->setVisible(showDash);
        arrowStyleGroup_->setVisible(showArrowHead);
        widthGroup_->setVisible(showWidth);
        textGroup_->setVisible(showTextSize);
        arrowGroup_->setVisible(showArrowSize);
        mosaicGroup_->setVisible(showMosaic);
        strengthGroup_->setVisible(showStrength);
        numberGroup_->setVisible(showNumberStyle);
        for (int index = 0; index < mosaicButtons_.size(); ++index) {
            const bool brush = mosaicValues_.at(index) == QStringLiteral("brush");
            mosaicButtons_.at(index)->setEnabled(
                selected == nullptr || !mosaic || (selectedMosaicShape ? !brush : brush));
        }

        // The two property rows share one canonical group order. When the
        // visible groups fit within the row width cap they merge into a
        // single line to keep the panel short; the widest combination (the
        // arrow tool) stays wrapped across two rows.
        struct StyleGroup {
            QWidget *widget;
            bool shown;
            bool optionsRow;
        };
        const StyleGroup orderedGroups[] = {
            {colorGroup_, showColor, true},          {fontGroup_, showFont, true},
            {dashGroup_, showDash, true},
            {arrowStyleGroup_, showArrowHead, true}, {mosaicGroup_, showMosaic, true},
            {numberGroup_, showNumberStyle, true},
            {widthGroup_, showWidth, false},         {arrowGroup_, showArrowSize, false},
            {textGroup_, showTextSize, false},       {strengthGroup_, showStrength, false},
        };
        QVector<QWidget *> visibleGroups;
        int visibleWidth = 0;
        for (const StyleGroup &entry : orderedGroups) {
            if (!entry.shown) {
                continue;
            }
            visibleGroups.append(entry.widget);
            visibleWidth += entry.widget->sizeHint().width();
        }
        auto *optionLayout = static_cast<QHBoxLayout *>(optionsRow_->layout());
        auto *numericLayout = static_cast<QHBoxLayout *>(numericRow_->layout());
        constexpr int kStyleRowMaxWidth = 640;
        const int groupCount = visibleGroups.size();
        const int mergedWidth = visibleWidth +
            optionLayout->spacing() * std::max(0, groupCount - 1) +
            optionLayout->contentsMargins().left() + optionLayout->contentsMargins().right() +
            4; // styleRow_ side margins
        const bool singleRow = groupCount > 0 && mergedWidth <= kStyleRowMaxWidth;
        const bool anyOptionsGroup =
            showColor || showFont || showDash || showArrowHead || showMosaic || showNumberStyle;
        const bool anyNumericGroup = showWidth || showArrowSize || showTextSize || showStrength;
        for (const StyleGroup &entry : orderedGroups) {
            QWidget *homeRow = (singleRow || entry.optionsRow) ? optionsRow_ : numericRow_;
            if (entry.widget->parentWidget() == homeRow) {
                continue;
            }
            if (QWidget *previous = entry.widget->parentWidget()) {
                if (QLayout *layout = previous->layout()) {
                    layout->removeWidget(entry.widget);
                }
            }
            (homeRow == optionsRow_ ? optionLayout : numericLayout)->addWidget(entry.widget);
        }
        optionsRow_->setVisible(singleRow ? groupCount > 0 : anyOptionsGroup);
        numericRow_->setVisible(!singleRow && anyNumericGroup);
        styleRow_->setVisible(optionsRow_->isVisibleTo(styleRow_) ||
                              numericRow_->isVisibleTo(styleRow_));
        styleDivider_->setVisible(styleRow_->isVisible());
        if (pickerPopup_ != nullptr && pickerPopup_->isVisible() &&
            !colorGroup_->isVisibleTo(this)) {
            pickerPopup_->hide();
        }
        if (fontPopup_ != nullptr && fontPopup_->isVisible() &&
            !fontGroup_->isVisibleTo(this)) {
            fontPopup_->hide();
        }
        // The visibility toggles above can leave nested size-hint caches
        // stale when a row's hint is unchanged (e.g. swapping two equally
        // tall groups): adjustSize() would then keep a stale panel height.
        optionsRow_->updateGeometry();
        numericRow_->updateGeometry();
        styleRow_->updateGeometry();

        const QColor color = selected != nullptr ? selected->color : controller_->currentColor_;
        const QString dash = selected != nullptr ? selected->dash : controller_->currentDash_;
        const std::uint32_t width = selected != nullptr ? selected->width : controller_->currentWidth_;
        const std::uint32_t size = selected != nullptr ? selected->size : controller_->arrowSize_;
        const QString arrowStyle = selected != nullptr ? selected->arrowStyle
                                                        : controller_->currentArrowStyle_;
        const QString font = selected != nullptr ? selected->font : controller_->currentFont_;
        const std::uint32_t textSize = selected != nullptr ? selected->textPixels : controller_->textSize_;
        const QString mask = selected != nullptr
            ? (selected->kind == Annotation::Kind::Stroke ? QStringLiteral("brush") : selected->mask)
            : controller_->mosaicShape_;
        const std::uint32_t strength =
            selected != nullptr ? selected->strength : controller_->mosaicStrength_;
        const NumberStyle numberStyle =
            selected != nullptr && isNumberAnnotation(*selected) ? selected->numberStyle
                                                                 : controller_->numberStyle_;
        lastColor_ = color;
        // A swatch is "selected" when its RGB matches, whatever the current
        // alpha: the swatches carry no alpha, so a translucent version of a
        // palette colour is still that colour and must keep its ring.
        const bool customColor =
            std::none_of(swatchColors_.cbegin(), swatchColors_.cend(),
                         [&color](const QColor &swatch) { return swatch.rgb() == color.rgb(); });
        pickerButton_->setIcon(colorPickerIcon(customColor, ratio));
        for (int index = 0; index < swatchButtons_.size(); ++index) {
            swatchButtons_.at(index)->setIcon(swatchIcon(
                swatchColors_.at(index), swatchColors_.at(index).rgb() == color.rgb(), ratio));
        }
        syncToggleGroup(dashButtons_, dashValues_, dash);
        fontButton_->setText(font.isEmpty() ? QApplication::font().family() : font);
        syncToggleGroup(arrowStyleButtons_, arrowStyleValues_, arrowStyle);
        syncToggleGroup(mosaicButtons_, mosaicValues_, mask);
        syncToggleGroup(numberButtons_, numberValues_, numberStyleValue(numberStyle));
        // The number tool's button names the style that is armed, the way the
        // standalone toolbar's does; the badge it would place is what the style
        // row shows.
        for (int index = 0; index < tools_.size(); ++index) {
            if (tools_.at(index) == Tool::Number) {
                toolButtons_.at(index)->setToolTip(
                    uiTr("Number: %1 (click to place a number)")
                        .arg(numberStyleName(numberStyle)));
            }
        }
        {
            const QSignalBlocker widthBlocker(widthSlider_);
            const QSignalBlocker arrowBlocker(arrowSlider_);
            const QSignalBlocker textBlocker(textSpin_);
            const QSignalBlocker strengthBlocker(strengthSlider_);
            widthSlider_->setValue(static_cast<int>(std::clamp(width, 1u, 64u)));
            arrowSlider_->setValue(static_cast<int>(std::clamp(size, 1u, 8u)));
            textSpin_->setValue(clampTextPixels(static_cast<int>(textSize)));
            strengthSlider_->setValue(static_cast<int>(std::clamp(strength, 1u, 3u)));
        }
        widthLabel_->setText(uiTr("Width %1").arg(widthSlider_->value()));
        arrowLabel_->setText(uiTr("Arrow %1").arg(arrowSlider_->value()));
        strengthLabel_->setText(uiTr("Mosaic %1").arg(strengthSlider_->value()));
        // While a label is being typed the size box must not take the keyboard:
        // clicking it would blur the editor the user is typing in.  A spin box
        // defaults to WheelFocus, so it is the one control on this bar whose
        // policy is flipped rather than set once.  With no editor open it keeps
        // the normal policy, so the value stays typeable.
        textSpin_->setFocusPolicy(controller_->textEdit_ != nullptr ? Qt::NoFocus
                                                                    : Qt::WheelFocus);
        layout()->activate();
        adjustSize();
    }

    // Flips the style sub-panel to the far side of the command bar: above it
    // when the panel sits above the selection, below when it sits underneath.
    void setStyleRowAbove(bool above)
    {
        styleRowAbove_ = above;
        auto *box = qobject_cast<QBoxLayout *>(layout());
        if (box != nullptr) {
            box->setDirection(above ? QBoxLayout::BottomToTop : QBoxLayout::TopToBottom);
            // Settle the row order now: the controller reads the command bar's
            // size right after the flip, and may only realize the geometry on
            // the next event-loop pass otherwise.
            box->activate();
        }
    }

    bool styleRowAbove() const { return styleRowAbove_; }

    // Height of the command bar itself (the tool row, without the style row).
    // The controller pins this row to the selection, so it needs the size on
    // its own rather than the whole panel's.
    int commandBarHeight() const
    {
        return commandSurface_ != nullptr ? commandSurface_->height() : 0;
    }

    // Distance from the panel's top edge to its first row.  Constant, but read
    // off the live layout so a theme change to the padding stays correct.
    int panelTopPadding() const { return layout()->contentsMargins().top(); }

    // Height the style row adds to the panel while it is shown, and 0 while it
    // is hidden.  The controller uses it to decide whether the row still fits
    // beyond the command bar or has to double back over the selection.
    int styleRowExtra() const
    {
        const QMargins margins = layout()->contentsMargins();
        return std::max(0, height() - commandBarHeight() - margins.top() - margins.bottom());
    }

protected:
    bool event(QEvent *event) override
    {
        if (event->type() == QEvent::ParentChange && pickerPopup_ != nullptr) {
            // The toolbar is re-parented when the selection moves to another
            // output; the popup must tag along.
            pickerPopup_->setParent(parentWidget());
            if (fontPopup_ != nullptr) {
                fontPopup_->setParent(parentWidget());
            }
        }
        return QWidget::event(event);
    }

    void hideEvent(QHideEvent *event) override
    {
        if (pickerPopup_ != nullptr) {
            pickerPopup_->hide();
        }
        if (fontPopup_ != nullptr) {
            fontPopup_->hide();
        }
        QWidget::hideEvent(event);
    }

    // Paints the frosted-glass superellipse surface: the frozen frame behind
    // the panel is sampled, blurred, tinted and clipped to the squircle.
    void paintEvent(QPaintEvent *event) override
    {
        Q_UNUSED(event);
        QPainter painter(this);
        painter.setRenderHint(QPainter::Antialiasing, true);
        const QPainterPath shape = superellipsePath(QRectF(rect()), 26.0, 5.0);
        if (parentWidget() != backdropOwner_ || geometry() != backdropGeometry_) {
            backdropOwner_ = parentWidget();
            backdropGeometry_ = geometry();
            backdrop_ = frostedBackdrop(toolbarFrame(), backdropDeviceRect());
        }
        if (!backdrop_.isNull()) {
            painter.save();
            painter.setClipPath(shape);
            painter.setRenderHint(QPainter::SmoothPixmapTransform, true);
            painter.drawImage(rect(), backdrop_);
            painter.restore();
        }
        painter.setPen(Qt::NoPen);
        painter.setBrush(QColor(30, 34, 41, 204));
        painter.drawPath(shape);
        painter.setBrush(Qt::NoBrush);
        painter.setPen(QPen(QColor(64, 71, 82), 1.0));
        painter.drawPath(superellipsePath(QRectF(rect()).adjusted(0.5, 0.5, -0.5, -0.5),
                                          25.5, 5.0));
    }

    void mousePressEvent(QMouseEvent *event) override
    {
        // Empty panel areas act as a grip: dragging detaches the panel from
        // its automatic selection-following position.
        if (event->button() == Qt::LeftButton &&
            childAt(event->position().toPoint()) == nullptr) {
            dragging_ = true;
            dragOffset_ = event->globalPosition().toPoint() - mapToGlobal(QPoint(0, 0));
            controller_->notifyPanelDragged();
            event->accept();
            return;
        }
        QWidget::mousePressEvent(event);
    }

    void mouseMoveEvent(QMouseEvent *event) override
    {
        if (dragging_ && (event->buttons() & Qt::LeftButton) && parentWidget() != nullptr) {
            const QPoint target = event->globalPosition().toPoint() - dragOffset_;
            const QPoint local = parentWidget()->mapFromGlobal(target);
            const int x = std::clamp(local.x(), 0,
                                     std::max(0, parentWidget()->width() - width()));
            const int y = std::clamp(local.y(), 0,
                                     std::max(0, parentWidget()->height() - height()));
            move(x, y);
            event->accept();
            return;
        }
        QWidget::mouseMoveEvent(event);
    }

    void mouseReleaseEvent(QMouseEvent *event) override
    {
        if (dragging_) {
            dragging_ = false;
            // Re-parenting mid-drag would drop the implicit mouse grab, so the
            // panel only hops to another output when the drag settles.
            controller_->settlePanelAtGlobal(event->globalPosition().toPoint() - dragOffset_);
            event->accept();
            return;
        }
        QWidget::mouseReleaseEvent(event);
    }

private:
    QImage toolbarFrame() const
    {
        const QWidget *owner = parentWidget();
        if (owner == nullptr) {
            return {};
        }
        for (int index = 0; index < controller_->overlays_.size(); ++index) {
            if (controller_->overlays_.at(index) == owner) {
                if (index >= controller_->session_.outputs.size()) {
                    return {};
                }
                return controller_->session_.outputs.at(index).image;
            }
        }
        return {};
    }

    // The panel's rect in image pixels, or an empty rect when the panel is not
    // fully covered by the pinned/frozen image. Pin editing puts the toolbar on
    // the bare canvas beside the image, where there is nothing to sample: the
    // frosted glass would otherwise smear a stretched crop across the panel.
    QRect backdropDeviceRect() const
    {
        const QWidget *owner = parentWidget();
        const QImage frame = toolbarFrame();
        if (owner == nullptr || frame.isNull() || owner->width() <= 0 ||
            owner->height() <= 0) {
            return {};
        }
        const OutputSession *output = nullptr;
        for (int index = 0; index < controller_->overlays_.size(); ++index) {
            if (controller_->overlays_.at(index) == owner &&
                index < controller_->session_.outputs.size()) {
                output = &controller_->session_.outputs.at(index);
                break;
            }
        }
        if (output == nullptr) {
            return {};
        }
        const LogicalRect &surface = surfaceOf(*output);
        const double sx = static_cast<double>(output->scale);
        const double logicalToLocal =
            static_cast<double>(owner->width()) / static_cast<double>(surface.width);
        const double logicalToLocalY =
            static_cast<double>(owner->height()) / static_cast<double>(surface.height);
        if (logicalToLocal <= 0 || logicalToLocalY <= 0) {
            return {};
        }
        // Panel local rect -> global logical -> image pixels.
        const double left = surface.x + x() / logicalToLocal;
        const double top = surface.y + y() / logicalToLocalY;
        const QRect panel(static_cast<int>(std::round(left)),
                          static_cast<int>(std::round(top)),
                          static_cast<int>(std::round(width() / logicalToLocal)),
                          static_cast<int>(std::round(height() / logicalToLocalY)));
        const LogicalRect &geometry = output->geometry;
        const QRect globalImage(geometry.x, geometry.y, static_cast<int>(geometry.width),
                                static_cast<int>(geometry.height));
        if (!globalImage.contains(panel)) {
            return {};
        }
        const QRect device(
            static_cast<int>(std::round((panel.x() - geometry.x) * sx)),
            static_cast<int>(std::round((panel.y() - geometry.y) * sx)),
            static_cast<int>(std::round(panel.width() * sx)),
            static_cast<int>(std::round(panel.height() * sx)));
        return device.intersected(frame.rect());
    }

    static void setButtonActive(QAbstractButton *button, bool active)
    {
        if (button->property("active").toBool() == active) {
            return;
        }
        button->setProperty("active", active);
        button->style()->unpolish(button);
        button->style()->polish(button);
        button->update();
        // SegmentFrame paints the active fill for its child buttons.
        if (QWidget *parent = button->parentWidget()) {
            parent->update();
        }
    }

    static void setToolButtonActive(QAbstractButton *button, Tool tool, bool active, qreal ratio)
    {
        setButtonActive(button, active);
        if (auto *toolButton = qobject_cast<QToolButton *>(button)) {
            toolButton->setIcon(toolbarIcon(
                tool, active ? QColor(QStringLiteral("#12243d"))
                             : QColor(QStringLiteral("#dfe3ea")),
                ratio));
        }
    }

    template<typename Apply>
    void addStyleButtons(QWidget *group, const QStringList &labels, const QString &tooltip,
                         const QStringList &values, QVector<QPushButton *> *buttons,
                         QVector<QString> *storedValues, Apply apply)
    {
        const int segmentCount = labels.size();
        for (int index = 0; index < segmentCount; ++index) {
            auto *button = new QPushButton(labels.at(index), group);
            button->setProperty("segment", true);
            button->setProperty("segmentPosition", index == 0
                                                         ? QStringLiteral("first")
                                                         : index == segmentCount - 1
                                                             ? QStringLiteral("last")
                                                             : QStringLiteral("middle"));
            button->setSizePolicy(QSizePolicy::Fixed, QSizePolicy::Fixed);
            button->setCursor(Qt::PointingHandCursor);
            button->setFocusPolicy(Qt::NoFocus);
            button->setToolTip(tooltip);
            button->setAccessibleName(QStringLiteral("%1: %2").arg(tooltip, labels.at(index)));
            group->layout()->addWidget(button);
            if (group->property("segmentFrame").toBool()) {
                // Only SegmentFrame groups carry this property.
                static_cast<SegmentFrame *>(group)->trackButton(button);
            }
            buttons->push_back(button);
            storedValues->push_back(values.at(index));
            const QString value = values.at(index);
            connect(button, &QPushButton::clicked, [apply, value] { apply(value); });
        }
    }

    template<typename Values>
    void syncToggleGroup(const QVector<QPushButton *> &buttons, const Values &values,
                         const QString &current)
    {
        for (int index = 0; index < buttons.size(); ++index) {
            setButtonActive(buttons.at(index), values.at(index) == current);
        }
    }

    // Opens or closes the custom color picker next to the swatch row, on the
    // same side the style sub-panel expanded (away from the selection).
    void togglePickerPopup()
    {
        if (pickerPopup_->isVisible()) {
            pickerPopup_->hide();
            return;
        }
        pickerPopup_->adjustSize();
        QWidget *overlay = parentWidget();
        if (overlay == nullptr) {
            return;
        }
        // Overlay coordinates: the popup is a child of the overlay so it can
        // extend beyond the toolbar's clipped rect.
        QPoint target = pos() + pickerButton_->mapTo(this, QPoint(0, 0));
        target.rx() -= (pickerPopup_->width() - pickerButton_->width()) / 2;
        constexpr int gap = 4;
        const int above = y() - pickerPopup_->height() - gap;
        const int below = y() + height() + gap;
        int placedY = styleRowAbove_ ? above : below;
        const auto fits = [this, overlay](int value) {
            return value >= 0 && value + pickerPopup_->height() <= overlay->height();
        };
        // Prefer the side the style sub-panel expanded to; fall back to the
        // opposite side when the popup would leave the output.
        if (!fits(placedY)) {
            placedY = fits(above) ? above : below;
        }
        target.ry() = placedY;
        target.rx() = std::clamp(target.x(), 0,
                                 std::max(0, overlay->width() - pickerPopup_->width()));
        target.ry() = std::clamp(target.y(), 0,
                                 std::max(0, overlay->height() - pickerPopup_->height()));
        pickerPopup_->openAt(lastColor_, target);
    }

    void toggleFontPopup()
    {
        if (fontPopup_->isVisible()) {
            fontPopup_->hide();
            return;
        }
        fontPopup_->adjustSize();
        QWidget *overlay = parentWidget();
        if (overlay == nullptr) {
            return;
        }
        QPoint target = pos() + fontButton_->mapTo(this, QPoint(0, 0));
        target.rx() -= (fontPopup_->width() - fontButton_->width()) / 2;
        constexpr int gap = 4;
        const int above = y() - fontPopup_->height() - gap;
        const int below = y() + height() + gap;
        int placedY = styleRowAbove_ ? above : below;
        const auto fits = [this, overlay](int value) {
            return value >= 0 && value + fontPopup_->height() <= overlay->height();
        };
        if (!fits(placedY)) {
            placedY = fits(above) ? above : below;
        }
        target.ry() = std::clamp(placedY, 0,
                                 std::max(0, overlay->height() - fontPopup_->height()));
        target.rx() = std::clamp(target.x(), 0,
                                 std::max(0, overlay->width() - fontPopup_->width()));
        fontPopup_->openAt(fontButton_->text(), target);
    }


    QWidget *addGroup(QHBoxLayout *row, bool framed = false)
    {
        QWidget *group = framed ? new SegmentFrame(this) : new QWidget(this);
        group->setProperty("toolbarGroup", true);
        group->setCursor(Qt::ArrowCursor);
        if (framed) {
            group->setProperty("segmentFrame", true);
        }
        group->setSizePolicy(QSizePolicy::Fixed, QSizePolicy::Fixed);
        auto *layout = new QHBoxLayout(group);
        layout->setContentsMargins(2, 2, 2, 2);
        layout->setSpacing(0);
        row->addWidget(group);
        return group;
    }

    QPushButton *addActionButton(QHBoxLayout *layout, const QString &label)
    {
        auto *button = new QPushButton(label, this);
        button->setSizePolicy(QSizePolicy::Fixed, QSizePolicy::Fixed);
        button->setCursor(Qt::PointingHandCursor);
        button->setFocusPolicy(Qt::NoFocus);
        button->setAccessibleName(label);
        layout->addWidget(button);
        return button;
    }

    void addTool(QHBoxLayout *layout, const QString &label, Tool tool)
    {
        auto *button = new QToolButton(this);
        button->setProperty("toolButton", true);
        button->setToolButtonStyle(Qt::ToolButtonTextUnderIcon);
        button->setText(label);
        button->setIcon(toolbarIcon(tool, QColor(230, 225, 229), devicePixelRatioF()));
        button->setIconSize(QSize(20, 20));
        button->setFixedSize(kToolButtonWidth, kToolButtonHeight);
        button->setCursor(Qt::PointingHandCursor);
        button->setFocusPolicy(Qt::NoFocus);
        button->setToolTip(toolTipForTool(tool));
        button->setAccessibleName(uiTr("Tool: %1").arg(label));
        layout->addWidget(button);
        tools_.push_back(tool);
        toolButtons_.push_back(button);
        connect(button, &QToolButton::clicked, [controller = controller_, tool] {
            controller->chooseTool(tool);
        });
    }

    static QString toolTipForTool(Tool tool)
    {
        switch (tool) {
        case Tool::Select:
            return uiTr(
                "Adjust selection; click an annotation to select, drag to move, "
                "handles to resize, double-click text to re-edit");
        case Tool::Rectangle:
            return uiTr("Draw a rectangular annotation");
        case Tool::Ellipse:
            return uiTr("Draw an elliptical annotation");
        case Tool::Arrow:
            return uiTr("Draw an arrow with an adjustable head");
        case Tool::Line:
            return uiTr("Draw a straight line");
        case Tool::Wave:
            return uiTr("Draw a wavy line");
        case Tool::Bezier:
            return uiTr("Draw a curved path: click to add an anchor, drag to bend the "
                        "curve, click the first anchor to close and fill it, "
                        "double-click to finish it open");
        case Tool::Pen:
            return uiTr("Draw a freehand line");
        case Tool::Text:
            return uiTr("Click to place a text label, click text to re-edit");
        case Tool::Number:
            return uiTr("Click to place a number; each click counts up from one");
        case Tool::Mosaic:
            return uiTr("Pixelate an area: rectangle, ellipse or freehand brush");
        }
        return QString();
    }

    // The width a button needs to draw every one of `labels` in full.
    //
    // A tool button elides its text to the box it was given, so one sized for
    // the label it opens with cuts a longer one off.  That is what the text
    // button did: it is built for "Text+" and then flashes "Copied" in the same
    // box, and on a font that draws the word any wider than this one it comes
    // out cut off.  Asking the style for each label's own size hint is asking it
    // the same question it elides against, so the answer holds for whatever
    // font and style are in force.  The tool buttons' own width is the floor, so
    // a narrower label cannot shrink the row out of shape.
    //
    // This has to run before the width is fixed: a button at a fixed width
    // reports that width back from `sizeHint`, so measuring afterwards would
    // only ever confirm the width it already had.
    static int widthForLabels(QToolButton *button, const QStringList &labels)
    {
        button->ensurePolished();
        const QString shown = button->text();
        int widest = kToolButtonWidth;
        for (const QString &label : labels) {
            button->setText(label);
            widest = std::max(widest, button->sizeHint().width());
        }
        button->setText(shown);
        return widest;
    }

    // The size every button in the tool row is built at, whether it enters a
    // mode or does one thing.  A button that has to flash a longer label is
    // widened past this; see `widthForLabels`.
    static constexpr int kToolButtonWidth = 48;
    static constexpr int kToolButtonHeight = 46;

    // A button in the tool row that does one thing instead of entering a mode,
    // laid out exactly like the tool buttons: same size, same text-under-icon
    // shape, same hover and press painting from ToolCardFrame.  Kept out of
    // `toolButtons_` because nothing stays selected: a paste and a text read
    // happen and are over.
    //
    // `alsoShows` names every label the button will show after `label`, so the
    // box is wide enough for all of them from the start.  A button that changes
    // its text is the one case where the size cannot be a constant.
    QToolButton *addToolAction(QHBoxLayout *layout, const QString &label, const QIcon &icon,
                               const QString &tooltip, const QString &objectName,
                               const QStringList &alsoShows = QStringList())
    {
        auto *button = new QToolButton(this);
        button->setProperty("toolButton", true);
        button->setToolButtonStyle(Qt::ToolButtonTextUnderIcon);
        button->setText(label);
        button->setIcon(icon);
        button->setIconSize(QSize(20, 20));
        button->setCursor(Qt::PointingHandCursor);
        button->setFocusPolicy(Qt::NoFocus);
        button->setToolTip(tooltip);
        button->setAccessibleName(label);
        button->setObjectName(objectName);
        QStringList all;
        all << label << alsoShows;
        button->setFixedSize(widthForLabels(button, all), kToolButtonHeight);
        layout->addWidget(button);
        return button;
    }

    OverlayController *controller_;
    QWidget *commandSurface_ = nullptr;
    QWidget *styleRow_ = nullptr;
    QFrame *styleDivider_ = nullptr;
    QVector<QAbstractButton *> toolButtons_;
    QVector<Tool> tools_;
    // The Text+ button, whose label reports what a text selection did.  The
    // result can come from a key the toolbar never sees, so it is stored rather
    // than reached through the click handler's capture.
    QToolButton *textButton_ = nullptr;
    // The scrolling-capture button, enabled only when the session offers the
    // action and the selection fits in one output; its state is set by
    // `syncState` along with every other button's.
    QToolButton *longButton_ = nullptr;
    QVector<QColor> swatchColors_;
    QVector<QPushButton *> swatchButtons_;
    QWidget *colorGroup_ = nullptr;
    QPushButton *pickerButton_ = nullptr;
    ColorPickerPopup *pickerPopup_ = nullptr;
    QWidget *fontGroup_ = nullptr;
    QPushButton *fontButton_ = nullptr;
    FontPickerPopup *fontPopup_ = nullptr;
    QColor lastColor_;
    bool styleRowAbove_ = false;
    QWidget *dashGroup_ = nullptr;
    QWidget *widthGroup_ = nullptr;
    QWidget *arrowGroup_ = nullptr;
    QWidget *arrowStyleGroup_ = nullptr;
    QWidget *textGroup_ = nullptr;
    QWidget *mosaicGroup_ = nullptr;
    QWidget *strengthGroup_ = nullptr;
    QWidget *numberGroup_ = nullptr;
    QWidget *optionsRow_ = nullptr;
    QWidget *numericRow_ = nullptr;
    QVector<QPushButton *> dashButtons_;
    QVector<QString> dashValues_;
    QVector<QPushButton *> mosaicButtons_;
    QVector<QString> mosaicValues_;
    QVector<QPushButton *> numberButtons_;
    QVector<QString> numberValues_;
    QVector<QPushButton *> arrowStyleButtons_;
    QVector<QString> arrowStyleValues_;
    QSlider *widthSlider_ = nullptr;
    QLabel *widthLabel_ = nullptr;
    QSlider *arrowSlider_ = nullptr;
    QLabel *arrowLabel_ = nullptr;
    QSpinBox *textSpin_ = nullptr;
    QLabel *textLabel_ = nullptr;
    QSlider *strengthSlider_ = nullptr;
    QLabel *strengthLabel_ = nullptr;
    QPushButton *undo_ = nullptr;
    QPushButton *redo_ = nullptr;
    bool dragging_ = false;
    QPoint dragOffset_;
    QImage backdrop_;
    QRect backdropGeometry_;
    const QWidget *backdropOwner_ = nullptr;
};

class OverlayController::InlineTextEdit final : public QLineEdit {
public:
    using Finished = std::function<void(bool)>;

    explicit InlineTextEdit(QWidget *parent, Finished finished)
        : QLineEdit(parent)
        , finished_(std::move(finished))
    {
        setAttribute(Qt::WA_DeleteOnClose, false);
        setFrame(true);
        setPlaceholderText(uiTr("Text"));
        // No input validator: the label is rasterized to a bitmap by Qt (with
        // fontconfig fallback) and composited by Rust, so any Unicode text —
        // including CJK typed through an input method — is supported. An ASCII
        // validator here would silently drop IME commits.
    }

protected:
    void keyPressEvent(QKeyEvent *event) override
    {
        if (event->key() == Qt::Key_Return || event->key() == Qt::Key_Enter) {
            if (finished_) {
                finished_(true);
            }
            event->accept();
            return;
        }
        if (event->key() == Qt::Key_Escape) {
            if (finished_) {
                finished_(false);
            }
            event->accept();
            return;
        }
        QLineEdit::keyPressEvent(event);
    }

private:
    Finished finished_;
};

OverlayController::OverlayController(Session session)
    : session_(std::move(session))
    , gesture_(new Gesture)
{
    // Whether the toolbar offers the scrolling-capture action.  Read from
    // `session_` rather than the parameter: the parameter has already been
    // moved from by the time this body runs.
    longAllowed_ = session_.longAllowed;
    // Window picking is driven by the session's candidate list instead of a
    // free-hand drag: the pointer highlights a candidate and a click takes it.
    if (session_.mode == QStringLiteral("window-pick")) {
        pickMode_ = true;
        candidates_ = session_.candidates;
    }
    // Scrolling capture wants a rectangle, not an editor: the pixels it will
    // annotate only exist once the page has been scrolled and stitched.
    selectOnly_ = session_.mode == QStringLiteral("region-only");
    // The style the user last left the editor in.
    const EditorPreferences preferences = loadEditorPreferences();
    currentColor_ = preferences.color;
    currentFont_ = preferences.font;
    currentWidth_ = preferences.width;
    textSize_ = preferences.textSize;
    currentDash_ = preferences.dash;
    arrowSize_ = preferences.arrowSize;
    currentArrowStyle_ = preferences.arrowStyle;
    mosaicShape_ = preferences.mosaicShape;
    mosaicStrength_ = preferences.mosaicStrength;
    // The remembered tool is restored only where a tool is already meaningful:
    // a session that starts in editing state -- one that arrives with its
    // selection made (`beginPresetEdit`, the window picker's follow-up) and the
    // pin editor, whose whole image is preselected in `beginPinEdit`.  A fresh
    // region session must open on Select no matter what the file says -- its
    // first step is dragging the rectangle, and opening on Text means the first
    // click starts a label instead, which reads as "region capture is broken".
    // Scrolling capture (`selectOnly_`) and picking (`pickMode_`) have their
    // own reasons to stay on Select either way.
    const bool startsInEdit =
        session_.selection.has_value() || session_.mode == QStringLiteral("pin-edit");
    if (startsInEdit && !selectOnly_ && !pickMode_) {
        const Tool remembered = toolForName(preferences.tool);
        if (remembered != Tool::Select) {
            tool_ = remembered;
        }
    }
}

OverlayController::~OverlayController()
{
    removeTextEditor();
    delete toolbar_;
    delete gesture_;
    // Explicit rather than parented: the controller is not a QObject.
    delete pinSocket_;
    delete candidateReader_;
    delete candidateTimer_;
}

int OverlayController::outputCount() const
{
    return session_.outputs.size();
}

const Session &OverlayController::session() const
{
    return session_;
}

CaptureOverlay *OverlayController::addOverlay(int outputIndex, QScreen *screen, QString *error)
{
    if (outputIndex < 0 || outputIndex >= session_.outputs.size() || screen == nullptr) {
        if (error != nullptr) {
            *error = QStringLiteral("invalid screen/output while creating overlay");
        }
        return nullptr;
    }
    auto *overlay = new CaptureOverlay(outputIndex, this, screen);
    overlays_.push_back(overlay);
    if (toolbar_ == nullptr) {
        toolbarOutput_ = outputIndex;
    }
    return overlay;
}

Point OverlayController::globalPoint(CaptureOverlay *overlay, const QPointF &local) const
{
    return clampPoint(unclampedGlobalPoint(overlay, local));
}

// Same conversion as globalPoint() but without clamping into the session
// bounds: the pin editor needs to tell a click on the pinned image (inside the
// bounds) from one on the surrounding canvas (outside them).
Point OverlayController::unclampedGlobalPoint(CaptureOverlay *overlay,
                                              const QPointF &local) const
{
    const OutputSession &output = overlay->output();
    const LogicalRect &surface = surfaceOf(output);
    const double sx = overlay->width() > 0
        ? static_cast<double>(surface.width) / static_cast<double>(overlay->width())
        : 1.0;
    const double sy = overlay->height() > 0
        ? static_cast<double>(surface.height) / static_cast<double>(overlay->height())
        : 1.0;
    const auto x = static_cast<std::int64_t>(std::floor(surface.x + local.x() * sx));
    const auto y = static_cast<std::int64_t>(std::floor(surface.y + local.y() * sy));
    return Point{static_cast<std::int32_t>(std::clamp<std::int64_t>(
                      x, std::numeric_limits<std::int32_t>::min(),
                      std::numeric_limits<std::int32_t>::max())),
                 static_cast<std::int32_t>(std::clamp<std::int64_t>(
                     y, std::numeric_limits<std::int32_t>::min(),
                     std::numeric_limits<std::int32_t>::max()))};
}

Point OverlayController::clampPoint(Point point) const
{
    const LogicalRect &limits = annotationLimits();
    const std::int64_t x = std::clamp<std::int64_t>(point.x, limits.x, limits.right() - 1);
    const std::int64_t y = std::clamp<std::int64_t>(point.y, limits.y, limits.bottom() - 1);
    return Point{static_cast<std::int32_t>(x), static_cast<std::int32_t>(y)};
}

// Area the tool may paint on. Region capture paints over the whole session;
// the pin editor confines drawing to the pinned image, which the user can drag
// around the surrounding canvas.
const LogicalRect &OverlayController::annotationLimits() const
{
    if (pinEdit_ && selection_.has_value()) {
        return *selection_;
    }
    return session_.bounds;
}

// Area the selection itself may occupy. Region capture keeps it inside the
// frozen scene; in the pin editor the image may roam over the whole output,
// which is exactly the surface the overlay covers.
LogicalRect OverlayController::selectionLimits() const
{
    if (pinEdit_ && !session_.outputs.isEmpty()) {
        return surfaceOf(session_.outputs.constFirst());
    }
    return session_.bounds;
}

// Shifts every annotation by the given global delta (the image under them
// moved). Deliberately unclamped: the image and its marks travel as one rigid
// body, so a mark sitting on the image's edge must move with it rather than
// being pinned back to the edge it started on.
void OverlayController::translateAnnotations(std::int32_t dx, std::int32_t dy)
{
    if (dx == 0 && dy == 0) {
        return;
    }
    for (Annotation &annotation : annotations_) {
        switch (annotation.kind) {
        case Annotation::Kind::Shape:
        case Annotation::Kind::Image:
            annotation.rect.x = static_cast<std::int32_t>(annotation.rect.x + dx);
            annotation.rect.y = static_cast<std::int32_t>(annotation.rect.y + dy);
            break;
        case Annotation::Kind::Stroke:
            for (Point &point : annotation.points) {
                point.x = static_cast<std::int32_t>(point.x + dx);
                point.y = static_cast<std::int32_t>(point.y + dy);
            }
            break;
        case Annotation::Kind::Text:
            annotation.origin.x = static_cast<std::int32_t>(annotation.origin.x + dx);
            annotation.origin.y = static_cast<std::int32_t>(annotation.origin.y + dy);
            if (isNumberAnnotation(annotation)) {
                // A badge's box travels with it: the hit test and the raster
                // bounds are read from it, so leaving it behind would strand the
                // badge where the image used to be.
                annotation.rect.x = static_cast<std::int32_t>(annotation.rect.x + dx);
                annotation.rect.y = static_cast<std::int32_t>(annotation.rect.y + dy);
            }
            break;
        }
    }
}

LogicalRect OverlayController::selectionBetween(Point first, Point second) const
{
    const std::int64_t left = std::min(first.x, second.x);
    const std::int64_t top = std::min(first.y, second.y);
    const std::int64_t rightEdge = std::max(first.x, second.x) + 1;
    const std::int64_t bottomEdge = std::max(first.y, second.y) + 1;
    LogicalRect candidate = rectFromEdges(left, top, rightEdge, bottomEdge);
    LogicalRect result;
    if (intersection(candidate, annotationLimits(), &result)) {
        return result;
    }
    return LogicalRect{};
}

LogicalRect OverlayController::moveSelection(LogicalRect origin, Point anchor, Point current) const
{
    const std::int64_t dx = static_cast<std::int64_t>(current.x) - anchor.x;
    const std::int64_t dy = static_cast<std::int64_t>(current.y) - anchor.y;
    const LogicalRect &limits = selectionLimits();
    const std::int64_t minX = limits.x;
    const std::int64_t minY = limits.y;
    const std::int64_t maxX = std::max<std::int64_t>(limits.right() - origin.width, minX);
    const std::int64_t maxY = std::max<std::int64_t>(limits.bottom() - origin.height, minY);
    const std::int64_t x = std::clamp<std::int64_t>(origin.x + dx, minX, maxX);
    const std::int64_t y = std::clamp<std::int64_t>(origin.y + dy, minY, maxY);
    return rectFromEdges(x, y, x + origin.width, y + origin.height);
}

LogicalRect OverlayController::resizeSelection(LogicalRect origin, int handle, Point current) const
{
    // Region capture resizes the selection inside the frozen scene; in the pin
    // editor the same helper resizes a mark inside the image (which may have
    // been dragged), so the limit follows the drawing area, not the scene.
    const LogicalRect &limits = annotationLimits();
    const std::int64_t boundsLeft = limits.x;
    const std::int64_t boundsTop = limits.y;
    const std::int64_t boundsRight = limits.right();
    const std::int64_t boundsBottom = limits.bottom();
    std::int64_t left = origin.x;
    std::int64_t top = origin.y;
    std::int64_t rightEdge = origin.right();
    std::int64_t bottomEdge = origin.bottom();
    const std::int64_t x = current.x;
    const std::int64_t y = current.y;
    switch (handle) {
    case 1:
        left = std::clamp<std::int64_t>(x, boundsLeft, rightEdge - 1);
        top = std::clamp<std::int64_t>(y, boundsTop, bottomEdge - 1);
        break;
    case 2:
        top = std::clamp<std::int64_t>(y, boundsTop, bottomEdge - 1);
        break;
    case 3:
        rightEdge = std::clamp<std::int64_t>(x + 1, left + 1, boundsRight);
        top = std::clamp<std::int64_t>(y, boundsTop, bottomEdge - 1);
        break;
    case 4:
        rightEdge = std::clamp<std::int64_t>(x + 1, left + 1, boundsRight);
        break;
    case 5:
        rightEdge = std::clamp<std::int64_t>(x + 1, left + 1, boundsRight);
        bottomEdge = std::clamp<std::int64_t>(y + 1, top + 1, boundsBottom);
        break;
    case 6:
        bottomEdge = std::clamp<std::int64_t>(y + 1, top + 1, boundsBottom);
        break;
    case 7:
        left = std::clamp<std::int64_t>(x, boundsLeft, rightEdge - 1);
        bottomEdge = std::clamp<std::int64_t>(y + 1, top + 1, boundsBottom);
        break;
    case 8:
        left = std::clamp<std::int64_t>(x, boundsLeft, rightEdge - 1);
        break;
    default:
        break;
    }
    return rectFromEdges(left, top, rightEdge, bottomEdge);
}

int OverlayController::hitHandle(Point point) const
{
    if (!selection_.has_value()) {
        return 0;
    }
    const LogicalRect &selection = *selection_;
    const std::int64_t left = selection.x;
    const std::int64_t top = selection.y;
    const std::int64_t rightEdge = selection.right() - 1;
    const std::int64_t bottomEdge = selection.bottom() - 1;
    const auto close = [](std::int64_t first, std::int64_t second) {
        return std::abs(first - second) <= kHandleRadius;
    };
    const bool nearLeft = close(point.x, left);
    const bool nearRight = close(point.x, rightEdge);
    const bool nearTop = close(point.y, top);
    const bool nearBottom = close(point.y, bottomEdge);
    if (nearLeft && nearTop) {
        return 1;
    }
    if (nearTop && close(point.x, (left + rightEdge) / 2)) {
        return 2;
    }
    if (nearRight && nearTop) {
        return 3;
    }
    if (nearRight && close(point.y, (top + bottomEdge) / 2)) {
        return 4;
    }
    if (nearRight && nearBottom) {
        return 5;
    }
    if (nearBottom && close(point.x, (left + rightEdge) / 2)) {
        return 6;
    }
    if (nearLeft && nearBottom) {
        return 7;
    }
    if (nearLeft && close(point.y, (top + bottomEdge) / 2)) {
        return 8;
    }
    if (selection.x <= point.x && point.x < selection.right() && selection.y <= point.y &&
        point.y < selection.bottom()) {
        return 9;
    }
    return 0;
}

/// The candidate window on top at `point`, or -1.  The session's list arrives
/// in stacking order, bottom to top, so the *last* window containing the point
/// is the one on top — which is what the user means by pointing at that spot.
/// The rule is not "the smallest one": a floating window sitting on a tiled one
/// is usually the smaller of the two but not always, and picking the smaller
/// one there would hand back the window underneath.
int OverlayController::candidateIndexAt(Point point) const
{
    int best = -1;
    for (int index = 0; index < candidates_.size(); ++index) {
        const LogicalRect &rect = candidates_.at(index).rect;
        if (point.x < rect.x || point.y < rect.y || point.x >= rect.right() ||
            point.y >= rect.bottom()) {
            continue;
        }
        best = index;
    }
    return best;
}

/// What the size pill reads while a candidate is hovered: the window's label
/// when it has one, always followed by the size the capture would have.
QString OverlayController::candidatePillText() const
{
    const QString dimensions =
        QStringLiteral("%1 × %2").arg(selection_->width).arg(selection_->height);
    if (hoveredCandidate_ < 0 || hoveredCandidate_ >= candidates_.size()) {
        return dimensions;
    }
    const QString label = candidates_.at(hoveredCandidate_).label.trimmed();
    if (label.isEmpty()) {
        return dimensions;
    }
    // The pill is single-line and drawn at the window's corner, so a long
    // title is elided rather than pushed across the screen.
    const QFontMetrics metrics(pillFont());
    const QString elided = metrics.elidedText(label, Qt::ElideRight, kPickerLabelWidth);
    return QStringLiteral("%1  %2").arg(elided, dimensions);
}

/// Moves the hover highlight to the candidate under the pointer.  Returns true
/// when the highlight actually changed, so the caller can skip the repaint.
bool OverlayController::applyCandidateHover(Point point, CaptureOverlay *overlay)
{
    const int index = candidateIndexAt(point);
    overlay->setCursor(index >= 0 ? Qt::PointingHandCursor : Qt::ArrowCursor);
    if (index == hoveredCandidate_ && selection_.has_value() == (index >= 0)) {
        return false;
    }
    hoveredCandidate_ = index;
    if (index >= 0) {
        selection_ = candidates_.at(index).rect;
    } else {
        selection_.reset();
    }
    return true;
}

void OverlayController::enableCandidateRefresh()
{
    if (!pickMode_ || candidateRefreshEnabled_) {
        return;
    }
    candidateRefreshEnabled_ = true;
    // The CLI answers on the same pipe the session path came in on.  It is a
    // pipe, so this never blocks on a terminal: drain what is there, and let
    // the notifier wake the controller when more arrives.
    const int flags = ::fcntl(STDIN_FILENO, F_GETFL, 0);
    if (flags >= 0) {
        ::fcntl(STDIN_FILENO, F_SETFL, flags | O_NONBLOCK);
    }
    candidateReader_ = new QSocketNotifier(STDIN_FILENO, QSocketNotifier::Read);
    QObject::connect(candidateReader_, &QSocketNotifier::activated, candidateReader_, [this] {
        readCandidateReplies();
    });
    // The pointer asks as it travels; this asks when it does not, so a desktop
    // that changed without an event here still catches up.
    candidateClock_.restart();
    candidateTimer_ = new QTimer();
    candidateTimer_->setInterval(kCandidateRefreshPollMs);
    QObject::connect(candidateTimer_, &QTimer::timeout, candidateTimer_, [this] {
        requestCandidateRefresh();
    });
    candidateTimer_->start();
}

void OverlayController::requestCandidateRefresh()
{
    if (!candidateRefreshEnabled_ || candidateRefreshPending_ || finished_ || cancelled_) {
        return;
    }
    if (candidateClock_.elapsed() < kCandidateRefreshIntervalMs) {
        return;
    }
    candidateClock_.restart();
    const QByteArray request = QByteArrayLiteral("{\"request\":\"candidates\"}\n");
    if (std::fwrite(request.constData(), 1, static_cast<std::size_t>(request.size()), stdout) !=
        static_cast<std::size_t>(request.size())) {
        return;
    }
    std::fflush(stdout);
    candidateRefreshPending_ = true;
}

/// Reads whatever the CLI has answered so far: one JSON object per line, a
/// `candidates` array being a fresh list.  Anything without one (an empty
/// object, or a CLI that has no window list to offer) leaves the current list
/// alone, so a picker that cannot be refreshed still highlights something.
void OverlayController::readCandidateReplies()
{
    char buffer[4096];
    while (true) {
        const ssize_t got = ::read(STDIN_FILENO, buffer, sizeof(buffer));
        if (got > 0) {
            candidateReplies_.append(buffer, static_cast<int>(got));
            continue;
        }
        if (got == 0) {
            // The CLI closed the pipe (it is on its way out): stop listening.
            candidateReader_->setEnabled(false);
            return;
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno == EAGAIN || errno == EWOULDBLOCK) {
            break;
        }
        candidateReader_->setEnabled(false);
        return;
    }

    int newline = candidateReplies_.indexOf('\n');
    while (newline >= 0) {
        const QByteArray line = candidateReplies_.left(newline);
        candidateReplies_.remove(0, newline + 1);
        newline = candidateReplies_.indexOf('\n');
        candidateRefreshPending_ = false;
        QJsonParseError parseError;
        const QJsonDocument document = QJsonDocument::fromJson(line, &parseError);
        if (parseError.error != QJsonParseError::NoError || !document.isObject()) {
            continue;
        }
        const QJsonValue value = document.object().value(QStringLiteral("candidates"));
        if (!value.isArray()) {
            continue;
        }
        const QJsonArray array = value.toArray();
        QVector<WindowCandidate> candidates;
        candidates.reserve(array.size());
        bool valid = true;
        for (int index = 0; index < array.size(); ++index) {
            QString reason;
            WindowCandidate candidate;
            if (!array.at(index).isObject() ||
                !parseWindowCandidate(array.at(index).toObject(),
                                      QStringLiteral("candidate %1").arg(index), &candidate,
                                      &reason)) {
                valid = false;
                break;
            }
            candidates.push_back(std::move(candidate));
        }
        if (valid) {
            applyCandidates(std::move(candidates));
        }
    }
}

/// Replaces the candidate list with a fresh one and points the hover at
/// whatever the (unmoved) pointer is over now.  The picker polls, so most
/// answers are the list it already has: comparing first keeps the veil from
/// being repainted three times a second for nothing.
void OverlayController::applyCandidates(QVector<WindowCandidate> candidates)
{
    const bool sameList = candidates.size() == candidates_.size() &&
        std::equal(candidates.cbegin(), candidates.cend(), candidates_.cbegin(),
                   [](const WindowCandidate &first, const WindowCandidate &second) {
                       return first.rect.x == second.rect.x && first.rect.y == second.rect.y &&
                           first.rect.width == second.rect.width &&
                           first.rect.height == second.rect.height &&
                           first.label == second.label;
                   });
    if (sameList) {
        return;
    }
    candidates_ = std::move(candidates);
    // The pointer has not moved, but what sits under it may have: re-point the
    // hover at the new list instead of keeping a highlight that no longer
    // describes anything on screen.
    hoveredCandidate_ = candidateIndexAt(pointer_);
    if (hoveredCandidate_ >= 0) {
        selection_ = candidates_.at(hoveredCandidate_).rect;
    } else {
        selection_.reset();
    }
    for (CaptureOverlay *overlay : overlays_) {
        overlay->setCursor(hoveredCandidate_ >= 0 ? Qt::PointingHandCursor : Qt::ArrowCursor);
    }
    updateAll();
}

void OverlayController::startSelection(Point point)
{
    gesture_->type = Gesture::Type::Selecting;
    gesture_->anchor = clampPoint(point);
    gesture_->current = gesture_->anchor;
    selection_.reset();
    selectedAnnotation_ = -1;
    editing_ = false;
    hideToolbar();
}

void OverlayController::updateSelection(Point point)
{
    gesture_->current = clampPoint(point);
    selection_ = selectionBetween(gesture_->anchor, gesture_->current);
}

void OverlayController::finishSelection(Point point)
{
    updateSelection(point);
    gesture_->type = Gesture::Type::None;
    if (!selection_.has_value()) {
        return;
    }
    if (selectOnly_) {
        // A drag that lands on something usable ends the session right there;
        // a stray click leaves the surface alone so the user can try again.
        if (hasValidSelection()) {
            terminal(false);
        }
        return;
    }
    editing_ = true;
    showToolbar();
}

void OverlayController::beginDrawing(Point point)
{
    gesture_->type = Gesture::Type::Drawing;
    gesture_->anchor = clampPoint(point);
    gesture_->current = gesture_->anchor;
    gesture_->points.clear();
    gesture_->points.push_back(gesture_->anchor);
    // The live raster starts over with each stroke.
    gesture_->liveRaster = QImage();
    gesture_->liveOrigin = QPoint();
    gesture_->liveBaked = 1;
    gesture_->liveLength = 0.0;
    gesture_->liveKey.clear();
    liveStrokeBakes_ = 0;
}

void OverlayController::updateDrawing(Point point)
{
    const Point bounded = clampPoint(point);
    gesture_->current = bounded;
    if (tool_ == Tool::Arrow || tool_ == Tool::Line || tool_ == Tool::Wave) {
        // The two-point tools: only the anchor and the current point matter, so
        // the gesture never records the wandering intermediate positions.  The
        // arrow and the line are the segment between them; the wave is the sine
        // sample of that same segment, drawn from it on every paint.
        gesture_->points = {gesture_->anchor, bounded};
        return;
    }
    if (gesture_->points.isEmpty() || gesture_->points.constLast().x != bounded.x ||
        gesture_->points.constLast().y != bounded.y) {
        gesture_->points.push_back(bounded);
    }
}

void OverlayController::finishDrawing(Point point)
{
    updateDrawing(point);
    const QVector<Point> points = gesture_->points;
    const Tool drawingTool = tool_;
    gesture_->type = Gesture::Type::None;
    gesture_->points.clear();
    gesture_->liveRaster = QImage();
    gesture_->liveKey.clear();
    gesture_->liveBaked = 0;
    gesture_->liveLength = 0.0;
    if (points.isEmpty()) {
        return;
    }
    if (drawingTool == Tool::Arrow &&
        (points.size() < 2 || points.constFirst() == points.constLast())) {
        return;
    }
    Annotation annotation;
    annotation.tool = toolName(drawingTool);
    annotation.textPixels = textSize_;
    annotation.color = currentColor_;
    annotation.width = currentWidth_;
    annotation.dash = currentDash_;
    annotation.size = arrowSize_;
    annotation.arrowStyle = currentArrowStyle_;
    annotation.strength = mosaicStrength_;
    if (drawingTool == Tool::Rectangle || drawingTool == Tool::Ellipse) {
        annotation.kind = Annotation::Kind::Shape;
        annotation.rect = selectionBetween(points.constFirst(), points.constLast());
        // A click without a drag yields a degenerate 1x1 rect: drop it.
        if (annotation.rect.isEmpty() || annotation.rect.width < 2 ||
            annotation.rect.height < 2) {
            return;
        }
    } else if (drawingTool == Tool::Mosaic && mosaicShape_ != QStringLiteral("brush")) {
        // Rectangle/ellipse mosaic is a shape annotation so it can be selected
        // and resized afterwards.
        annotation.kind = Annotation::Kind::Shape;
        annotation.rect = selectionBetween(points.constFirst(), points.constLast());
        annotation.mask = mosaicShape_;
        if (annotation.rect.isEmpty() || annotation.rect.width < 2 ||
            annotation.rect.height < 2) {
            return;
        }
    } else {
        annotation.kind = Annotation::Kind::Stroke;
        annotation.points = points;
    }
    QVector<Annotation> next = annotations_;
    next.push_back(annotation);
    const int newIndex = next.size() - 1;
    mutateAnnotations(std::move(next));
    // Newly drawn annotations stay selected so the panel restyles them and
    // the Select tool can immediately move/resize them.
    selectAnnotation(newIndex);
}

void OverlayController::beginBezier(Point point)
{
    gesture_->type = Gesture::Type::Bezier;
    gesture_->anchor = clampPoint(point);
    gesture_->current = gesture_->anchor;
    gesture_->points.clear();
    // None of the freehand stroke's incremental raster is used here: a pen path
    // is redrawn whole from its anchors on every paint, which is what it always
    // was, so there is nothing to accumulate.
    gesture_->liveRaster = QImage();
    gesture_->liveOrigin = QPoint();
    gesture_->liveBaked = 0;
    gesture_->liveLength = 0.0;
    gesture_->liveKey.clear();
}

void OverlayController::updateBezier(Point point, bool dragging)
{
    const Point bounded = clampPoint(point);
    gesture_->current = bounded;
    if (dragging && !gesture_->points.isEmpty()) {
        // The drag pulls the outgoing handle of the anchor just placed out; the
        // incoming side is its mirror, so only one of the two is ever stored --
        // the same symmetric handle the wire format describes.
        gesture_->points.last() = bounded;
    }
}

void OverlayController::finishBezier(bool closed)
{
    const QVector<Point> points = gesture_->points;
    gesture_->type = Gesture::Type::None;
    gesture_->points.clear();
    if (points.isEmpty()) {
        updateAll();
        return;
    }
    Annotation annotation;
    annotation.kind = Annotation::Kind::Stroke;
    annotation.tool = toolName(Tool::Bezier);
    // A single anchor has no segment to close, so a path that was somehow closed
    // before it had two of them stays open rather than being filled as a point.
    annotation.closed = closed && bezierAnchors(points) >= 2;
    annotation.color = currentColor_;
    annotation.width = currentWidth_;
    annotation.dash = currentDash_;
    annotation.points = points;
    QVector<Annotation> next = annotations_;
    next.push_back(annotation);
    const int newIndex = next.size() - 1;
    mutateAnnotations(std::move(next));
    // Newly drawn annotations stay selected so the panel restyles them and the
    // Select tool can immediately move them.
    selectAnnotation(newIndex);
}

void OverlayController::undo()
{
    if (finished_ || cancelled_ || undoStack_.isEmpty()) {
        return;
    }
    if (textEdit_ != nullptr) {
        finishText(true);
    }
    redoStack_.push_back(annotations_);
    annotations_ = undoStack_.takeLast();
    selectedAnnotation_ = -1;
    updateAll();
}

void OverlayController::redo()
{
    if (finished_ || cancelled_ || redoStack_.isEmpty()) {
        return;
    }
    if (textEdit_ != nullptr) {
        finishText(true);
    }
    undoStack_.push_back(annotations_);
    annotations_ = redoStack_.takeLast();
    selectedAnnotation_ = -1;
    updateAll();
}

void OverlayController::mutateAnnotations(QVector<Annotation> next)
{
    undoStack_.push_back(annotations_);
    if (undoStack_.size() > kMaxUndoSteps) {
        undoStack_.removeFirst();
    }
    redoStack_.clear();
    annotations_ = std::move(next);
    if (selectedAnnotation_ >= annotations_.size()) {
        selectedAnnotation_ = -1;
    }
    updateAll();
}

bool OverlayController::annotationBounds(const Annotation &annotation, LogicalRect *bounds) const
{
    return annotationLogicalBounds(annotation, bounds);
}

bool OverlayController::canDrawAt(Point point) const
{
    const LogicalRect &limits = annotationLimits();
    return point.x >= limits.x && point.x < limits.right() &&
           point.y >= limits.y && point.y < limits.bottom();
}

void OverlayController::placeNumber(Point point)
{
    Annotation annotation;
    annotation.kind = Annotation::Kind::Text;
    annotation.tool = QStringLiteral("number");
    // One past the highest badge already on the canvas, read fresh every time.
    // Undoing a badge therefore hands its number back to the next click, and
    // there is no counter that could drift out of step with the list.
    int highest = 0;
    for (const Annotation &existing : annotations_) {
        highest = std::max(highest, existing.number);
    }
    annotation.number = highest + 1;
    annotation.numberStyle = numberStyle_;
    annotation.color = currentColor_;
    annotation.width = currentWidth_;
    // The badge hangs from the point the click landed on, and its box is
    // recorded on the annotation: the hit test, the drag clamp, the raster cache
    // and the paint all read it from there, which is what makes a badge
    // selectable, movable and deletable rather than merely visible.  `origin` is
    // the same corner, because that is where the renderer blits the bitmap.
    layoutNumberBox(annotation, clampPoint(point));
    QVector<Annotation> next = annotations_;
    next.push_back(annotation);
    const int newIndex = next.size() - 1;
    mutateAnnotations(std::move(next));
    selectAnnotation(newIndex);
}

void OverlayController::beginText(CaptureOverlay *overlay, Point point)
{
    if (textEdit_ != nullptr) {
        finishText(true);
    }
    point = clampPoint(point);
    int index = -1;
    for (int i = annotations_.size() - 1; i >= 0; --i) {
        LogicalRect bounds;
        // Only a real label is re-editable.  A numbered badge is a text
        // annotation by wire but carries no typed string, so letting the scan
        // find one would open an empty editor over it and replace the badge with
        // a label on commit.
        if (annotations_.at(i).kind == Annotation::Kind::Text &&
            !isNumberAnnotation(annotations_.at(i)) &&
            annotationBounds(annotations_.at(i), &bounds) && bounds.x - 3 <= point.x &&
            point.x < bounds.right() + 3 && bounds.y - 3 <= point.y &&
            point.y < bounds.bottom() + 3) {
            index = i;
            break;
        }
    }
    startTextEditor(overlay, index, index >= 0 ? annotations_.at(index).origin : point);
}

void OverlayController::startTextEditor(CaptureOverlay *overlay, int index, Point origin)
{
    QString initial;
    if (index >= 0) {
        // Snapshot the pre-edit state now so accepting the replacement keeps a
        // single undo step that restores the original text.
        undoStack_.push_back(annotations_);
        if (undoStack_.size() > kMaxUndoSteps) {
            undoStack_.removeFirst();
        }
        redoStack_.clear();
        textEditSnapshot_ = true;
        cancelledText_ = annotations_.takeAt(index);
        editingTextIndex_ = index;
        initial = cancelledText_->text;
        origin = cancelledText_->origin;
        selectedAnnotation_ = -1;
    } else {
        cancelledText_.reset();
        editingTextIndex_ = -1;
        // A fresh label must not inherit the previous selection: otherwise
        // style edits made while this editor is open (font/color/size) would
        // restyle the last committed annotation instead.
        selectedAnnotation_ = -1;
    }
    // While re-editing, the editor mirrors the annotation's own style so the
    // user must not re-pick it after moving a label around.
    textEditPixels_ = cancelledText_.has_value() ? cancelledText_->textPixels : textSize_;
    const QColor editColor = cancelledText_.has_value() ? cancelledText_->color : currentColor_;
    textEditFont_ = cancelledText_.has_value() ? cancelledText_->font : currentFont_;
    textOutput_ = overlay->outputIndex();
    textOrigin_ = origin;
    auto *owner = overlay;
    textEdit_ = new InlineTextEdit(owner, [this](bool accept) {
        if (accept) {
            finishText(true);
        } else {
            finishText(false);
        }
    });
    textEdit_->setText(initial);
    QFont editorFont = textFont(textEditFont_, std::max(1, static_cast<int>(textEditPixels_)));
    textEdit_->setFont(editorFont);
    textEdit_->setStyleSheet(
        QStringLiteral("QLineEdit { color: %1; background: rgba(0, 0, 0, 140); "
                       "border: 1px solid #888; padding: 0 3px; }")
            .arg(editColor.name()));
    const QPointF local = owner->localFromGlobal(origin);
    const int width = std::min(360, std::max(160, owner->width() - 16));
    // Hug the mirrored glyph height instead of QLineEdit's roomy default
    // frame: the box only needs the text plus a small breathing margin.
    const int height = std::max(20, QFontMetrics(editorFont).height() + 6);
    const int x = std::clamp(static_cast<int>(std::round(local.x())), 4, std::max(4, owner->width() - width - 4));
    const int y = std::clamp(static_cast<int>(std::round(local.y())), 4, std::max(4, owner->height() - height - 4));
    textEdit_->setGeometry(x, y, width, height);
    textEdit_->show();
    textEdit_->raise();
    textEdit_->setFocus(Qt::OtherFocusReason);
    if (!initial.isEmpty()) {
        textEdit_->selectAll();
    }
    updateAll();
}

void OverlayController::finishText(bool accept)
{
    if (textEdit_ == nullptr) {
        return;
    }
    const QString value = textEdit_->text();
    // Capture the height the editor is actually drawing at before the state is
    // cleared: a size change made while the box was open lives here and in
    // nowhere else (during a re-edit no annotation is selected to restyle).
    const std::uint32_t editedPixels = textEditPixels_;
    textEdit_->hide();
    textEdit_->deleteLater();
    textEdit_ = nullptr;
    textEditPixels_ = 0;
    if (accept && !value.isEmpty()) {
        Annotation annotation;
        annotation.kind = Annotation::Kind::Text;
        annotation.tool = QStringLiteral("text");
        annotation.origin = cancelledText_.has_value() ? cancelledText_->origin : textOrigin_;
        annotation.text = value;
        if (cancelledText_.has_value()) {
            // Re-edits keep the label's own style unless the panel restyles
            // it while editing; textEditFont_ tracks a live font change and
            // textEditPixels_ a live size change.
            annotation.textPixels = editedPixels > 0 ? editedPixels : cancelledText_->textPixels;
            annotation.color = cancelledText_->color;
            annotation.font = textEditFont_;
        } else {
            annotation.textPixels = textSize_;
            annotation.color = currentColor_;
            annotation.font = currentFont_;
        }
        QVector<Annotation> next = annotations_;
        if (editingTextIndex_ >= 0) {
            // Re-edit: the pre-edit snapshot was already pushed by beginText,
            // so a single undo restores the original text.
            next.insert(std::min(editingTextIndex_, static_cast<int>(next.size())), annotation);
            textEditSnapshot_ = false;
            redoStack_.clear();
            annotations_ = std::move(next);
            selectedAnnotation_ = std::min(editingTextIndex_, static_cast<int>(annotations_.size()) - 1);
            updateAll();
        } else {
            next.push_back(annotation);
            const int newIndex = next.size() - 1;
            mutateAnnotations(std::move(next));
            selectAnnotation(newIndex);
        }
    } else if (!accept && cancelledText_.has_value()) {
        if (textEditSnapshot_) {
            // Rejected edit: drop the beginText snapshot so undo does not
            // replay a no-op, then restore the previous text exactly.
            undoStack_.removeLast();
            textEditSnapshot_ = false;
        }
        annotations_.insert(std::min(editingTextIndex_, static_cast<int>(annotations_.size())),
                            *cancelledText_);
        updateAll();
    }
    cancelledText_.reset();
    editingTextIndex_ = -1;
    textOutput_ = -1;
}

void OverlayController::showToolbar()
{
    if (!selection_.has_value() || overlays_.isEmpty()) {
        return;
    }
    // The selection may have moved to another output (drag, resize, arrow
    // keys): re-evaluate the owning overlay on every show.
    toolbarOutput_ = outputIndexForSelection();
    if (toolbar_ == nullptr) {
        toolbar_ = new FloatingToolbar(this, overlays_.at(toolbarOutput_));
    } else if (toolbar_->parentWidget() != overlays_.at(toolbarOutput_)) {
        toolbar_->setParent(overlays_.at(toolbarOutput_));
    }
    toolbar_->adjustSize();
    toolbar_->show();
    toolbar_->raise();
    toolbar_->syncState();
    updateToolbarGeometry();
}

// The output that currently hosts the selection (by its center point).
int OverlayController::outputIndexForSelection() const
{
    if (overlays_.isEmpty()) {
        return 0;
    }
    if (selection_.has_value()) {
        const std::int64_t centerX =
            selection_->x + static_cast<std::int64_t>(selection_->width) / 2;
        const std::int64_t centerY =
            selection_->y + static_cast<std::int64_t>(selection_->height) / 2;
        for (CaptureOverlay *overlay : overlays_) {
            const OutputSession &output = overlay->output();
            if (centerX >= output.geometry.x && centerX < output.geometry.right() &&
                centerY >= output.geometry.y && centerY < output.geometry.bottom()) {
                return overlay->outputIndex();
            }
        }
    }
    return toolbarOutput_ >= 0 && toolbarOutput_ < overlays_.size() ? toolbarOutput_ : 0;
}

// Drops a dragged panel on whichever output it was released over.
void OverlayController::settlePanelAtGlobal(QPoint topLeft)
{
    if (toolbar_ == nullptr || overlays_.isEmpty() || !toolbar_->isVisible()) {
        return;
    }
    const QPoint center = topLeft + QPoint(toolbar_->width() / 2, toolbar_->height() / 2);
    CaptureOverlay *target = nullptr;
    for (CaptureOverlay *overlay : overlays_) {
        const QRect bounds(overlay->mapToGlobal(QPoint(0, 0)), overlay->size());
        if (bounds.contains(center)) {
            target = overlay;
            break;
        }
    }
    if (target == nullptr) {
        return;
    }
    if (target != toolbar_->parentWidget()) {
        toolbarOutput_ = target->outputIndex();
        toolbarAnchorValid_ = false;
        toolbar_->setParent(target);
        toolbar_->show();
        toolbar_->raise();
    }
    const QPoint local = target->mapFromGlobal(topLeft);
    const int x = std::clamp(local.x(), 0, std::max(0, target->width() - toolbar_->width()));
    const int y = std::clamp(local.y(), 0, std::max(0, target->height() - toolbar_->height()));
    toolbar_->move(x, y);
}

void OverlayController::hideToolbar()
{
    toolbarAnchorValid_ = false;
    if (toolbar_ != nullptr) {
        toolbar_->hide();
    }
}

void OverlayController::updateToolbarGeometry()
{
    if (toolbar_ == nullptr || !toolbar_->isVisible() || !selection_.has_value() ||
        toolbarOutput_ < 0 || toolbarOutput_ >= overlays_.size()) {
        return;
    }
    if (panelPinned_) {
        // The user moved the panel to a custom spot; keep it there.
        return;
    }
    // Follow the selection across outputs while it is being dragged or resized.
    const int outputIndex = outputIndexForSelection();
    if (outputIndex != toolbarOutput_) {
        toolbarOutput_ = outputIndex;
        toolbarAnchorValid_ = false;
        if (toolbar_->parentWidget() != overlays_.at(toolbarOutput_)) {
            toolbar_->setParent(overlays_.at(toolbarOutput_));
        }
    }
    CaptureOverlay *owner = overlays_.at(toolbarOutput_);
    // Keep the command bar attached to the selection: it settles centred above
    // the selection, or below when the panel does not fit above, always clamped
    // to the owning output.  The style row takes whichever side of the command
    // bar still has room; when neither side does -- a capture that nearly fills
    // the display -- it doubles back over the selection instead, so showing or
    // hiding it never nudges the buttons.
    const OutputSession &output = owner->output();
    const QRectF localSelection = localRect(output, *selection_, owner->size());
    const QRect anchorSelection = localSelection.toRect();
    const int width = toolbar_->width();
    const int height = toolbar_->height();
    const int barHeight = toolbar_->commandBarHeight();
    const int topPadding = toolbar_->panelTopPadding();
    const int styleExtra = toolbar_->styleRowExtra();
    constexpr int kEdgeMargin = 4;
    constexpr int kSelectionGap = 8;
    const int selectionTop = static_cast<int>(std::round(localSelection.top()));
    const int selectionBottom = static_cast<int>(std::round(localSelection.bottom()));
    // While the selection stays put, keep the side it was placed on so a style
    // toggle cannot throw the command bar to the other side of the selection.
    const bool selectionSettled =
        toolbarAnchorValid_ && toolbarAnchorSelection_ == anchorSelection;
    const bool below = selectionSettled
        ? toolbarAnchorBelow_
        : selectionTop - kSelectionGap - height < kEdgeMargin;
    // The style row prefers the far side of the command bar; when it does not
    // fit there, it expands the other way, over the selection.
    bool styleAbove = !below;
    if (styleAbove) {
        styleAbove = selectionTop - kSelectionGap - barHeight - topPadding - styleExtra >=
            kEdgeMargin;
    } else {
        const bool fitsBelow =
            selectionBottom + kSelectionGap - topPadding + height <= owner->height() - kEdgeMargin;
        styleAbove = !fitsBelow;
    }
    toolbar_->setStyleRowAbove(styleAbove);
    // Anchor the command bar's edge nearest the selection, not the panel edge:
    // the style row may sit on either side of it, so only this edge is fixed.
    const int barTop =
        below ? selectionBottom + kSelectionGap : selectionTop - kSelectionGap - barHeight;
    int y = barTop - (styleAbove ? topPadding + styleExtra : topPadding);
    int x = 0;
    if (selectionSettled) {
        x = std::clamp(toolbarAnchor_.x(), 0, std::max(0, owner->width() - width));
    } else {
        x = std::clamp(static_cast<int>(std::round(localSelection.center().x() - width / 2.0)),
                       kEdgeMargin, std::max(kEdgeMargin, owner->width() - width - kEdgeMargin));
    }
    y = std::clamp(y, kEdgeMargin,
                   std::max(kEdgeMargin, owner->height() - height - kEdgeMargin));
    toolbarAnchor_ = QPoint(x, y);
    toolbarAnchorSelection_ = anchorSelection;
    toolbarAnchorBelow_ = below;
    toolbarAnchorValid_ = true;
    toolbar_->setGeometry(x, y, width, height);
}

int OverlayController::sceneScale() const
{
    int scale = 1;
    for (const OutputSession &output : session_.outputs) {
        scale = std::max(scale, static_cast<int>(output.scale));
    }
    return scale;
}

void OverlayController::repaintEverything()
{
    for (CaptureOverlay *overlay : overlays_) {
        overlay->update();
    }
    if (toolbar_ != nullptr && toolbar_->isVisible()) {
        toolbar_->syncState();
        updateToolbarGeometry();
    }
}

void OverlayController::updateAll()
{
    // A full repaint erases whatever the narrow ones left, so the rect they were
    // tracking stops being the record of what is on the surface.
    hasLastTouch_ = false;
    repaintEverything();
}

// How far outside a mark's own rect its pixels can reach.  The answer lives in
// the mark's rasterizer -- the pen width, the arrow head, the mosaic brush
// radius -- which is why it is not simply a field of the annotation.
int annotationReach(const Annotation &annotation);

// Pixels of chrome a selection drag can paint outside the selection itself: the
// two-pixel outline and the round handles centred on its corners.
constexpr int kSelectionChrome = 6;

namespace {

// The session-space union of two rects, written with the explicit 64-bit
// arithmetic `LogicalRect`'s mixed-signed fields otherwise make awkward.  An
// empty rect is the identity, which is what a gesture that has not touched
// anything yet hands over.
LogicalRect uniteLogical(const LogicalRect &first, const LogicalRect &second)
{
    if (first.width == 0 || first.height == 0) {
        return second;
    }
    if (second.width == 0 || second.height == 0) {
        return first;
    }
    const std::int64_t left = std::min<std::int64_t>(first.x, second.x);
    const std::int64_t top = std::min<std::int64_t>(first.y, second.y);
    const std::int64_t far = std::max(right(first), right(second));
    const std::int64_t low = std::max(bottom(first), bottom(second));
    return LogicalRect{static_cast<std::int32_t>(left), static_cast<std::int32_t>(top),
                       static_cast<std::uint32_t>(far - left),
                       static_cast<std::uint32_t>(low - top)};
}

// The same rect grown by `margin` logical pixels on every side.
LogicalRect growBy(LogicalRect rect, int margin)
{
    rect.x -= static_cast<std::int32_t>(margin);
    rect.y -= static_cast<std::int32_t>(margin);
    rect.width += static_cast<std::uint32_t>(2 * margin);
    rect.height += static_cast<std::uint32_t>(2 * margin);
    return rect;
}

// How far outside its path a live preview paints.  The freehand pen reaches out
// by half its width; the mosaic brush stamps a block whose radius comes from the
// strength and can be far wider than the cursor.  `paintLiveStroke` builds its
// own padding out of the same two numbers, so the two move together.
double liveStrokeMargin(bool brush, int widthLogical, int scale, std::uint32_t strength)
{
    if (!brush) {
        return widthLogical / 2.0 + 2.0;
    }
    const double deviceRadius = std::clamp(
        brushRadiusForStrength(strength, std::clamp(widthLogical * scale / 2, 1, 512)), 1, 512);
    return deviceRadius / scale + 2.0;
}

// Room for the size pill the editor pins to the selection's top-left corner: it
// is centred on that corner, so it reaches half its width to either side and a
// line below.  Slack rather than the measured width, because the step has to
// invalidate before the paint knows what the text will be.
constexpr int kInfoPillSlack = 64;

} // namespace

// The magnifier the editor follows the pointer with while a gesture is dragging
// something, plus the coordinate pill that hangs under it.  The loupe sits a
// little past the pointer and flips to the other side near an edge, so the box
// is the pointer plus the whole reach on every side: half a diameter in x, and
// enough in y for the pill below the circle.
LogicalRect OverlayController::pointerTouch() const
{
    constexpr int kReachX = kLoupeDiameter + kLoupeMargin;
    constexpr int kReachY = kLoupeDiameter + kInfoPillSlack + kLoupeMargin;
    return LogicalRect{pointer_.x - kReachX, pointer_.y - kReachY, 2 * kReachX, 2 * kReachY};
}

LogicalRect OverlayController::selectionTouch() const
{
    if (!selection_.has_value()) {
        return LogicalRect{};
    }
    return uniteLogical(growBy(*selection_, kInfoPillSlack), pointerTouch());
}

LogicalRect OverlayController::annotationTouch() const
{
    if (selectedAnnotation_ < 0 || selectedAnnotation_ >= annotations_.size()) {
        return LogicalRect{};
    }
    const Annotation &annotation = annotations_.at(selectedAnnotation_);
    LogicalRect bounds;
    if (!annotationBounds(annotation, &bounds)) {
        return LogicalRect{};
    }
    // The mark's own rasterizer knows how far its pixels reach; a mark that has
    // not been painted yet has none, so the padding comes from a temporary one.
    return uniteLogical(growBy(bounds, annotationReach(annotation) + kSelectionChrome),
                        pointerTouch());
}

LogicalRect OverlayController::drawingTouch(int pointsBefore) const
{
    if (gesture_->points.isEmpty()) {
        return LogicalRect{};
    }
    // A growing stroke only stamps the segments the last pointer move added; the
    // steps before are already baked into the raster and copied, so their pixels
    // stay on screen.  The straight tools redraw their whole shape from the
    // anchor every step, so all of it counts as touched.
    const int count = static_cast<int>(gesture_->points.size());
    int first = 0;
    if (drawsGrowingStroke()) {
        first = std::clamp(pointsBefore - 1, 0, count - 1);
    }
    LogicalRect touched{gesture_->points.at(first).x, gesture_->points.at(first).y, 1u, 1u};
    for (int index = first + 1; index < count; ++index) {
        const Point &point = gesture_->points.at(index);
        touched = uniteLogical(touched, LogicalRect{point.x, point.y, 1u, 1u});
    }
    // The preview is stamped on the output the stroke started on, and that
    // output's scale is what turns the device-space brush radius back into
    // logical pixels.
    int scale = 1;
    for (CaptureOverlay *overlay : overlays_) {
        if (overlay->outputIndex() == gesture_->liveOutput) {
            scale = static_cast<int>(overlay->output().scale > 0 ? overlay->output().scale : 1);
            break;
        }
    }
    const bool brush = tool_ == Tool::Mosaic;
    const int margin = static_cast<int>(std::ceil(liveStrokeMargin(
                           brush, std::max(1, static_cast<int>(currentWidth_)), scale,
                           mosaicStrength_))) +
        kSelectionChrome;
    return growBy(touched, margin);
}

LogicalRect OverlayController::bezierTouch() const
{
    // A path always holds whole [anchor, handle] pairs, so anything shorter than
    // two has nothing to bound and nothing to join.
    if (gesture_->points.size() < 2) {
        return LogicalRect{};
    }
    // The preview is the path plus the rubber band, so its rect is the path's
    // own box united with the band.  Both are measured through the same two
    // helpers the preview paints through, so a step can never invalidate less
    // than it changed.
    LogicalRect bounds;
    if (!annotationLogicalBounds(previewAnnotation(), &bounds)) {
        return LogicalRect{};
    }
    // The rubber band is a straight segment from the path's last anchor to the
    // pointer, so its own rect is the box between the two -- the segment never
    // leaves it.
    const Point &anchor = gesture_->points.at(gesture_->points.size() - 2);
    const Point &cursor = gesture_->current;
    const LogicalRect band{std::min(anchor.x, cursor.x), std::min(anchor.y, cursor.y),
                           static_cast<std::uint32_t>(std::abs(cursor.x - anchor.x) + 1),
                           static_cast<std::uint32_t>(std::abs(cursor.y - anchor.y) + 1)};
    return growBy(uniteLogical(bounds, band),
                  annotationReach(previewAnnotation()) + kSelectionChrome);
}

// The mark the pen path in progress would commit: the path's anchors and
// handles as they stand.  The preview, its bounds and the rubber band's own
// segment all read the same shape from it.
Annotation OverlayController::previewAnnotation() const
{
    Annotation preview;
    preview.kind = Annotation::Kind::Stroke;
    preview.tool = QStringLiteral("bezier");
    preview.color = currentColor_;
    preview.width = currentWidth_;
    preview.dash = currentDash_;
    preview.points = gesture_->points;
    return preview;
}

bool OverlayController::drawsGrowingStroke() const
{
    return (tool_ == Tool::Pen && currentColor_.alpha() == 255) ||
           (tool_ == Tool::Mosaic && mosaicShape_ == QStringLiteral("brush"));
}

void OverlayController::updateTouch(const LogicalRect &touched)
{
    lastTouchLocal_ = QRect();
    if (touched.width == 0 || touched.height == 0) {
        updateAll();
        return;
    }
    // The step before this one was a full repaint -- the press that started the
    // gesture, a tool change -- so what is on the surface is not known from a
    // rect.  Cover everything once, and remember this rect so that the steps
    // which follow can be narrow: what a step has to erase is what the step
    // before it painted, and that is exactly this rect.
    if (!hasLastTouch_) {
        lastTouch_ = touched;
        hasLastTouch_ = true;
        repaintEverything();
        return;
    }
    const LogicalRect region = uniteLogical(touched, lastTouch_);
    lastTouch_ = touched;
    hasLastTouch_ = true;
    for (CaptureOverlay *overlay : overlays_) {
        LogicalRect visible;
        if (!intersection(region, overlay->output().geometry, &visible)) {
            continue;
        }
        // One pixel of slack: a mark's rounded or antialiased edge can spill
        // past the rect its geometry reports.
        const QRect local =
            localRect(overlay->output(), visible, overlay->size()).toAlignedRect().adjusted(
                -1, -1, 1, 1);
        lastTouchLocal_ = local;
        overlay->update(local);
    }
    if (toolbar_ != nullptr && toolbar_->isVisible()) {
        toolbar_->syncState();
        updateToolbarGeometry();
    }
}

QRect OverlayController::lastInteractiveUpdate() const
{
    return lastTouchLocal_;
}

void OverlayController::press(CaptureOverlay *overlay, const QPointF &local,
                              Qt::MouseButton button, Qt::KeyboardModifiers modifiers)
{
    Q_UNUSED(modifiers);
    if (finished_ || cancelled_) {
        return;
    }
    if (button == Qt::RightButton) {
        cancel();
        return;
    }
    if (button != Qt::LeftButton) {
        return;
    }
    if (textMode_) {
        // The mode owns the left button: a press on a character starts the
        // range there, a press on the empty canvas clears it.  No tool path
        // runs while the mode is on, and the pointer the mode selects with is
        // the press itself, so the range starts where the button went down.
        pointer_ = globalPoint(overlay, local);
        pointerOutput_ = overlay->outputIndex();
        const int index =
            textLayer_.has_value() ? textLayer_->indexAt(pointer_.x, pointer_.y) : -1;
        if (index >= 0) {
            selectTextRange(index, index);
            textDragging_ = true;
        } else {
            textAnchor_ = -1;
            textFocus_ = -1;
            textDragging_ = false;
        }
        updateAll();
        return;
    }
    if (textEdit_ != nullptr) {
        finishText(true);
    }
    const Point point = globalPoint(overlay, local);
    if (pickMode_ && !editing_) {
        // The click takes the window under the pointer and ends the session:
        // picking only decides what to capture, and the pixels come from the
        // frame Rust captures once this overlay is off the screen.
        const int index = candidateIndexAt(point);
        if (index >= 0) {
            hoveredCandidate_ = index;
            selection_ = candidates_.at(index).rect;
            pointer_ = point;
            // Take the highlight off the screen first: the compositor destroys
            // these surfaces asynchronously, and a lingering copy of the
            // picker would end up in the very capture that follows.
            for (CaptureOverlay *item : overlays_) {
                item->hide();
            }
            terminal(false);
        } else {
            updateAll();
        }
        return;
    }
    if (pinEdit_ && !canDrawAt(point)) {
        // Outside the pinned image the surface is bare canvas: only the Select
        // tool reacts, and only by dropping the current annotation selection.
        if (tool_ == Tool::Select) {
            selectAnnotation(-1);
            updateAll();
        }
        return;
    }
    if (tool_ == Tool::Text) {
        beginText(overlay, point);
        return;
    }
    if (tool_ == Tool::Number) {
        // One click, one badge.  Nothing waits for a release: the tool has no
        // drag to preview, so the press is the whole gesture.
        placeNumber(point);
        return;
    }
    if (tool_ == Tool::Bezier) {
        if (!canDrawAt(point)) {
            return;
        }
        // A press back onto the first anchor closes the path.  It commits here
        // rather than on a release: the click is the whole closing gesture, and
        // waiting for the button to come up would leave the filled shape
        // hanging on a press the user already made.
        const double reach = std::max(8.0, static_cast<double>(currentWidth_) * 2.0);
        if (gesture_->type == Gesture::Type::Bezier &&
            bezierAnchors(gesture_->points) >= 2 &&
            std::hypot(static_cast<double>(gesture_->points.constFirst().x - point.x),
                       static_cast<double>(gesture_->points.constFirst().y - point.y)) <= reach) {
            finishBezier(true);
            return;
        }
        if (gesture_->type != Gesture::Type::Bezier) {
            beginBezier(point);
        }
        // The anchor, and its outgoing handle starting on top of it: the drag
        // that follows pulls the handle out, and the incoming side is its
        // mirror, so the handle is symmetric by construction.
        const Point bounded = clampPoint(point);
        gesture_->points.push_back(bounded);
        gesture_->points.push_back(bounded);
        gesture_->current = bounded;
        updateAll();
        return;
    }
    if (tool_ == Tool::Select) {
        // Annotation handles and annotations take precedence over the outer
        // capture selection, so every existing mark remains adjustable.
        const int annotationHandle =
            selectedAnnotation_ >= 0 ? annotationHandleAt(point) : 0;
        const int annotationIndex = annotationHitAt(point);
        if (annotationHandle != 0) {
            beginAnnotationDrag(point, true);
        } else if (annotationIndex >= 0) {
            selectAnnotation(annotationIndex);
            beginAnnotationDrag(point, false);
        } else if (pinEdit_) {
            // Empty image surface: drop the current annotation selection and
            // start dragging the image itself (which carries its annotations).
            selectAnnotation(-1);
            if (selection_.has_value()) {
                gesture_->anchor = point;
                gesture_->current = point;
                gesture_->origin = *selection_;
                gesture_->handle = 9;
                gesture_->type = Gesture::Type::Moving;
            }
        } else {
            const int handle = hitHandle(point);
            if (selection_.has_value() && handle != 0 && handle != 9) {
                gesture_->anchor = point;
                gesture_->current = point;
                gesture_->origin = *selection_;
                gesture_->handle = handle;
                gesture_->type = Gesture::Type::Resizing;
            } else if (selection_.has_value() && handle == 9) {
                gesture_->anchor = point;
                gesture_->current = point;
                gesture_->origin = *selection_;
                gesture_->handle = handle;
                gesture_->type = Gesture::Type::Moving;
            } else {
                startSelection(point);
            }
        }
    } else {
        if (!canDrawAt(point)) {
            return;
        }
        beginDrawing(point);
        // The live raster is local to the overlay the stroke starts on; a
        // stroke that wanders onto another output is clipped away there anyway.
        gesture_->liveOutput = overlay->outputIndex();
    }
    updateAll();
}

void OverlayController::move(CaptureOverlay *overlay, const QPointF &local, Qt::MouseButtons buttons,
                             Qt::KeyboardModifiers modifiers)
{
    Q_UNUSED(modifiers);
    if (finished_ || cancelled_) {
        return;
    }
    const Point point = globalPoint(overlay, local);
    pointer_ = point;
    pointerOutput_ = overlay->outputIndex();
    if (textMode_) {
        // The mode has the pointer to itself: no candidate hover, no gesture.
        // A drag widens the range to the character nearest the pointer, which
        // is what keeps a drag that left the text meaning something; an idle
        // pointer is the caret the mode selects with.
        if (textDragging_ && textLayer_.has_value()) {
            const int index = textLayer_->nearestIndex(point.x, point.y);
            if (index >= 0) {
                selectTextRange(textAnchor_, index);
                updateAll();
            }
        } else {
            overlay->setCursor(Qt::IBeamCursor);
        }
        return;
    }
    if (pickMode_ && !editing_ && buttons == Qt::NoButton &&
        gesture_->type == Gesture::Type::None) {
        // Nothing is committed yet: the pointer only previews which window a
        // click would take.  Travelling is also the moment to re-check what
        // the compositor has — a workspace switch or a moved window since the
        // list was taken would otherwise leave the highlight pointing at
        // nothing (or at the wrong window).
        requestCandidateRefresh();
        if (applyCandidateHover(point, overlay)) {
            updateAll();
        }
        return;
    }
    const bool insideImage = canDrawAt(point);
    if (pinEdit_ && !insideImage && buttons == Qt::NoButton &&
        gesture_->type == Gesture::Type::None) {
        // Bare canvas around the pin image: the toolbar lives there, so the
        // canvas keeps a neutral pointer and no crosshair.
        overlay->setCursor(Qt::ArrowCursor);
        return;
    }
    if (buttons != Qt::NoButton) {
        // The shape must match the gesture in progress: move for drags, the
        // handle's resize arrow, cross for drawing.
        switch (gesture_->type) {
        case Gesture::Type::Moving:
        case Gesture::Type::MovingAnnotation:
            overlay->setCursor(Qt::SizeAllCursor);
            break;
        case Gesture::Type::Resizing:
        case Gesture::Type::ResizingAnnotation:
            overlay->setCursor(cursorForHandle(gesture_->handle));
            break;
        default:
            overlay->setCursor(Qt::CrossCursor);
            break;
        }
    } else if (pinEdit_) {
        // Inside the image the pointer announces the drag that moves it;
        // anywhere else with a drawing tool it is the crosshair.
        overlay->setCursor(insideImage ? Qt::SizeAllCursor : Qt::ArrowCursor);
    } else if (tool_ == Tool::Select && selection_.has_value() && editing_) {
        // Handles map to resize arrows; anywhere else inside the selection
        // (hitHandle returns 9) means the selection itself can be dragged.
        overlay->setCursor(cursorForHandle(hitHandle(point)));
    }
    if (gesture_->type == Gesture::Type::Selecting) {
        updateSelection(point);
        updateTouch(selectionTouch());
    } else if (gesture_->type == Gesture::Type::Moving) {
        applySelectionMove(gesture_->origin, gesture_->anchor, point);
        updateTouch(selectionTouch());
    } else if (gesture_->type == Gesture::Type::Resizing) {
        selection_ = resizeSelection(gesture_->origin, gesture_->handle, clampPoint(point));
        updateTouch(selectionTouch());
    } else if (gesture_->type == Gesture::Type::MovingAnnotation ||
               gesture_->type == Gesture::Type::ResizingAnnotation) {
        updateAnnotationDrag(point);
        updateTouch(annotationTouch());
    } else if (gesture_->type == Gesture::Type::Drawing) {
        const int pointsBefore = gesture_->points.size();
        updateDrawing(point);
        updateTouch(drawingTouch(pointsBefore));
    } else if (gesture_->type == Gesture::Type::Bezier) {
        // With the button down the pointer is pulling the last anchor's handle
        // out; with it up the pointer only says where the rubber band reaches.
        // Both are the same step to the path, and both repaint the same rect.
        updateBezier(point, buttons != Qt::NoButton);
        updateTouch(bezierTouch());
    } else {
        return;
    }
}

void OverlayController::release(CaptureOverlay *overlay, const QPointF &local,
                                Qt::MouseButton button, Qt::KeyboardModifiers modifiers)
{
    Q_UNUSED(modifiers);
    if (finished_ || cancelled_ || button != Qt::LeftButton) {
        return;
    }
    if (textMode_) {
        textDragging_ = false;
        return;
    }
    const Point point = globalPoint(overlay, local);
    switch (gesture_->type) {
    case Gesture::Type::Selecting:
        toolbarOutput_ = overlay->outputIndex();
        finishSelection(point);
        break;
    case Gesture::Type::Moving:
        applySelectionMove(gesture_->origin, gesture_->anchor, point);
        gesture_->type = Gesture::Type::None;
        showToolbar();
        break;
    case Gesture::Type::Resizing:
        selection_ = resizeSelection(gesture_->origin, gesture_->handle, clampPoint(point));
        gesture_->type = Gesture::Type::None;
        showToolbar();
        break;
    case Gesture::Type::MovingAnnotation:
    case Gesture::Type::ResizingAnnotation:
        finishAnnotationDrag(overlay, point);
        break;
    case Gesture::Type::Drawing:
        finishDrawing(point);
        break;
    case Gesture::Type::Bezier:
        // A release only ends the handle drag that followed the last press.  The
        // path itself is not finished until it is closed or double-clicked, so
        // the gesture stays in progress and the preview stays on screen.
        updateBezier(point, false);
        break;
    case Gesture::Type::None:
        break;
    }
    updateAll();
}

void OverlayController::doubleClick(CaptureOverlay *overlay, const QPointF &local,
                                     Qt::MouseButton button)
{
    if (button != Qt::LeftButton) {
        return;
    }
    const Point point = globalPoint(overlay, local);
    if (textMode_) {
        // A double click takes the word under the pointer, the way it does in
        // a text field, and the index is -1 off the text, which takes nothing.
        //
        // A third click is the same event: Qt has no triple-click, it reports
        // the second *and* the third press of a rapid run as a double click.
        // So a double click that lands within the platform's double-click
        // interval of the last one is the third press, and widens the word to
        // its whole line -- otherwise a triple click would read as a second
        // double click and reselect the same word.
        const int index = textLayer_.has_value() ? textLayer_->indexAt(point.x, point.y) : -1;
        const bool third = index >= 0 && textClickClock_.isValid() &&
            textClickClock_.elapsed() <= QApplication::doubleClickInterval();
        textClickClock_.restart();
        if (third) {
            textSelectLine(index);
        } else {
            textSelectWord(index);
        }
        return;
    }
    if (tool_ == Tool::Bezier && gesture_->type == Gesture::Type::Bezier) {
        // A double click ends the path where it stands, open.  Qt delivers the
        // second click of the pair as this event rather than as a press, so the
        // anchor it would have placed is the one the first click already did --
        // the path is not left with a duplicate point on its end.
        finishBezier(false);
        return;
    }
    const int index = annotationHitAt(point);
    if (index >= 0 && annotations_.at(index).kind == Annotation::Kind::Text &&
        !isNumberAnnotation(annotations_.at(index))) {
        // Only a typed label re-edits.  Opening the editor over a badge would
        // take the badge out of the list and commit a label in its place.
        startTextEditor(overlay, index, annotations_.at(index).origin);
        return;
    }
    if (selection_.has_value() && !pinEdit_ && hitHandle(point) != 0) {
        confirm();
    }
}

void OverlayController::key(CaptureOverlay *overlay, int key, Qt::KeyboardModifiers modifiers)
{
    Q_UNUSED(overlay);
    if (key == Qt::Key_Escape) {
        if (textEdit_ != nullptr) {
            finishText(false);
        } else if (textMode_) {
            // The mode goes first: Escape leaves the text selection without
            // ending the capture, so a second Escape is what cancels it.
            leaveTextMode();
        } else if (gesture_->type == Gesture::Type::Bezier) {
            // The pen path in progress goes first: Escape drops it without
            // ending the session, the way it drops any other in-progress
            // gesture.  A second Escape then cancels the capture.
            gesture_->type = Gesture::Type::None;
            gesture_->points.clear();
            updateAll();
        } else {
            cancel();
        }
        return;
    }
    if (key == Qt::Key_Return || key == Qt::Key_Enter) {
        if (textEdit_ != nullptr) {
            finishText(true);
        } else if (textMode_) {
            // Enter copies what the range holds and leaves the mode.
            copyTextSelection();
        } else {
            confirm();
        }
        return;
    }
    if (textEdit_ != nullptr) {
        return;
    }
    if (textMode_) {
        // The mode owns the keys: Ctrl+C copies the range and Ctrl+A selects
        // it all, and every other key is swallowed so the arrow keys cannot
        // move the capture's selection out from under the text.
        if (modifiers & Qt::ControlModifier) {
            if (key == Qt::Key_C) {
                copyTextSelection();
            } else if (key == Qt::Key_A) {
                textSelectAll();
            }
        }
        return;
    }
    if (modifiers & Qt::ControlModifier) {
        if (key == Qt::Key_Z && !(modifiers & Qt::ShiftModifier)) {
            undo();
        } else if (key == Qt::Key_Z || key == Qt::Key_Y) {
            redo();
        } else if (key == Qt::Key_V) {
            // Paste is the one action here that can fail for a reason the user
            // needs told: an empty clipboard, or `wl-paste` missing. There is
            // no status line on a frozen overlay, so the message goes to stderr
            // where the CLI's own diagnostics already land.
            QString error;
            if (!pasteFromClipboard(&error)) {
                std::fprintf(stderr, "vshot-qt-ui: %s\n", error.toUtf8().constData());
                std::fflush(stderr);
            }
        }
        return;
    }
    if ((key == Qt::Key_Delete || key == Qt::Key_Backspace) &&
        gesture_->type == Gesture::Type::None && selectedAnnotation_ >= 0) {
        deleteSelectedAnnotation();
        return;
    }
    if (!selection_.has_value() || !editing_ || gesture_->type != Gesture::Type::None) {
        return;
    }
    if (pinEdit_) {
        // The pin editor's selection is the image itself: it moves with the
        // image (drag or arrow keys) and never resizes.
        return;
    }
    const int step = (modifiers & Qt::ShiftModifier) ? 10 : 1;
    int dx = 0;
    int dy = 0;
    switch (key) {
    case Qt::Key_Left:
        dx = -step;
        break;
    case Qt::Key_Right:
        dx = step;
        break;
    case Qt::Key_Up:
        dy = -step;
        break;
    case Qt::Key_Down:
        dy = step;
        break;
    default:
        return;
    }
    const Point anchor{selection_->x, selection_->y};
    const Point current{selection_->x + dx, selection_->y + dy};
    applySelectionMove(*selection_, anchor, current);
    updateAll();
}

// Moves the selection — the pinned image, in pin-edit mode — to follow the
// pointer, carrying its annotations along. Annotations are stored in global
// coordinates and the renderer anchors them to the returned selection, so
// translating them keeps the preview and the rendered result in step.
//
// In pin-edit mode the moving is delegated outright: the daemon repositions
// the real pin window with its own clamp, and the reply lands back here as the
// authoritative rect. The editor never paints the image itself, so there is
// nothing to keep in sync but the annotations.
void OverlayController::applySelectionMove(LogicalRect origin, Point anchor, Point current)
{
    const LogicalRect moved = moveSelection(origin, anchor, current);
    if (pinEdit_ && selection_.has_value()) {
        // Image and marks travel together: shifting both by the same delta
        // keeps every mark on the image pixel it was drawn on.
        translateAnnotations(moved.x - selection_->x, moved.y - selection_->y);
        // The daemon does the actual moving (its pin window is the one on
        // screen) and answers with the rect it clamped to; applyPinRect
        // corrects this optimistic position when the two disagree.
        requestPinMove(Point{moved.x, moved.y});
    }
    selection_ = moved;
}

void OverlayController::setPinTarget(std::uint64_t pinId, const QString &socketPath)
{
    pinId_ = pinId;
    pinSocketPath_ = socketPath;
}

// Opens one short-lived connection, sends the pin's new top-left (global
// logical pixels) and applies whatever comes back. The daemon serves exactly
// one request per connection — it writes the reply and disconnects — so a
// fresh socket per move mirrors the CLI's own client and avoids any
// reconnect bookkeeping.
void OverlayController::requestPinMove(Point globalTopLeft)
{
    if (pinSocketPath_.isEmpty() || pinId_ == 0) {
        return;
    }
    pendingPinOrigin_ = globalTopLeft;
    flushPinMove();
}

void OverlayController::flushPinMove()
{
    if (pinSocket_ != nullptr || !pendingPinOrigin_.has_value()) {
        return;
    }
    if (pinSocketPath_.isEmpty() || pinId_ == 0) {
        pendingPinOrigin_.reset();
        return;
    }
    const Point origin = *pendingPinOrigin_;
    pendingPinOrigin_.reset();

    auto *socket = new QLocalSocket;
    pinSocket_ = socket;
    const auto ownsSocket = [this, socket] { return pinSocket_ == socket; };
    QObject::connect(socket, &QLocalSocket::connected, socket, [this, socket, origin, ownsSocket] {
        if (!ownsSocket()) {
            return;
        }
        QJsonObject request;
        request.insert(QStringLiteral("cmd"), QStringLiteral("move"));
        request.insert(QStringLiteral("id"), static_cast<qint64>(pinId_));
        request.insert(QStringLiteral("x"), static_cast<qint64>(origin.x));
        request.insert(QStringLiteral("y"), static_cast<qint64>(origin.y));
        QByteArray line = QJsonDocument(request).toJson(QJsonDocument::Compact);
        line.append('\n');
        socket->write(line);
        socket->flush();
    });
    QObject::connect(socket, &QLocalSocket::readyRead, socket,
                     [this, socket, ownsSocket] {
                         if (ownsSocket()) {
                             consumePinReply(socket);
                         }
                     });
    // A refused or dropped connection is not fatal: the editor keeps working,
    // it just cannot move the live pin. A later move retries from scratch.
    QObject::connect(socket, &QLocalSocket::errorOccurred, socket,
                     [this, socket, ownsSocket](QLocalSocket::LocalSocketError) {
                         if (ownsSocket()) {
                             consumePinReply(socket);
                         }
                     });
    // The daemon closes the connection right after replying, so the reply must
    // be drained here too.
    QObject::connect(socket, &QLocalSocket::disconnected, socket,
                     [this, socket, ownsSocket] {
                         if (ownsSocket()) {
                             consumePinReply(socket);
                         }
                     });
    socket->connectToServer(pinSocketPath_);
}

// Drains the daemon's answer, applies it and retires this request's socket.
// Called from every terminal signal; the owner check upstream makes repeats
// harmless.
void OverlayController::consumePinReply(QLocalSocket *socket)
{
    pinReplyBuffer_ += socket->readAll();
    const qsizetype newline = pinReplyBuffer_.indexOf('\n');
    QByteArray line;
    if (newline >= 0) {
        line = pinReplyBuffer_.left(newline);
    }
    pinReplyBuffer_.clear();
    // Cleared before deleteLater() so the guard in every handler above stops
    // this socket from being treated as the live one while it is queued away.
    pinSocket_ = nullptr;
    socket->deleteLater();
    if (!line.isEmpty()) {
        applyPinReply(line);
    }
    flushPinMove();
}

void OverlayController::applyPinReply(QByteArray line)
{
    const QJsonDocument document = QJsonDocument::fromJson(line);
    const QJsonObject reply = document.object();
    if (!reply.value(QStringLiteral("ok")).toBool()) {
        // The daemon refused (pin gone, bad request): keep editing locally.
        return;
    }
    LogicalRect landed;
    landed.x = static_cast<std::int32_t>(reply.value(QStringLiteral("x")).toInteger());
    landed.y = static_cast<std::int32_t>(reply.value(QStringLiteral("y")).toInteger());
    landed.width = static_cast<std::uint32_t>(reply.value(QStringLiteral("width")).toInteger());
    landed.height = static_cast<std::uint32_t>(reply.value(QStringLiteral("height")).toInteger());
    if (landed.width > 0 && landed.height > 0) {
        applyPinRect(landed);
    }
}

// Adopts the rect the daemon clamped the pin to. The pin stays the same size,
// so only a positional correction can come back; the marks were already moved
// to the requested spot, so they get the same correction.
void OverlayController::applyPinRect(const LogicalRect &rect)
{
    if (!selection_.has_value()) {
        return;
    }
    const std::int32_t dx = rect.x - selection_->x;
    const std::int32_t dy = rect.y - selection_->y;
    if (dx == 0 && dy == 0) {
        return;
    }
    selection_ = LogicalRect{rect.x, rect.y, selection_->width, selection_->height};
    translateAnnotations(dx, dy);
    updateAll();
}

void OverlayController::chooseTool(Tool tool)
{
    // Any tool change invalidates the recognized layer: the marks are about to
    // be drawn over the text, and the pointer is no longer selecting it.
    leaveTextMode();
    if (finished_ || cancelled_) {
        return;
    }
    if (textEdit_ != nullptr) {
        finishText(true);
    }
    if (gesture_->type == Gesture::Type::Bezier && tool != Tool::Bezier) {
        // An unfinished pen path goes with the tool: it is one gesture rather
        // than a drawing that outlives a tool change, and leaving it in the
        // gesture would have the next press extend a path nobody is looking at
        // any more.
        gesture_->type = Gesture::Type::None;
        gesture_->points.clear();
    }
    tool_ = tool;
    // Keep the annotation selection when moving to Select so a freshly drawn
    // annotation can be adjusted right away; drawing tools start fresh.
    if (tool != Tool::Select) {
        selectedAnnotation_ = -1;
    }
    for (CaptureOverlay *overlay : overlays_) {
        overlay->setCursor(Qt::CrossCursor);
    }
    updateAll();
}

void OverlayController::setCurrentColor(const QColor &color)
{
    if (finished_ || cancelled_ || !color.isValid()) {
        return;
    }
    currentColor_ = color;
    applyStyleToSelected([&color](Annotation &annotation) { annotation.color = color; });
    updateAll();
}

void OverlayController::setCurrentFont(const QString &family)
{
    if (finished_ || cancelled_) {
        return;
    }
    currentFont_ = family.trimmed();
    // Restyle an open editor live so the chosen face shows while typing and
    // is what finishText commits.
    if (textEdit_ != nullptr) {
        textEditFont_ = currentFont_;
        // The height comes from the size box's current value, which is also
        // what setTextSize leaves in textEditPixels_ -- the two restyle paths
        // have to agree or a font change would revert a size change.
        QFont editorFont =
            textFont(textEditFont_, std::max(1, static_cast<int>(textEditPixels_)));
        textEdit_->setFont(editorFont);
        textEdit_->setFixedHeight(std::max(20, QFontMetrics(editorFont).height() + 6));
    }
    applyStyleToSelected([this](Annotation &annotation) {
        if (annotation.kind == Annotation::Kind::Text) {
            annotation.font = currentFont_;
        }
    });
    updateAll();
}
void OverlayController::setWidth(std::uint32_t width)
{
    if (finished_ || cancelled_) {
        return;
    }
    currentWidth_ = std::clamp(width, 1u, 64u);
    applyStyleToSelected([this](Annotation &annotation) {
        // A numbered badge is a text annotation, but its size *is* derived from
        // the stroke width, so the width control has to reach it; a label's is
        // its own size box and must not be overwritten here.
        if (annotation.kind != Annotation::Kind::Text) {
            annotation.width = currentWidth_;
            return;
        }
        if (isNumberAnnotation(annotation)) {
            const Point center = numberCenter(annotation);
            annotation.width = currentWidth_;
            // The box travels with the width, or the badge would keep the size
            // it was placed at while the slider moved.
            layoutNumberBox(annotation, center);
        }
    });
    updateAll();
}

void OverlayController::setDash(const QString &dash)
{
    if (finished_ || cancelled_) {
        return;
    }
    currentDash_ = dash == QStringLiteral("dashed") || dash == QStringLiteral("dotted")
        ? dash
        : QStringLiteral("solid");
    applyStyleToSelected([this](Annotation &annotation) {
        if (annotation.kind != Annotation::Kind::Text) {
            annotation.dash = currentDash_;
        }
    });
    updateAll();
}

void OverlayController::setArrowSize(std::uint32_t size)
{
    if (finished_ || cancelled_) {
        return;
    }
    arrowSize_ = std::clamp(size, 1u, 8u);
    applyStyleToSelected([this](Annotation &annotation) {
        if (annotation.tool == QStringLiteral("arrow")) {
            annotation.size = arrowSize_;
        }
    });
    updateAll();
}

void OverlayController::setArrowStyle(const QString &style)
{
    if (finished_ || cancelled_) {
        return;
    }
    currentArrowStyle_ = style == QStringLiteral("filled") ? QStringLiteral("filled")
                                                             : QStringLiteral("open");
    applyStyleToSelected([this](Annotation &annotation) {
        if (annotation.tool == QStringLiteral("arrow")) {
            annotation.arrowStyle = currentArrowStyle_;
        }
    });
    updateAll();
}

void OverlayController::setTextSize(std::uint32_t size)
{
    if (finished_ || cancelled_) {
        return;
    }
    textSize_ = static_cast<std::uint32_t>(clampTextPixels(static_cast<int>(size)));
    // Restyle an open editor live, the way a font change does: the height is
    // what the size box is for, and seeing it change while typing is the point.
    if (textEdit_ != nullptr) {
        textEditPixels_ = textSize_;
        QFont editorFont = textFont(textEditFont_, std::max(1, static_cast<int>(textEditPixels_)));
        textEdit_->setFont(editorFont);
        // Grow the box with the glyphs so a bigger size is not clipped.
        const int height = std::max(20, QFontMetrics(editorFont).height() + 6);
        textEdit_->setFixedHeight(height);
    }
    applyStyleToSelected([this](Annotation &annotation) {
        // A badge's pixel size *is* its diameter, kept in step by the width
        // control; letting the label size box write over it would decouple the
        // bitmap's density from the scale the protocol derives.
        if (annotation.kind == Annotation::Kind::Text && !isNumberAnnotation(annotation)) {
            annotation.textPixels = textSize_;
        }
    });
    updateAll();
}

void OverlayController::setMosaicShape(const QString &shape)
{
    if (finished_ || cancelled_) {
        return;
    }
    const QString requested = shape == QStringLiteral("ellipse") || shape == QStringLiteral("brush")
        ? shape
        : QStringLiteral("rect");
    if (selectedAnnotation_ >= 0 && selectedAnnotation_ < annotations_.size()) {
        const Annotation &selected = annotations_.at(selectedAnnotation_);
        if (selected.tool == QStringLiteral("mosaic")) {
            // Area mosaics and brush mosaics have different wire payloads; do
            // not create a Shape carrying the unsupported mask="brush" value.
            mosaicShape_ = selected.kind == Annotation::Kind::Stroke
                ? QStringLiteral("brush")
                : (requested == QStringLiteral("brush") ? selected.mask : requested);
        } else {
            mosaicShape_ = requested;
        }
    } else {
        mosaicShape_ = requested;
    }
    applyStyleToSelected([this](Annotation &annotation) {
        if (annotation.tool != QStringLiteral("mosaic")) {
            return;
        }
        if (annotation.kind == Annotation::Kind::Shape && mosaicShape_ != QStringLiteral("brush")) {
            annotation.mask = mosaicShape_;
        }
    });
    updateAll();
}

void OverlayController::setMosaicStrength(std::uint32_t strength)
{
    if (finished_ || cancelled_) {
        return;
    }
    mosaicStrength_ = std::clamp(strength, 1u, 3u);
    applyStyleToSelected([this](Annotation &annotation) {
        if (annotation.tool == QStringLiteral("mosaic")) {
            annotation.strength = mosaicStrength_;
        }
    });
    updateAll();
}

void OverlayController::setNumberStyle(NumberStyle style)
{
    if (finished_ || cancelled_) {
        return;
    }
    numberStyle_ = style;
    applyStyleToSelected([style](Annotation &annotation) {
        if (isNumberAnnotation(annotation)) {
            // The box is the same square for all four looks, so only the style
            // travels: no re-layout, no change to where the badge sits.
            annotation.numberStyle = style;
        }
    });
    updateAll();
}

void OverlayController::notifyPanelDragged()
{
    panelPinned_ = true;
}

bool OverlayController::pasteImage(const QImage &image, const QString &source)
{
    Q_UNUSED(source);
    if (finished_ || cancelled_ || image.isNull() || !editing_ || !selection_.has_value()) {
        return false;
    }
    const LogicalRect &canvas = *selection_;
    if (canvas.width == 0 || canvas.height == 0) {
        return false;
    }
    // The image is placed at its own pixel size, shrunk to fit the canvas when
    // it is larger -- an oversized paste would land with its edges already
    // outside the crop, which reads as a bug rather than as a placement. Small
    // images stay their own size: blowing them up to fill the canvas would
    // blur them and is not what "paste this here" means.
    double fit = 1.0;
    if (image.width() > 0 && image.height() > 0) {
        fit = std::min(1.0, std::min(static_cast<double>(canvas.width) / image.width(),
                                      static_cast<double>(canvas.height) / image.height()));
    }
    const int width = std::max(1, static_cast<int>(std::lround(image.width() * fit)));
    const int height = std::max(1, static_cast<int>(std::lround(image.height() * fit)));

    Annotation annotation;
    annotation.kind = Annotation::Kind::Image;
    annotation.tool = QStringLiteral("image");
    annotation.pixels = image;
    annotation.rect = LogicalRect{
        static_cast<std::int32_t>(canvas.x + (static_cast<std::int64_t>(canvas.width) - width) / 2),
        static_cast<std::int32_t>(canvas.y +
                                  (static_cast<std::int64_t>(canvas.height) - height) / 2),
        static_cast<std::uint32_t>(width),
        static_cast<std::uint32_t>(height),
    };
    // The paste replaces whatever was selected: leaving the old selection on
    // would make the handles resize the previous mark while the new image sits
    // there looking like the thing that is selected.
    QVector<Annotation> next = annotations_;
    next.push_back(annotation);
    mutateAnnotations(next);
    // Selected, so the handles are up and the image can be moved or resized
    // without a trip through the toolbar.
    chooseTool(Tool::Select);
    selectAnnotation(next.size() - 1);
    return true;
}

bool OverlayController::canPaste() const
{
    return !finished_ && !cancelled_ && editing_ && selection_.has_value();
}

bool OverlayController::beginTextSelection(QString *error)
{
    // Every way this can fail is reported both to the caller and to the button
    // the user pressed: the copy can be triggered by a key the toolbar never
    // sees, so the label cannot be driven from the click handler alone.
    const auto fail = [this, error](const QString &message) {
        if (error != nullptr) {
            *error = message;
        }
        if (textResultCallback_) {
            textResultCallback_(TextOutcome::Failed, message);
        }
        return false;
    };
    if (!canPaste()) {
        return fail(uiTr("Reading text needs a selection to read from."));
    }
    const LogicalRect &canvas = *selection_;
    if (canvas.isEmpty()) {
        return fail(uiTr("The selection is empty."));
    }
    // The pixels come from the output the selection sits on, at that output's
    // own scale -- the same source the mosaic preview reads, so what is
    // recognized is what the user sees under the rectangle.
    const int index = outputContaining(canvas);
    if (index < 0 || index >= session_.outputs.size()) {
        return fail(uiTr("The selection is on no output."));
    }
    const OutputSession &output = session_.outputs.at(index);
    if (output.image.isNull()) {
        return fail(uiTr("The captured frame is not available."));
    }
    // The rect is clipped to the output: a selection dragged past the edge of
    // its screen has no pixels beyond it to read.
    const QRect source = sourceRect(output, canvas)
                             .intersected(QRect(0, 0, output.image.width(), output.image.height()));
    if (source.isEmpty()) {
        return fail(uiTr("The selection has no pixels on this output."));
    }
    const QImage pixels = output.image.copy(source);
    if (pixels.isNull()) {
        return fail(uiTr("The selection has no pixels on this output."));
    }

    QTemporaryDir directory;
    if (!directory.isValid()) {
        return fail(uiTr("Cannot create a temporary directory for the text."));
    }
    const QString path = directory.filePath(QStringLiteral("selection.png"));
    if (!pixels.save(path, "PNG")) {
        return fail(uiTr("Cannot write the selection to read its text."));
    }

    // The engine lives in `vshot`, which is a sibling of this helper: the
    // same discovery the file dialog uses, for the same reason (this process
    // is the helper, so its own path names the program to run).
    char buffer[4096];
    const ssize_t length = ::readlink("/proc/self/exe", buffer, sizeof(buffer) - 1);
    if (length <= 0) {
        return fail(uiTr("Cannot locate vshot to read the text."));
    }
    buffer[length] = '\0';
    const QString helper = QString::fromLocal8Bit(buffer);
    // `vshot` is the same binary with the helper's directory walked back one
    // level: installed layouts put both in /usr/bin, and a source checkout has
    // `build-qt/vshot-qt-ui` beside `target/release/vshot`.
    QString program = QFileInfo(helper).absolutePath() + QStringLiteral("/vshot");
    if (!QFileInfo::exists(program)) {
        const QString beside =
            QFileInfo(helper).absolutePath() + QStringLiteral("/../target/release/vshot");
        if (QFileInfo::exists(beside)) {
            program = QDir::cleanPath(beside);
        } else {
            program = QStringLiteral("vshot");
        }
    }

    // The engine takes a moment -- a model load on the first run -- and the
    // wait below runs on the GUI thread, so the button says what it is waiting
    // for, and the label is given its paint before the wait begins.  Input is
    // held back for that paint: a click landing mid-recognition would reach a
    // mode that is not up yet.
    if (textResultCallback_) {
        textResultCallback_(TextOutcome::Busy, QString());
    }
    QCoreApplication::processEvents(QEventLoop::ExcludeUserInputEvents);

    QProcess process;
    process.setProgram(program);
    // `--json` carries the position of every character, which is what the text
    // mode draws and selects with; without it the engine prints only the text.
    process.setArguments(
        {QStringLiteral("ocr"), QStringLiteral("--input"), path, QStringLiteral("--json")});
    process.setStandardInputFile(QProcess::nullDevice());
    process.start();
    if (!process.waitForStarted(kClipboardProcessTimeoutMs)) {
        return fail(uiTr("Cannot start vshot to read the text."));
    }
    // A model load takes a moment on the first run and the recognition itself
    // is a fraction of a second, so the wait is generous compared to the
    // clipboard's; a hang still has to end, hence the deadline.
    const int ocrTimeoutMs = 60 * 1000;
    if (!process.waitForFinished(ocrTimeoutMs)) {
        process.kill();
        process.waitForFinished(kClipboardProcessTimeoutMs);
        return fail(uiTr("Reading the text took too long."));
    }
    const QByteArray document = process.readAllStandardOutput();
    if (process.exitStatus() != QProcess::NormalExit || process.exitCode() != 0) {
        const QString stderr = QString::fromUtf8(process.readAllStandardError()).trimmed();
        return fail(stderr.isEmpty() ? uiTr("Reading the text failed.") : stderr);
    }
    // The recognition came back: the mode starts, or -- for an engine that
    // reported no positions -- the whole text is copied here and now.
    if (!enterTextSelection(document, error)) {
        return fail(error != nullptr ? *error : QString());
    }
    if (textResultCallback_) {
        // The mode being up means nothing has been copied yet -- the button
        // waits for the gesture that picks a range.  The no-positions fallback
        // is the other way to get here, and that one did copy.
        textResultCallback_(textMode() ? TextOutcome::Idle : TextOutcome::Copied, QString());
    }
    return true;
}

bool OverlayController::enterTextSelection(const QByteArray &document, QString *error)
{
    if (!canPaste()) {
        if (error != nullptr) {
            *error = uiTr("Reading text needs a selection to read from.");
        }
        return false;
    }
    const LogicalRect &canvas = *selection_;
    if (canvas.isEmpty()) {
        if (error != nullptr) {
            *error = uiTr("The selection is empty.");
        }
        return false;
    }
    const int index = outputContaining(canvas);
    if (index < 0 || index >= session_.outputs.size()) {
        if (error != nullptr) {
            *error = uiTr("The selection is on no output.");
        }
        return false;
    }
    const OutputSession &output = session_.outputs.at(index);
    if (output.image.isNull()) {
        if (error != nullptr) {
            *error = uiTr("The captured frame is not available.");
        }
        return false;
    }
    // The same crop `beginTextSelection` recognized: the engine's coordinates
    // are counted from it, so its top-left in the overlay's own logical pixels
    // is where the layer is placed.
    const QRect source = sourceRect(output, canvas)
                             .intersected(QRect(0, 0, output.image.width(), output.image.height()));
    if (source.isEmpty()) {
        if (error != nullptr) {
            *error = uiTr("The selection has no pixels on this output.");
        }
        return false;
    }

    const LogicalRect sourceLogical = logicalFromSource(output, source);
    TextLayerPlacement placement;
    placement.scale = static_cast<double>(output.scale);
    placement.originX = sourceLogical.x;
    placement.originY = sourceLogical.y;

    QString parseError;
    std::optional<TextLayer> layer = TextLayer::fromJson(document, placement, &parseError);
    if (!layer.has_value()) {
        if (error != nullptr) {
            *error = uiTr("Reading the text failed.");
        }
        // The parser's own message says what was actually wrong with the
        // document, which is a diagnostic rather than something to show.
        std::fprintf(stderr, "vshot-qt-ui: %s\n", parseError.toUtf8().constData());
        std::fflush(stderr);
        return false;
    }
    if (layer->hasGeometry()) {
        textLayer_ = std::move(layer);
        textMode_ = true;
        textDragging_ = false;
        // The whole layer starts selected.  The user asked for the text and
        // usually wants all of it, so the copy is one key or one more click
        // away; a drag narrows the range from there.  A layer whose lines
        // carry no characters at all is left with nothing selected, which is
        // the one case where the mode still starts empty.
        selectTextRange(0, textLayer_->count() - 1);
        // A label being typed would take the keys the mode now needs.
        if (textEdit_ != nullptr) {
            finishText(false);
        }
        updateTextModeCursor();
        updateAll();
        return true;
    }
    // An external engine reports text and no positions, so there is nothing to
    // select: the whole text is handed over the way this used to be the only
    // thing it did.
    if (layer->plainText().isEmpty()) {
        if (error != nullptr) {
            *error = uiTr("No text was found in the selection.");
        }
        return false;
    }
    std::fprintf(stderr, "vshot-qt-ui: the engine reported no character positions; "
                         "copying the whole text instead\n");
    std::fflush(stderr);
    if (!writeClipboard(layer->plainText())) {
        if (error != nullptr) {
            *error = uiTr("Cannot copy the text to the clipboard.");
        }
        return false;
    }
    return true;
}

void OverlayController::leaveTextMode()
{
    if (!textMode_) {
        return;
    }
    textMode_ = false;
    textLayer_.reset();
    textAnchor_ = -1;
    textFocus_ = -1;
    textDragging_ = false;
    // A triple-click run cannot outlive the mode: the next entry starts a
    // fresh one.
    textClickClock_.invalidate();
    updateTextModeCursor();
    updateAll();
}

QString OverlayController::selectedText() const
{
    if (!textMode_ || !textLayer_.has_value() || textAnchor_ < 0 || textFocus_ < 0) {
        return QString();
    }
    return textLayer_->rangeText(textAnchor_, textFocus_);
}

void OverlayController::setTextResultCallback(
    std::function<void(TextOutcome, const QString &)> callback)
{
    textResultCallback_ = std::move(callback);
}

void OverlayController::setClipboardWriter(std::function<bool(const QString &)> writer)
{
    clipboardWriter_ = std::move(writer);
}

bool OverlayController::writeClipboard(const QString &text)
{
    if (clipboardWriter_) {
        return clipboardWriter_(text);
    }
    return runWlCopy(text);
}

void OverlayController::selectTextRange(int anchor, int focus)
{
    if (!textLayer_.has_value() || textLayer_->count() == 0) {
        textAnchor_ = -1;
        textFocus_ = -1;
        return;
    }
    const int last = textLayer_->count() - 1;
    textAnchor_ = std::clamp(anchor, 0, last);
    textFocus_ = std::clamp(focus, 0, last);
}

void OverlayController::textSelectAll()
{
    if (!textLayer_.has_value() || textLayer_->count() == 0) {
        return;
    }
    selectTextRange(0, textLayer_->count() - 1);
    updateAll();
}

void OverlayController::textSelectWord(int index)
{
    if (!textLayer_.has_value() || index < 0) {
        return;
    }
    int first = -1;
    int last = -1;
    textLayer_->wordRange(index, &first, &last);
    if (first < 0) {
        return;
    }
    selectTextRange(first, last);
    updateAll();
}

void OverlayController::textSelectLine(int index)
{
    if (!textLayer_.has_value() || index < 0) {
        return;
    }
    int first = -1;
    int last = -1;
    textLayer_->lineRange(index, &first, &last);
    if (first < 0) {
        return;
    }
    selectTextRange(first, last);
    updateAll();
}

void OverlayController::copyTextSelection()
{
    if (!textMode_ || !textLayer_.has_value()) {
        return;
    }
    const QString text = selectedText();
    if (text.isEmpty()) {
        // Nothing is selected, so there is nothing to copy and the mode stays
        // up for the user to try again.
        if (textResultCallback_) {
            textResultCallback_(TextOutcome::Failed, uiTr("No text is selected."));
        }
        return;
    }
    if (!writeClipboard(text)) {
        // The selection is not lost: the copy can be tried again.
        if (textResultCallback_) {
            textResultCallback_(TextOutcome::Failed, uiTr("Cannot copy the text to the clipboard."));
        }
        return;
    }
    if (textResultCallback_) {
        textResultCallback_(TextOutcome::Copied, QString());
    }
    leaveTextMode();
}

void OverlayController::updateTextModeCursor()
{
    const Qt::CursorShape shape = textMode_ ? Qt::IBeamCursor : Qt::CrossCursor;
    for (CaptureOverlay *overlay : overlays_) {
        overlay->setCursor(shape);
    }
}

bool OverlayController::pasteFromFile(QString *error)
{
    if (!canPaste()) {
        if (error != nullptr) {
            *error = uiTr("Paste needs a selection to paste onto.");
        }
        return false;
    }
    // The helper is this program.  It runs the dialog in a process of its own
    // because that process has to be free of this one's event loop, not because
    // the dialog is a different kind of window: it is a layer surface like this
    // overlay, mapped after it, which is what puts it above the frozen frame
    // instead of behind it.
    char buffer[4096];
    const ssize_t length = ::readlink("/proc/self/exe", buffer, sizeof(buffer) - 1);
    if (length <= 0) {
        if (error != nullptr) {
            *error = uiTr("Cannot locate the vshot helper to open the file dialog.");
        }
        return false;
    }
    buffer[length] = '\0';
    const QString helper = QString::fromLocal8Bit(buffer);
    // The dialog opens on the output the user is annotating, so it lands in
    // front of them rather than on whichever screen the compositor favours.
    const int outputIndex = outputIndexForSelection();
    const QString outputName = outputIndex >= 0 && outputIndex < session_.outputs.size()
        ? session_.outputs.at(outputIndex).name
        : QString();
    // The controller is not a QObject, so the watcher is parented to the
    // application: it has to outlive this call, and the overlay's own widgets
    // can be torn down while the dialog is still up.
    auto *dialog = new QProcess(qApp);
    dialog->setProgram(helper);
    dialog->setArguments({QStringLiteral("--open-dialog"), QString(), outputName});
    dialog->setStandardInputFile(QProcess::nullDevice());
    QObject::connect(dialog, &QProcess::finished, dialog,
                     [this, dialog](int code, QProcess::ExitStatus) {
                         const QByteArray out = dialog->readAllStandardOutput();
                         dialog->deleteLater();
                         QJsonParseError parseError;
                         const QJsonDocument document =
                             QJsonDocument::fromJson(out.trimmed(), &parseError);
                         if (code != 0 || parseError.error != QJsonParseError::NoError
                             || !document.isObject()
                             || !document.object().value(QStringLiteral("ok")).toBool()) {
                             return; // cancelled
                         }
                         const QString path =
                             document.object().value(QStringLiteral("path")).toString();
                         if (path.isEmpty()) {
                             return;
                         }
                         QImageReader reader(path);
                         const QImage image = reader.read();
                         if (image.isNull()) {
                             std::fprintf(stderr, "vshot-qt-ui: cannot read image `%s`\n",
                                          qPrintable(path));
                             std::fflush(stderr);
                             return;
                         }
                         // The session may have been confirmed or cancelled
                         // while the dialog was up; pasteImage checks that
                         // itself and simply does nothing then.
                         pasteImage(image, path);
                     });
    QObject::connect(dialog, &QProcess::errorOccurred, dialog,
                     [dialog](QProcess::ProcessError failure) {
                         if (failure != QProcess::FailedToStart) {
                             return;
                         }
                         std::fprintf(stderr,
                                      "vshot-qt-ui: could not start the file dialog\n");
                         std::fflush(stderr);
                         dialog->deleteLater();
                     });
    dialog->start();
    return true;
}

bool OverlayController::pasteFromClipboard(QString *error)
{
    if (!canPaste()) {
        if (error != nullptr) {
            *error = uiTr("Paste needs a selection to paste onto.");
        }
        return false;
    }
    const ClipboardImage clipboard = readClipboardImage();
    if (clipboard.installed == false) {
        if (error != nullptr) {
            *error = uiTr("`wl-paste` was not found, so the clipboard cannot be read.");
        }
        return false;
    }
    if (!clipboard.image.isNull()) {
        return pasteImage(clipboard.image, clipboard.source);
    }
    if (error != nullptr) {
        // Naming what was actually wrong is the difference between "the
        // shortcut does nothing" and a user knowing to copy an image instead.
        *error = clipboard.offered
                     ? uiTr("The clipboard holds no image.")
                     : uiTr("The clipboard is empty.");
    }
    return false;
}

void OverlayController::beginStyleAdjustment()
{
    if (styleAdjustmentActive_ || selectedAnnotation_ < 0 ||
        selectedAnnotation_ >= annotations_.size()) {
        return;
    }
    styleAdjustmentActive_ = true;
    styleAdjustmentChanged_ = false;
    styleAdjustmentSnapshot_ = annotations_;
}

void OverlayController::endStyleAdjustment()
{
    if (!styleAdjustmentActive_) {
        return;
    }
    styleAdjustmentActive_ = false;
    if (!styleAdjustmentChanged_) {
        styleAdjustmentSnapshot_.clear();
        return;
    }
    // The live annotation was updated without history during the drag; commit
    // the original snapshot as one undo step when the slider is released.
    undoStack_.push_back(std::move(styleAdjustmentSnapshot_));
    if (undoStack_.size() > kMaxUndoSteps) {
        undoStack_.removeFirst();
    }
    redoStack_.clear();
    styleAdjustmentSnapshot_.clear();
    updateAll();
}

QString OverlayController::styleTargetTool() const
{
    if (selectedAnnotation_ >= 0 && selectedAnnotation_ < annotations_.size()) {
        const Annotation &annotation = annotations_.at(selectedAnnotation_);
        if (isNumberAnnotation(annotation)) {
            // A selected badge restyles as a badge, not as a label: the style
            // row offers the four badge looks rather than the font picker.
            return QStringLiteral("number");
        }
        if (annotation.kind == Annotation::Kind::Text) {
            return QStringLiteral("text");
        }
        return annotation.tool;
    }
    return toolName(tool_);
}

void OverlayController::applyStyleToSelected(
    const std::function<void(Annotation &)> &mutate)
{
    if (selectedAnnotation_ < 0 || selectedAnnotation_ >= annotations_.size()) {
        return;
    }
    QVector<Annotation> next = annotations_;
    mutate(next[selectedAnnotation_]);
    if (annotationEquals(next.at(selectedAnnotation_),
                         annotations_.at(selectedAnnotation_))) {
        return;
    }
    if (styleAdjustmentActive_) {
        annotations_ = std::move(next);
        styleAdjustmentChanged_ = true;
        updateAll();
        return;
    }
    mutateAnnotations(std::move(next));
}

int OverlayController::annotationHitAt(Point point) const
{
    const auto distanceToSegment = [](const Point &value, const Point &first,
                                      const Point &second) {
        const double vx = static_cast<double>(second.x - first.x);
        const double vy = static_cast<double>(second.y - first.y);
        const double wx = static_cast<double>(value.x - first.x);
        const double wy = static_cast<double>(value.y - first.y);
        const double lengthSquared = vx * vx + vy * vy;
        const double projection = lengthSquared > 0.0
            ? std::clamp((wx * vx + wy * vy) / lengthSquared, 0.0, 1.0)
            : 0.0;
        const double dx = static_cast<double>(value.x - first.x) - projection * vx;
        const double dy = static_cast<double>(value.y - first.y) - projection * vy;
        return std::hypot(dx, dy);
    };
    for (int index = annotations_.size() - 1; index >= 0; --index) {
        const Annotation &annotation = annotations_.at(index);
        LogicalRect bounds;
        if (!annotationBounds(annotation, &bounds)) {
            continue;
        }
        const double tolerance = 4.0 +
            (annotation.kind == Annotation::Kind::Text ? 0.0 : annotation.width / 2.0);
        if (annotation.kind == Annotation::Kind::Shape) {
            if (annotation.tool != QStringLiteral("ellipse")) {
                if (point.x >= bounds.x - tolerance && point.x < bounds.right() + tolerance &&
                    point.y >= bounds.y - tolerance && point.y < bounds.bottom() + tolerance) {
                    return index;
                }
            } else {
                const double cx = bounds.x + bounds.width / 2.0;
                const double cy = bounds.y + bounds.height / 2.0;
                const double rx = std::max(1.0, bounds.width / 2.0 + tolerance);
                const double ry = std::max(1.0, bounds.height / 2.0 + tolerance);
                const double dx = (point.x - cx) / rx;
                const double dy = (point.y - cy) / ry;
                if (dx * dx + dy * dy <= 1.0) {
                    return index;
                }
            }
            continue;
        }
        if (annotation.kind == Annotation::Kind::Image) {
            // A pasted image is its own box: hit anywhere inside the rect it was
            // placed at, so it can be picked up and moved like any other mark.
            // Without this arm it would fall through to the stroke walk below
            // and never match -- a pasted image has no points -- which left the
            // paste impossible to select or drag.
            if (point.x >= bounds.x && point.x < bounds.right() &&
                point.y >= bounds.y && point.y < bounds.bottom()) {
                return index;
            }
            continue;
        }
        if (annotation.kind == Annotation::Kind::Text) {
            if (point.x >= bounds.x - tolerance && point.x < bounds.right() + tolerance &&
                point.y >= bounds.y - tolerance && point.y < bounds.bottom() + tolerance) {
                return index;
            }
            continue;
        }
        if (annotation.points.isEmpty()) {
            continue;
        }
        const double radius = annotation.tool == QStringLiteral("mosaic")
            ? std::max(1.0, annotation.width / 2.0)
            : std::max(1.0, annotation.width / 2.0);
        if (annotation.tool == QStringLiteral("wave") && annotation.points.size() >= 2) {
            // The ink of a wave is the sampled polyline, not the straight line
            // between its two points: a click on a crest has to reach the wave,
            // or a mark it plainly covers could not be selected.
            const Point &first = annotation.points.constFirst();
            const Point &last = annotation.points.constLast();
            const QVector<QPointF> wave =
                wavePolyline(QPointF(first.x, first.y), QPointF(last.x, last.y),
                             static_cast<int>(annotation.width), 1.0);
            for (int segment = 1; segment < wave.size(); ++segment) {
                const auto toPoint = [](const QPointF &value) {
                    return Point{static_cast<std::int32_t>(std::lround(value.x())),
                                 static_cast<std::int32_t>(std::lround(value.y()))};
                };
                if (distanceToSegment(point, toPoint(wave.at(segment - 1)),
                                      toPoint(wave.at(segment))) <= radius + 4.0) {
                    return index;
                }
            }
            continue;
        }
        if (annotation.tool == QStringLiteral("bezier")) {
            // A closed path is a solid mark: its fill reaches the inside, so a
            // click there selects it, exactly as it does for a badge.
            if (annotation.closed &&
                bezierPath(annotation.points, true)
                    .contains(QPointF(point.x, point.y))) {
                return index;
            }
            // Otherwise the ink is the sampled curve, not the anchors: a click
            // on a bulge the handles pulled out has to reach the path.
            const QVector<QPointF> curve = bezierPolyline(annotation.points, annotation.closed);
            const auto toPoint = [](const QPointF &value) {
                return Point{static_cast<std::int32_t>(std::lround(value.x())),
                             static_cast<std::int32_t>(std::lround(value.y()))};
            };
            if (curve.size() == 1) {
                const Point only = toPoint(curve.constFirst());
                if (distanceToSegment(point, only, only) <= radius + 4.0) {
                    return index;
                }
            }
            for (int segment = 1; segment < curve.size(); ++segment) {
                if (distanceToSegment(point, toPoint(curve.at(segment - 1)),
                                      toPoint(curve.at(segment))) <= radius + 4.0) {
                    return index;
                }
            }
            continue;
        }
        for (int segment = 1; segment < annotation.points.size(); ++segment) {
            if (distanceToSegment(point, annotation.points.at(segment - 1),
                                  annotation.points.at(segment)) <= radius + 4.0) {
                return index;
            }
        }
        if (annotation.points.size() == 1 &&
            distanceToSegment(point, annotation.points.constFirst(),
                              annotation.points.constFirst()) <= radius + 4.0) {
            return index;
        }
        if (annotation.tool == QStringLiteral("arrow") && annotation.points.size() >= 2) {
            const Point &start = annotation.points.at(annotation.points.size() - 2);
            const Point &end = annotation.points.constLast();
            const double dx = static_cast<double>(end.x - start.x);
            const double dy = static_cast<double>(end.y - start.y);
            const double length = std::hypot(dx, dy);
            if (length > 0.0) {
                const double head = std::min(
                    std::max(6.0, static_cast<double>(annotation.width) * 4.0) * annotation.size,
                    length);
                const double wing = std::max(head * 0.55, static_cast<double>(annotation.width));
                const Point base{
                    static_cast<std::int32_t>(std::lround(end.x - dx / length * head)),
                    static_cast<std::int32_t>(std::lround(end.y - dy / length * head)),
                };
                const Point left{
                    static_cast<std::int32_t>(std::lround(base.x - dy / length * wing)),
                    static_cast<std::int32_t>(std::lround(base.y + dx / length * wing)),
                };
                const Point right{
                    static_cast<std::int32_t>(std::lround(base.x + dy / length * wing)),
                    static_cast<std::int32_t>(std::lround(base.y - dx / length * wing)),
                };
                if (distanceToSegment(point, end, left) <= radius + 4.0 ||
                    distanceToSegment(point, end, right) <= radius + 4.0) {
                    return index;
                }
            }
        }
    }
    return -1;
}

int OverlayController::annotationHandleAt(Point point) const
{
    if (selectedAnnotation_ < 0 || selectedAnnotation_ >= annotations_.size()) {
        return 0;
    }
    const Annotation &annotation = annotations_.at(selectedAnnotation_);
    if (annotation.kind == Annotation::Kind::Text) {
        return 0; // text annotations move but never resize
    }
    LogicalRect bounds;
    if (!annotationBounds(annotation, &bounds)) {
        return 0;
    }
    const std::int64_t left = bounds.x;
    const std::int64_t top = bounds.y;
    const std::int64_t rightEdge = bounds.right() - 1;
    const std::int64_t bottomEdge = bounds.bottom() - 1;
    const auto close = [](std::int64_t first, std::int64_t second) {
        return std::abs(first - second) <= kHandleRadius;
    };
    const bool nearLeft = close(point.x, left);
    const bool nearRight = close(point.x, rightEdge);
    const bool nearTop = close(point.y, top);
    const bool nearBottom = close(point.y, bottomEdge);
    if (nearLeft && nearTop) {
        return 1;
    }
    if (nearTop && close(point.x, (left + rightEdge) / 2)) {
        return 2;
    }
    if (nearRight && nearTop) {
        return 3;
    }
    if (nearRight && close(point.y, (top + bottomEdge) / 2)) {
        return 4;
    }
    if (nearRight && nearBottom) {
        return 5;
    }
    if (nearBottom && close(point.x, (left + rightEdge) / 2)) {
        return 6;
    }
    if (nearLeft && nearBottom) {
        return 7;
    }
    if (nearLeft && close(point.y, (top + bottomEdge) / 2)) {
        return 8;
    }
    return 0;
}

void OverlayController::selectAnnotation(int index)
{
    selectedAnnotation_ = index >= 0 && index < annotations_.size() ? index : -1;
    updateAll();
}

void OverlayController::deleteSelectedAnnotation()
{
    if (selectedAnnotation_ < 0 || selectedAnnotation_ >= annotations_.size()) {
        return;
    }
    QVector<Annotation> next = annotations_;
    next.remove(selectedAnnotation_);
    selectedAnnotation_ = -1;
    mutateAnnotations(std::move(next));
}

void OverlayController::beginAnnotationDrag(Point point, bool resize)
{
    if (selectedAnnotation_ < 0 || selectedAnnotation_ >= annotations_.size()) {
        return;
    }
    gesture_->type = resize ? Gesture::Type::ResizingAnnotation
                            : Gesture::Type::MovingAnnotation;
    gesture_->anchor = point;
    gesture_->current = point;
    gesture_->handle = resize ? annotationHandleAt(point) : 0;
    dragAnnotation_ = annotations_.at(selectedAnnotation_);
    dragSnapshot_ = annotations_;
    dragMoved_ = false;
}

void OverlayController::updateAnnotationDrag(Point point)
{
    if (selectedAnnotation_ < 0 || selectedAnnotation_ >= annotations_.size()) {
        gesture_->type = Gesture::Type::None;
        return;
    }
    const Point current = clampPoint(point);
    gesture_->current = current;
    if (!dragMoved_) {
        const std::int64_t dx = static_cast<std::int64_t>(current.x) - gesture_->anchor.x;
        const std::int64_t dy = static_cast<std::int64_t>(current.y) - gesture_->anchor.y;
        if (dx * dx + dy * dy <= 16) {
            return; // stay below the drag threshold: a plain click selects the mark
        }
        dragMoved_ = true;
    }
    if (gesture_->type == Gesture::Type::MovingAnnotation) {
        const int dx = current.x - gesture_->anchor.x;
        const int dy = current.y - gesture_->anchor.y;
        annotations_[selectedAnnotation_] = translatedAnnotation(dragAnnotation_, dx, dy);
    } else {
        LogicalRect originalBounds;
        if (!annotationBounds(dragAnnotation_, &originalBounds)) {
            return;
        }
        const LogicalRect newBounds = resizeSelection(originalBounds, gesture_->handle, current);
        annotations_[selectedAnnotation_] = scaledAnnotation(dragAnnotation_, newBounds);
    }
}

void OverlayController::finishAnnotationDrag(CaptureOverlay *overlay, Point point)
{
    Q_UNUSED(overlay);
    updateAnnotationDrag(point);
    const bool moved = dragMoved_;
    gesture_->type = Gesture::Type::None;
    if (!moved) {
        // A plain click selects every annotation.  Text editing is explicitly a
        // double-click action so a selected label can still receive style edits.
        return;
    }
    undoStack_.push_back(dragSnapshot_);
    if (undoStack_.size() > kMaxUndoSteps) {
        undoStack_.removeFirst();
    }
    redoStack_.clear();
    updateAll();
}

Annotation OverlayController::translatedAnnotation(const Annotation &original, int dx,
                                                    int dy) const
{
    int clampedDx = dx;
    int clampedDy = dy;
    LogicalRect bounds;
    // The mark may only travel inside the area it is drawn on. In pin-edit
    // mode that is the image, which moves with the user's drags — clamping
    // against the session bounds there would yank every mark back toward the
    // image's original position, which reads as the mark vanishing.
    const LogicalRect &limits = annotationLimits();
    if (annotationBounds(original, &bounds)) {
        const std::int64_t minDx = limits.x - bounds.x;
        const std::int64_t maxDx = limits.right() - bounds.right();
        const std::int64_t minDy = limits.y - bounds.y;
        const std::int64_t maxDy = limits.bottom() - bounds.bottom();
        // A mark wider or taller than the area it sits on cannot be confined
        // to it: move it freely rather than snapping it to one edge.
        if (minDx <= maxDx) {
            clampedDx = static_cast<int>(std::clamp<std::int64_t>(dx, minDx, maxDx));
        }
        if (minDy <= maxDy) {
            clampedDy = static_cast<int>(std::clamp<std::int64_t>(dy, minDy, maxDy));
        }
    }
    Annotation result = original;
    switch (original.kind) {
    case Annotation::Kind::Shape:
        result.rect.x = static_cast<std::int32_t>(result.rect.x + clampedDx);
        result.rect.y = static_cast<std::int32_t>(result.rect.y + clampedDy);
        break;
    case Annotation::Kind::Image:
        // A pasted image moves as a whole, the same way a shape does.
        result.rect.x = static_cast<std::int32_t>(result.rect.x + clampedDx);
        result.rect.y = static_cast<std::int32_t>(result.rect.y + clampedDy);
        break;
    case Annotation::Kind::Stroke:
        for (Point &point : result.points) {
            point.x = static_cast<std::int32_t>(point.x + clampedDx);
            point.y = static_cast<std::int32_t>(point.y + clampedDy);
        }
        break;
    case Annotation::Kind::Text:
        result.origin.x = static_cast<std::int32_t>(result.origin.x + clampedDx);
        result.origin.y = static_cast<std::int32_t>(result.origin.y + clampedDy);
        if (isNumberAnnotation(result)) {
            // A badge's box is its content rather than a label box derived from
            // the text, so it has to travel with the badge -- the hit test and
            // the raster bounds are read from it.
            result.rect.x = static_cast<std::int32_t>(result.rect.x + clampedDx);
            result.rect.y = static_cast<std::int32_t>(result.rect.y + clampedDy);
        }
        break;
    }
    return result;
}

Annotation OverlayController::scaledAnnotation(const Annotation &original,
                                               const LogicalRect &newBounds) const
{
    Annotation result = original;
    if (original.kind == Annotation::Kind::Shape || original.kind == Annotation::Kind::Image) {
        // A shape's rect *is* its geometry, and a pasted image's rect is where
        // it sits and how big it is; both scale by taking the new rect.
        result.rect = newBounds;
        return result;
    }
    LogicalRect oldBounds;
    if (!annotationBounds(original, &oldBounds) || oldBounds.width == 0 ||
        oldBounds.height == 0) {
        return result;
    }
    const double spanX = oldBounds.width > 1 ?
        static_cast<double>(newBounds.width - 1) / static_cast<double>(oldBounds.width - 1) : 0.0;
    const double spanY = oldBounds.height > 1 ?
        static_cast<double>(newBounds.height - 1) / static_cast<double>(oldBounds.height - 1) : 0.0;
    for (Point &point : result.points) {
        point.x = static_cast<std::int32_t>(std::lround(
            newBounds.x + (static_cast<double>(point.x) - oldBounds.x) * spanX));
        point.y = static_cast<std::int32_t>(std::lround(
            newBounds.y + (static_cast<double>(point.y) - oldBounds.y) * spanY));
    }
    return result;
}

bool OverlayController::hasValidSelection() const
{
    return selection_.has_value() && selection_->width >= kMinimumSelection &&
           selection_->height >= kMinimumSelection;
}

bool OverlayController::canRequestLongCapture() const
{
    if (!longAllowed_ || finished_ || cancelled_ || !hasValidSelection()) {
        return false;
    }
    // A scrolling capture needs the region inside a single output; the CLI
    // resolves the output from the region and refuses anything wider.
    for (const OutputSession &output : session_.outputs) {
        const LogicalRect &surface = output.surface;
        if (selection_->x >= surface.x && selection_->y >= surface.y &&
            selection_->right() <= surface.right() &&
            selection_->bottom() <= surface.bottom()) {
            return true;
        }
    }
    return false;
}

void OverlayController::requestLongCapture()
{
    if (finished_ || cancelled_ || !canRequestLongCapture()) {
        return;
    }
    longRequested_ = true;
    terminal(false);
}

void OverlayController::beginPinEdit()
{
    if (!pinEdit_ || finished_ || cancelled_) {
        return;
    }
    // The editable canvas is the whole pin image: the session bounds (the
    // pinned image) is preselected, and the surface around it stays bare so
    // the toolbar can sit beside the image like a region-capture toolbar.
    selection_ = LogicalRect{session_.bounds.x, session_.bounds.y, session_.bounds.width,
                             session_.bounds.height};
    editing_ = true;
    toolbarOutput_ = 0;
    showToolbar();
}

void OverlayController::beginPinEditText()
{
    // The same editor opened on the text rather than on the marks: the pin
    // image is the canvas and the whole of it is what recognition reads, so the
    // characters come back where they were for the pointer to select a range.
    beginPinEdit();
    // A recognition that fails reports through `textResultCallback_` -- the
    // toolbar's `Text+` button is on screen by now and says so -- and the
    // editor simply stays in the ordinary pin-editing state. There is no
    // second error path here.
    beginTextSelection(nullptr);
}

void OverlayController::beginPresetEdit()
{
    if (!session_.selection.has_value() || finished_ || cancelled_) {
        return;
    }
    // Start where a finished drag would leave a region session: the selection
    // is the one the picker resolved, so the toolbar is up and the frozen
    // frame under it is the canvas the user will annotate and save.
    selection_ = *session_.selection;
    editing_ = true;
    toolbarOutput_ = outputContaining(*selection_);
    showToolbar();
    updateAll();
}

int OverlayController::outputContaining(const LogicalRect &rect) const
{
    const Point center{
        rect.x + static_cast<std::int32_t>(rect.width / 2),
        rect.y + static_cast<std::int32_t>(rect.height / 2),
    };
    for (int index = 0; index < session_.outputs.size(); ++index) {
        const LogicalRect &geometry = session_.outputs.at(index).geometry;
        if (center.x >= geometry.x && center.x < geometry.right() && center.y >= geometry.y &&
            center.y < geometry.bottom()) {
            return index;
        }
    }
    // A selection that lands on no output at all (a torn-down one) keeps the
    // toolbar on the first surface instead of nowhere.
    return 0;
}

void OverlayController::confirm()
{
    if (finished_ || cancelled_) {
        return;
    }
    if (textEdit_ != nullptr) {
        finishText(true);
    }
    if (!hasValidSelection()) {
        return;
    }
    terminal(false);
}

void OverlayController::cancel()
{
    if (finished_ || cancelled_) {
        return;
    }
    if (textEdit_ != nullptr) {
        finishText(false);
    }
    terminal(true);
}

void OverlayController::terminal(bool cancelled)
{
    if (finished_ || cancelled_) {
        return;
    }
    finished_ = true;
    cancelled_ = cancelled;
    // Nothing a session did is written back to the config: everything here --
    // the tool, the colour, the width, the font size -- is the session's own
    // working state, not a preference.  The config is the *reset* value every
    // session starts from, and it changes only where the user can see and mean
    // it: the settings window (`vshot settings`) or a hand edit.  Saving here
    // would make one capture's improvisation silently redefine the next one's
    // starting point.
    hideToolbar();
    removeTextEditor();
    if (terminalCallback_) {
        terminalCallback_();
    }
}

void OverlayController::removeTextEditor()
{
    if (textEdit_ != nullptr) {
        textEdit_->hide();
        textEdit_->deleteLater();
        textEdit_ = nullptr;
    }
    textEditPixels_ = 0;
}

bool OverlayController::isFinished() const
{
    return finished_;
}

bool OverlayController::isCancelled() const
{
    return cancelled_;
}

const std::optional<LogicalRect> &OverlayController::selection() const
{
    return selection_;
}

const QVector<Annotation> &OverlayController::annotations() const
{
    return annotations_;
}

int OverlayController::liveStrokeBakes() const
{
    return liveStrokeBakes_;
}

QJsonDocument OverlayController::resultDocument(const QString &bitmapDirectory,
                                                QString *error) const
{
    QJsonObject root;
    if (cancelled_) {
        root.insert(QStringLiteral("status"), QStringLiteral("cancelled"));
        return QJsonDocument(root);
    }
    root.insert(QStringLiteral("status"), QStringLiteral("ok"));
    QJsonObject selection;
    if (selection_.has_value()) {
        selection.insert(QStringLiteral("x"), static_cast<qint64>(selection_->x));
        selection.insert(QStringLiteral("y"), static_cast<qint64>(selection_->y));
        selection.insert(QStringLiteral("width"), static_cast<qint64>(selection_->width));
        selection.insert(QStringLiteral("height"), static_cast<qint64>(selection_->height));
    }
    root.insert(QStringLiteral("selection"), selection);
    // The scrolling-capture action ends the session like a confirmation, so
    // the answer travels here rather than in `status`.
    if (longRequested_) {
        root.insert(QStringLiteral("long"), true);
    }
    // Picking reports the click position as well: it runs on a live desktop,
    // so the caller re-resolves there which window the click actually landed
    // on before it captures the frame.
    if (pickMode_) {
        QJsonObject point;
        point.insert(QStringLiteral("x"), static_cast<qint64>(pointer_.x));
        point.insert(QStringLiteral("y"), static_cast<qint64>(pointer_.y));
        root.insert(QStringLiteral("point"), point);
    }

    QJsonArray outputAnnotations;
    for (const Annotation &annotation : annotations_) {
        QJsonObject value;
        if (annotation.kind == Annotation::Kind::Image) {
            // The pixels travel as a raw RGBA8888 file beside the session JSON,
            // exactly like a text label's bitmap: the protocol carries paths,
            // not megabytes of base64.
            value.insert(QStringLiteral("kind"), QStringLiteral("image"));
            value.insert(QStringLiteral("tool"), QStringLiteral("image"));
            QJsonObject rect;
            rect.insert(QStringLiteral("x"), static_cast<qint64>(annotation.rect.x));
            rect.insert(QStringLiteral("y"), static_cast<qint64>(annotation.rect.y));
            rect.insert(QStringLiteral("width"), static_cast<qint64>(annotation.rect.width));
            rect.insert(QStringLiteral("height"), static_cast<qint64>(annotation.rect.height));
            value.insert(QStringLiteral("rect"), rect);
            if (bitmapDirectory.isEmpty() || annotation.pixels.isNull()) {
                // No directory to write into: the annotation cannot be handed
                // over, so it is dropped rather than reported as a mark the
                // renderer would then fail to find.
                continue;
            }
            // Rasterize at the size the image occupies on the canvas, in scene
            // device pixels -- the same contract a text bitmap is written
            // under. The source is usually a different size entirely (a photo
            // pasted small, or a screenshot pasted smaller than its pixels),
            // and rendering it here means the file is bounded by the canvas
            // rather than by the source, and that what travels is exactly what
            // the preview showed.
            const int scale = sceneScale();
            const int width = static_cast<int>(annotation.rect.width) * scale;
            const int height = static_cast<int>(annotation.rect.height) * scale;
            if (width <= 0 || height <= 0
                || static_cast<qint64>(width) * static_cast<qint64>(height) > 16LL * 1024 * 1024) {
                if (error != nullptr) {
                    *error = QStringLiteral("pasted image is too large to render (%1x%2)")
                                 .arg(width)
                                 .arg(height);
                    return QJsonDocument();
                }
                continue;
            }
            const QImage pixels = annotation.pixels
                                      .scaled(width, height, Qt::IgnoreAspectRatio,
                                              Qt::SmoothTransformation)
                                      .convertToFormat(QImage::Format_RGBA8888);
            if (pixels.isNull()) {
                continue;
            }
            const QString path = QStringLiteral("%1/image-%2.rgba")
                                     .arg(bitmapDirectory)
                                     .arg(imageBitmapIndex_++);
            QFile file(path);
            if (!file.open(QIODevice::WriteOnly | QIODevice::Truncate)
                || file.write(reinterpret_cast<const char *>(pixels.constBits()),
                              static_cast<qint64>(pixels.sizeInBytes()))
                    != static_cast<qint64>(pixels.sizeInBytes())) {
                if (error != nullptr) {
                    *error = QStringLiteral("cannot write pasted image `%1`: %2")
                                 .arg(path, file.errorString());
                    return QJsonDocument();
                }
                continue;
            }
            value.insert(QStringLiteral("bitmap_width"), static_cast<qint64>(pixels.width()));
            value.insert(QStringLiteral("bitmap_height"), static_cast<qint64>(pixels.height()));
            value.insert(QStringLiteral("bitmap"), path);
        } else if (annotation.kind == Annotation::Kind::Shape) {
            value.insert(QStringLiteral("kind"), QStringLiteral("shape"));
            value.insert(QStringLiteral("tool"), annotation.tool);
            value.insert(QStringLiteral("color"), colorText(annotation.color));
            value.insert(QStringLiteral("width"), static_cast<qint64>(annotation.width));
            value.insert(QStringLiteral("dash"), annotation.dash);
            if (annotation.tool == QStringLiteral("mosaic")) {
                value.insert(QStringLiteral("mask"), annotation.mask);
                value.insert(QStringLiteral("strength"),
                             static_cast<qint64>(annotation.strength));
            }
            QJsonObject rect;
            rect.insert(QStringLiteral("x"), static_cast<qint64>(annotation.rect.x));
            rect.insert(QStringLiteral("y"), static_cast<qint64>(annotation.rect.y));
            rect.insert(QStringLiteral("width"), static_cast<qint64>(annotation.rect.width));
            rect.insert(QStringLiteral("height"), static_cast<qint64>(annotation.rect.height));
            value.insert(QStringLiteral("rect"), rect);
        } else if (annotation.kind == Annotation::Kind::Stroke) {
            value.insert(QStringLiteral("kind"), QStringLiteral("stroke"));
            value.insert(QStringLiteral("tool"), annotation.tool);
            value.insert(QStringLiteral("color"), colorText(annotation.color));
            value.insert(QStringLiteral("width"), static_cast<qint64>(annotation.width));
            value.insert(QStringLiteral("dash"), annotation.dash);
            if (annotation.tool == QStringLiteral("arrow")) {
                value.insert(QStringLiteral("size"), static_cast<qint64>(annotation.size));
                value.insert(QStringLiteral("arrow_style"), annotation.arrowStyle);
            }
            if (annotation.tool == QStringLiteral("mosaic")) {
                value.insert(QStringLiteral("strength"),
                             static_cast<qint64>(annotation.strength));
            }
            if (annotation.tool == QStringLiteral("bezier")) {
                // The pen path's closure, and the one field that makes the two
                // ends of the protocol agree: the renderer fills a closed path
                // before it strokes it.  Only a bezier carries it, the same way
                // only an arrow carries `size`.
                value.insert(QStringLiteral("closed"), annotation.closed);
            }
            QJsonArray points;
            for (const Point &point : annotation.points) {
                QJsonObject item;
                item.insert(QStringLiteral("x"), static_cast<qint64>(point.x));
                item.insert(QStringLiteral("y"), static_cast<qint64>(point.y));
                points.push_back(item);
            }
            value.insert(QStringLiteral("points"), points);
        } else {
            const bool number = isNumberAnnotation(annotation);
            value.insert(QStringLiteral("kind"), QStringLiteral("text"));
            if (number) {
                // The number tool rides the text annotation, and the one extra
                // name is what tells the two apart on the way out.  The renderer
                // does not read it -- it composites the bitmap like any other
                // text annotation -- which is why the Rust side needs no change
                // at all to carry a numbered badge.
                value.insert(QStringLiteral("tool"), QStringLiteral("number"));
            }
            QJsonObject origin;
            origin.insert(QStringLiteral("x"), static_cast<qint64>(annotation.origin.x));
            origin.insert(QStringLiteral("y"), static_cast<qint64>(annotation.origin.y));
            value.insert(QStringLiteral("origin"), origin);
            value.insert(QStringLiteral("text"),
                         number ? QString::number(annotation.number) : annotation.text);
            // The protocol still carries the legacy integer glyph multiple;
            // it is derived from the pixel size here and nowhere else.  A
            // helper that ships a bitmap below renders the exact size, so this
            // only ever reaches the Rust fallback font.
            value.insert(QStringLiteral("scale"), static_cast<qint64>(textPixelsToScale(
                                                     static_cast<int>(annotation.textPixels))));
            value.insert(QStringLiteral("color"), colorText(annotation.color));
            if (!annotation.font.isEmpty()) {
                value.insert(QStringLiteral("font"), annotation.font);
            }
            // Rasterize the label -- or the badge -- so the final PNG matches
            // what the user saw. The bitmap is rendered in scene device pixels
            // (the highest output scale), matching how Rust composites it onto
            // the cropped frame; the renderer is handed that same density, so
            // the composite is a straight blit at 1:1 rather than a resample.
            if (!bitmapDirectory.isEmpty()) {
                const int scale = sceneScale();
                // A label's box comes from its glyphs; a badge's is the box it
                // was placed with, which is also where its ink is drawn.
                const QSize logical =
                    number ? QSize(static_cast<int>(annotation.rect.width),
                                   static_cast<int>(annotation.rect.height))
                           : textMetrics(annotation);
                const int width = logical.width() * scale;
                const int height = logical.height() * scale;
                if (width > 0 && height > 0 &&
                    static_cast<qint64>(width) * static_cast<qint64>(height) <=
                        16LL * 1024 * 1024) {
                    QImage bitmap(width, height, QImage::Format_RGBA8888);
                    if (!bitmap.isNull()) {
                        bitmap.fill(Qt::transparent);
                        QPainter bitmapPainter(&bitmap);
                        bitmapPainter.setRenderHint(QPainter::Antialiasing, true);
                        bitmapPainter.setRenderHint(QPainter::TextAntialiasing, true);
                        if (number) {
                            // The badge is drawn at the scene's density, into
                            // the whole bitmap: its top-left is the annotation's
                            // own origin, which is exactly where the renderer
                            // blits the file.
                            bitmapPainter.scale(scale, scale);
                            paintNumberBadge(bitmapPainter,
                                             QRectF(0, 0, logical.width(), logical.height()),
                                             QString::number(annotation.number),
                                             annotation.numberStyle, annotation.color,
                                             static_cast<int>(annotation.width));
                        } else {
                            QFont font = textFont(
                                annotation.font,
                                std::max(1, static_cast<int>(annotation.textPixels) * scale));
                            bitmapPainter.setFont(font);
                            bitmapPainter.setPen(annotation.color);
                            const QStringList lines = annotation.text.split(QLatin1Char('\n'));
                            const QFontMetrics metrics(font);
                            qreal y = 0.0;
                            for (const QString &line : lines) {
                                bitmapPainter.drawText(QRectF(0, y, width, metrics.height()),
                                                       Qt::AlignLeft | Qt::AlignTop, line);
                                y += metrics.lineSpacing();
                            }
                        }
                        bitmapPainter.end();
                        const QString path = QStringLiteral("%1/text-%2.rgba")
                                                 .arg(bitmapDirectory)
                                                 .arg(textBitmapIndex_++);
                        QFile file(path);
                        if (file.open(QIODevice::WriteOnly | QIODevice::Truncate) &&
                            file.write(reinterpret_cast<const char *>(bitmap.constBits()),
                                       static_cast<qint64>(bitmap.sizeInBytes())) ==
                                static_cast<qint64>(bitmap.sizeInBytes())) {
                            value.insert(QStringLiteral("bitmap_width"),
                                         static_cast<qint64>(width));
                            value.insert(QStringLiteral("bitmap_height"),
                                         static_cast<qint64>(height));
                            value.insert(QStringLiteral("bitmap"), path);
                        } else if (error != nullptr) {
                            *error = QStringLiteral("cannot write text bitmap `%1`: %2")
                                         .arg(path, file.errorString());
                            return QJsonDocument();
                        }
                    } else if (error != nullptr) {
                        *error = QStringLiteral("cannot allocate %1x%2 text bitmap")
                                     .arg(width)
                                     .arg(height);
                        return QJsonDocument();
                    }
                }
            }
        }
        outputAnnotations.push_back(value);
    }
    root.insert(QStringLiteral("annotations"), outputAnnotations);
    return QJsonDocument(root);
}

void OverlayController::setTerminalCallback(std::function<void()> callback)
{
    terminalCallback_ = std::move(callback);
}

// The rasterized form of one annotation, kept beside it between repaints.
//
// Every kind draws something different, so each one says for itself what its
// pixels depend on -- a mosaic's bounds, strength and source image; a label's
// text, font and size; a stroke's points.  The paint loop only asks the shared
// question "are the cached pixels still current?" through `paint`, so it never
// branches on the kind itself.  `Annotation` holds the cache.
class AnnotationRaster {
public:
    AnnotationRaster() = default;
    AnnotationRaster(const AnnotationRaster &) = delete;
    AnnotationRaster &operator=(const AnnotationRaster &) = delete;
    virtual ~AnnotationRaster() = default;

    // Draws the annotation into the overlay, rasterizing it first only when
    // something it draws has changed since the last time.  `outputIndex` names
    // the screen being painted: a session that spans several of them paints the
    // same marks on each, and one shared raster would be thrown away and rebuilt
    // every time the paint moved to the next screen.  Each output keeps its own.
    void paint(QPainter *painter, const Annotation &annotation, const OutputSession &output,
               const QSize &size, int outputIndex)
    {
        const int pad = padding(annotation);
        // Inflate before testing emptiness: a perfectly horizontal or vertical
        // stroke has a zero-thickness bounding box, which `isEmpty` rejects
        // even though there is a line to draw.
        //
        // The rect is deliberately not clipped to the canvas: trimming it to the
        // surface would change the raster's size as a mark slid over the edge of
        // the screen, and a size change rebuilds it.  The blit is clipped by the
        // painter anyway, so the pixels the user sees do not change.
        const QRect clip = bounds(annotation, output, size)
                               .adjusted(-pad, -pad, pad, pad)
                               .toAlignedRect();
        if (clip.isEmpty() || !clip.intersects(QRect(QPoint(0, 0), size))) {
            return;
        }
        // A repaint narrowed to what a pointer move can have changed must not
        // pay for the marks outside it: at 4K a full-frame blit is about 1.7 ms,
        // and a capture can easily carry a hundred marks.  The clip is in the
        // same logical coordinates the rect is.
        if (painter->hasClipping() && !painter->clipBoundingRect().intersects(QRectF(clip))) {
            return;
        }
        const QByteArray key = signature(annotation, output, size);
        // The raster holds device pixels, not logical ones: the preview painter
        // carries the device-pixel-ratio transform while every mark is
        // rasterized in logical coordinates, and the overlay draws with smooth
        // transforms off.  A raster built one-logical-pixel-per-pixel would
        // therefore be magnified with nearest-neighbour and blur the mark on a
        // high-DPI screen.  The ratio is part of the cache's validity, so a
        // capture that moves to another screen rebuilds it.
        const qreal ratio = deviceRatio(painter);
        const QSize device = (QSizeF(clip.size()) * ratio).toSize();
        Cache &cache = caches_[outputIndex];
        if (cache.image.size() != device || cache.ratio != ratio) {
            // Only a different size or screen ratio needs a new buffer: a
            // rebuild for a changed mark redraws into the pixels it already
            // holds instead of allocating an identical image again.
            cache.image = QImage(device, QImage::Format_ARGB32_Premultiplied);
            cache.image.setDevicePixelRatio(ratio);
            cache.ratio = ratio;
            cache.key.clear();
        }
        if (key != cache.key) {
            cache.image.fill(Qt::transparent);
            QPainter raster(&cache.image);
            raster.setRenderHint(QPainter::Antialiasing, true);
            raster.scale(ratio, ratio);
            raster.translate(-clip.topLeft());
            draw(&raster, annotation, output, size);
            cache.key = key;
            ++cache.rebuilds;
            builtAtRatio_ = ratio;
        }
        painter->drawImage(clip.topLeft(), cache.image);
    }

    // How many times the pixels have been built, over every output.  Read
    // through `Annotation::rasterRebuilds` by the offline check.
    int rebuilds() const
    {
        int total = 0;
        for (const Cache &cache : caches_) {
            total += cache.rebuilds;
        }
        return total;
    }

    // The device-pixel ratio the pixels were last built at, or 0 before the
    // first build.  Read through `Annotation::rasterDeviceRatio`.
    qreal builtAtRatio() const { return builtAtRatio_; }

    // How far outside the rect its `bounds` reports a mark's pixels can reach,
    // in logical pixels.  The controller sizes the region a drag has to
    // invalidate with it, so it cannot stay behind `protected`.
    int reach(const Annotation &annotation) const { return padding(annotation); }

    // The device-pixel ratio a raster has to be built at for this painter.  The
    // marks are rasterized in logical coordinates, so the raster has to hold
    // the device pixels the painter will actually touch.
    static qreal deviceRatio(const QPainter *painter)
    {
        const QPaintDevice *device = painter != nullptr ? painter->device() : nullptr;
        if (device == nullptr) {
            return 1.0;
        }
        const qreal ratio = device->devicePixelRatioF();
        return ratio > 0.0 ? ratio : 1.0;
    }

protected:
    // Local rect the mark covers, pens excluded.
    virtual QRectF bounds(const Annotation &annotation, const OutputSession &output,
                          const QSize &size) const = 0;
    // Everything the raster depends on; equal keys mean the cached pixels hold.
    virtual QByteArray signature(const Annotation &annotation, const OutputSession &output,
                                 const QSize &size) const = 0;
    // Draws the mark in overlay-local coordinates; the painter is already
    // translated so those coordinates match the overlay's own.
    virtual void draw(QPainter *painter, const Annotation &annotation, const OutputSession &output,
                      const QSize &size) const = 0;
    // Room around `bounds` for antialiasing and any pen or head that reaches
    // past the geometry itself.
    virtual int padding(const Annotation &annotation) const
    {
        return static_cast<int>(annotation.width) / 2 + 2;
    }

    // The key prefix every kind shares: the output the pixels were rasterized
    // against and the surface size they were rasterized for.
    static void writeContext(QDataStream &stream, const OutputSession &output, const QSize &size)
    {
        const LogicalRect &surface = surfaceOf(output);
        // The mosaic and the mosaic brush average the source image, and both
        // locate their samples through `geometry` rather than `surface`, so a
        // raster of either is only valid for the frame and the geometry it was
        // built from.  Neither changes within a session today, but leaving them
        // out of the key would freeze such a mark on stale pixels the moment
        // one does.
        stream << size.width() << size.height() << output.id << output.scale << surface.x
               << surface.y << surface.width << surface.height << output.geometry.x
               << output.geometry.y << output.geometry.width << output.geometry.height
               << output.image.cacheKey();
    }

private:
    // One cached raster per output.  `key` is empty while the buffer holds
    // nothing current, and `rebuilds` counts the builds this output needed.
    struct Cache {
        QImage image;
        QByteArray key;
        qreal ratio = 1.0;
        int rebuilds = 0;
    };
    QHash<int, Cache> caches_;
    qreal builtAtRatio_ = 0.0;
};

// Rectangles, ellipses and the area mosaic.  The mosaic averages the source
// image, so it reads the output as well as the annotation.
class ShapeRaster final : public AnnotationRaster {
protected:
    QRectF bounds(const Annotation &annotation, const OutputSession &output,
                  const QSize &size) const override
    {
        return localRect(output, annotation.rect, size);
    }

    QByteArray signature(const Annotation &annotation, const OutputSession &output,
                         const QSize &size) const override
    {
        QByteArray data;
        QDataStream stream(&data, QIODevice::WriteOnly);
        writeContext(stream, output, size);
        stream << annotation.tool << annotation.dash << annotation.width
               << static_cast<quint32>(annotation.color.rgba()) << annotation.mask
               << annotation.strength;
        // A plain shape's pixels depend only on its size and style, so a pure
        // translation leaves the cached raster valid and the blit lands it at
        // the new place.  The area mosaic instead averages the source image at
        // its absolute position, so a move changes every block it draws and its
        // top-left has to stay part of the key.
        if (annotation.tool == QStringLiteral("mosaic")) {
            stream << static_cast<qint64>(annotation.rect.x)
                   << static_cast<qint64>(annotation.rect.y);
        }
        stream << static_cast<quint64>(annotation.rect.width)
               << static_cast<quint64>(annotation.rect.height);
        return data;
    }

    void draw(QPainter *painter, const Annotation &annotation, const OutputSession &output,
              const QSize &size) const override
    {
        if (annotation.tool == QStringLiteral("mosaic")) {
            drawMosaicAnnotation(painter, output, annotation.rect, annotation.mask,
                                 annotation.strength, size);
            return;
        }
        painter->setPen(penForAnnotation(annotation));
        painter->setBrush(Qt::NoBrush);
        const QRectF rect = localRect(output, annotation.rect, size);
        if (annotation.tool == QStringLiteral("ellipse")) {
            painter->drawEllipse(rect);
        } else if (annotation.dash != QStringLiteral("solid")) {
            // Match the final renderer's band-centerline dash walk.
            painter->drawPolyline(
                insetRectPolygon(rect, static_cast<double>(annotation.width)));
        } else {
            painter->drawRect(rect);
        }
    }
};

// Freehand pen strokes, arrows and the freehand mosaic brush.
class StrokeRaster final : public AnnotationRaster {
protected:
    QRectF bounds(const Annotation &annotation, const OutputSession &output,
                  const QSize &size) const override
    {
        LogicalRect rect;
        if (!annotationLogicalBounds(annotation, &rect)) {
            return QRectF();
        }
        return localRect(output, rect, size);
    }

    QByteArray signature(const Annotation &annotation, const OutputSession &output,
                         const QSize &size) const override
    {
        QByteArray data;
        QDataStream stream(&data, QIODevice::WriteOnly);
        writeContext(stream, output, size);
        stream << annotation.tool << annotation.dash << annotation.width
               << static_cast<quint32>(annotation.color.rgba()) << annotation.size
               << annotation.arrowStyle << annotation.strength << annotation.closed;
        if (annotation.tool == QStringLiteral("mosaic")) {
            // The freehand mosaic brush averages the source image under the
            // path, so every point's absolute position has to stay in the key.
            for (const Point &point : annotation.points) {
                stream << point.x << point.y;
            }
        } else {
            // A stroke's pixels depend only on the shape of its path, not on
            // where it sits: keep the point count and each point's offset from
            // the first, so translating the whole stroke leaves the key alone.
            stream << annotation.points.size();
            if (!annotation.points.isEmpty()) {
                const Point &first = annotation.points.constFirst();
                for (qsizetype index = 1; index < annotation.points.size(); ++index) {
                    stream << (annotation.points.at(index).x - first.x)
                           << (annotation.points.at(index).y - first.y);
                }
            }
        }
        return data;
    }

    void draw(QPainter *painter, const Annotation &annotation, const OutputSession &output,
              const QSize &size) const override
    {
        if (annotation.points.isEmpty()) {
            return;
        }
        if (annotation.tool == QStringLiteral("mosaic")) {
            // Freehand mosaic brush: smear discs along the path.
            drawMosaicBrush(painter, output, annotation.points, annotation.width,
                            annotation.strength, size);
            return;
        }
        if (annotation.tool == QStringLiteral("bezier")) {
            // A pen path is a cubic per segment rather than a polyline, and a
            // closed one is filled as well as stroked.  The points are put in
            // this overlay's own coordinates first: the same helper then serves
            // the live preview and the cached raster.
            QVector<QPointF> at;
            at.reserve(annotation.points.size());
            for (const Point &point : annotation.points) {
                at.append(localPoint(output, point, size));
            }
            paintBezierInk(painter, at, annotation.closed, annotation.color,
                           static_cast<int>(annotation.width));
            return;
        }
        const double scale = output.scale > 0 ? static_cast<double>(output.scale) : 1.0;
        QPolygonF polygon;
        QPen pen = penForAnnotation(annotation);
        if (annotation.tool == QStringLiteral("wave") && annotation.points.size() >= 2) {
            // A wave is the sine sample of the segment between its two points,
            // not the segment itself: sample it here the same way the live
            // preview and the Rust renderer do, and draw it solid.
            const QVector<QPointF> wave = wavePolyline(
                localPoint(output, annotation.points.constFirst(), size),
                localPoint(output, annotation.points.constLast(), size),
                static_cast<int>(annotation.width), scale);
            polygon = QPolygonF(wave.begin(), wave.end());
            pen = wavePen(annotation);
        } else {
            for (const Point &point : annotation.points) {
                polygon.push_back(localPoint(output, point, size));
            }
        }
        painter->setPen(pen);
        painter->drawPolyline(polygon);
        if (annotation.tool != QStringLiteral("arrow") || polygon.size() < 2) {
            return;
        }
        // The head is always solid and scales with the annotation's size.
        painter->setPen(QPen(annotation.color, static_cast<double>(annotation.width),
                             Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
        const QPointF end = polygon.constLast();
        const QPointF start = polygon.at(polygon.size() - 2);
        const QLineF line(start, end);
        if (line.length() <= 1.0) {
            return;
        }
        const double widthDevice = static_cast<double>(annotation.width) * scale;
        const double headDevice = std::min(std::max(6.0, widthDevice * 4.0) *
                                               static_cast<double>(annotation.size),
                                           line.length() * scale);
        const double wingDevice = std::max(headDevice * 0.55, widthDevice);
        const double wing = wingDevice / scale;
        const double unitX = (end.x() - start.x()) / line.length();
        const double unitY = (end.y() - start.y()) / line.length();
        const double baseX = end.x() - unitX * headDevice / scale;
        const double baseY = end.y() - unitY * headDevice / scale;
        const QPointF first(baseX - unitY * wing, baseY + unitX * wing);
        const QPointF second(baseX + unitY * wing, baseY - unitX * wing);
        if (annotation.arrowStyle == QStringLiteral("filled")) {
            QPolygonF head;
            head << end << first << second;
            painter->setBrush(annotation.color);
            painter->drawPolygon(head);
        }
        painter->drawLine(end, first);
        painter->drawLine(end, second);
    }

    int padding(const Annotation &annotation) const override
    {
        if (annotation.tool == QStringLiteral("mosaic")) {
            // The disc radius in local pixels.  The brush is stamped in device
            // space as clamp(width*scale/2, .., 512) doubled for the strongest
            // setting; scaling back down, that is at most twice width/2 for
            // every output scale, which is what this bounds from.
            const int base = std::clamp(static_cast<int>(annotation.width) / 2, 1, 512);
            return std::clamp(brushRadiusForStrength(annotation.strength, base), 1, 512) + 2;
        }
        if (annotation.tool == QStringLiteral("arrow")) {
            const int head = std::max(6, static_cast<int>(annotation.width) * 4) *
                static_cast<int>(annotation.size);
            return head + static_cast<int>(annotation.width) + 4;
        }
        if (annotation.tool == QStringLiteral("wave")) {
            // The wave's crests reach `amplitude` off the line its two points
            // describe -- the box `annotationLogicalBounds` reports -- so the
            // room has to cover that plus the pen's own half width and a pixel
            // for the antialiased edge.  The controller sizes the region a drag
            // invalidates from this, so a crest left outside it would stay on
            // screen after the wave moved.
            const int amplitude = std::max(static_cast<int>(annotation.width) * 2, 4);
            return amplitude + static_cast<int>(annotation.width) / 2 + 4;
        }
        return AnnotationRaster::padding(annotation);
    }
};

// One text label, rasterized with the preview font so the bitmap matches the
// final render.
class TextRaster final : public AnnotationRaster {
protected:
    QRectF bounds(const Annotation &annotation, const OutputSession &output,
                  const QSize &size) const override
    {
        LogicalRect rect;
        if (!annotationLogicalBounds(annotation, &rect)) {
            return QRectF();
        }
        return localRect(output, rect, size);
    }

    QByteArray signature(const Annotation &annotation, const OutputSession &output,
                         const QSize &size) const override
    {
        QByteArray data;
        QDataStream stream(&data, QIODevice::WriteOnly);
        writeContext(stream, output, size);
        // The text's pixels depend only on its text, font, size and colour: a
        // move shifts where the cached bitmap is blitted, not what it holds.
        stream << annotation.text << annotation.font << annotation.textPixels
               << static_cast<quint32>(annotation.color.rgba());
        return data;
    }

    void draw(QPainter *painter, const Annotation &annotation, const OutputSession &output,
              const QSize &size) const override
    {
        LogicalRect rect;
        if (!annotationLogicalBounds(annotation, &rect)) {
            return;
        }
        painter->setFont(annotationFont(annotation));
        painter->setPen(annotation.color);
        // Top-left anchored inside the measured bounds so the preview matches
        // the Rust glyph origin and the re-edit hit test.
        painter->drawText(localRect(output, rect, size), Qt::AlignLeft | Qt::AlignTop,
                          annotation.text);
    }

    int padding(const Annotation &) const override { return 2; }
};

// A numbered badge, rasterized like a label: its pixels depend on the count,
// the style, the colour and the width it is sized from -- not on where it sits,
// so dragging one reuses the cached bitmap.  It pads by the ordinary stroke
// reach, which is what covers the ring style's line.
class NumberRaster final : public AnnotationRaster {
protected:
    QRectF bounds(const Annotation &annotation, const OutputSession &output,
                  const QSize &size) const override
    {
        LogicalRect rect;
        if (!annotationLogicalBounds(annotation, &rect)) {
            return QRectF();
        }
        return localRect(output, rect, size);
    }

    QByteArray signature(const Annotation &annotation, const OutputSession &output,
                         const QSize &size) const override
    {
        QByteArray data;
        QDataStream stream(&data, QIODevice::WriteOnly);
        writeContext(stream, output, size);
        stream << annotation.number << static_cast<int>(annotation.numberStyle)
               << static_cast<quint32>(annotation.width)
               << static_cast<quint32>(annotation.color.rgba());
        return data;
    }

    void draw(QPainter *painter, const Annotation &annotation, const OutputSession &output,
              const QSize &size) const override
    {
        LogicalRect rect;
        if (!annotationLogicalBounds(annotation, &rect)) {
            return;
        }
        // The box *is* the badge's own square, so it is drawn into directly
        // rather than centred on a point again.
        paintNumberBadge(*painter, localRect(output, rect, size),
                         QString::number(annotation.number), annotation.numberStyle,
                         annotation.color, static_cast<int>(annotation.width));
    }
};

// A pasted image, drawn at the rect it was placed at.
class ImageRaster final : public AnnotationRaster {
protected:
    QRectF bounds(const Annotation &annotation, const OutputSession &output,
                  const QSize &size) const override
    {
        if (annotation.pixels.isNull()) {
            return QRectF();
        }
        return localRect(output, annotation.rect, size);
    }

    QByteArray signature(const Annotation &annotation, const OutputSession &output,
                         const QSize &size) const override
    {
        QByteArray data;
        QDataStream stream(&data, QIODevice::WriteOnly);
        writeContext(stream, output, size);
        stream << annotation.pixels.cacheKey() << static_cast<qint64>(annotation.rect.x)
               << static_cast<qint64>(annotation.rect.y)
               << static_cast<quint64>(annotation.rect.width)
               << static_cast<quint64>(annotation.rect.height);
        return data;
    }

    void draw(QPainter *painter, const Annotation &annotation, const OutputSession &output,
              const QSize &size) const override
    {
        if (annotation.pixels.isNull()) {
            return;
        }
        painter->setRenderHint(QPainter::SmoothPixmapTransform, true);
        painter->drawImage(localRect(output, annotation.rect, size), annotation.pixels);
    }

    int padding(const Annotation &) const override { return 2; }
};

std::shared_ptr<AnnotationRaster> makeAnnotationRaster(const Annotation &annotation)
{
    switch (annotation.kind) {
    case Annotation::Kind::Shape:
        return std::make_shared<ShapeRaster>();
    case Annotation::Kind::Text:
        // A numbered badge is a text annotation by wire, but not by paint: its
        // bitmap is drawn from the badge painter, and it caches on its own.
        if (isNumberAnnotation(annotation)) {
            return std::make_shared<NumberRaster>();
        }
        return std::make_shared<TextRaster>();
    case Annotation::Kind::Image:
        return std::make_shared<ImageRaster>();
    case Annotation::Kind::Stroke:
        break;
    }
    return std::make_shared<StrokeRaster>();
}

int annotationReach(const Annotation &annotation)
{
    const std::shared_ptr<AnnotationRaster> raster =
        annotation.raster != nullptr ? annotation.raster : makeAnnotationRaster(annotation);
    return raster->reach(annotation);
}

int Annotation::rasterRebuilds() const
{
    return raster != nullptr ? raster->rebuilds() : -1;
}

qreal Annotation::rasterDeviceRatio() const
{
    return raster != nullptr ? raster->builtAtRatio() : 0.0;
}

// Draws the region editor's base layer -- the frozen session image plus the
// translucent veil over it -- from a pre-composed device-pixel image.  Composing
// the two together once turns a repaint's two full-frame passes into a single
// 1:1 blit; the in-selection copy of the image is still drawn by the caller.
//
// The composite is only used when the painter maps the overlay onto its device
// with a plain integer scale and no offset, which is what makes the cached blit
// land pixel for pixel: the target rects become exact integer device rects, so
// drawImage samples the composite one pixel to one device pixel.  Any other
// transform (a fractionally scaled or offset painter) returns false so the
// caller draws the two operations directly, keeping the output identical.
bool drawCachedBaseLayer(QPainter *painter, const OutputSession &output, const QSize &size,
                         const QRectF &imageRect, QImage *cache, QByteArray *cacheKey)
{
    const QTransform transform = painter->combinedTransform();
    if (transform.type() > QTransform::TxScale || transform.dx() != 0.0 ||
        transform.dy() != 0.0 || transform.m11() != transform.m22()) {
        return false;
    }
    const qreal ratio = transform.m11();
    if (ratio < 1.0 || ratio != std::floor(ratio)) {
        return false;
    }
    QByteArray key;
    {
        QDataStream stream(&key, QIODevice::WriteOnly);
        stream << output.id << output.image.cacheKey() << output.scale
               << output.geometry.x << output.geometry.y << output.geometry.width
               << output.geometry.height << output.surface.x << output.surface.y
               << output.surface.width << output.surface.height << size.width() << size.height()
               << static_cast<double>(ratio);
    }
    if (cache->isNull() || key != *cacheKey) {
        QImage composite((QSizeF(size) * ratio).toSize(), QImage::Format_ARGB32_Premultiplied);
        composite.setDevicePixelRatio(ratio);
        composite.fill(Qt::transparent);
        QPainter builder(&composite);
        // The composite's painter carries the same device-pixel-ratio transform
        // as the caller's, so the two operations below land on the very pixels
        // the caller would have drawn them to.
        builder.setRenderHint(QPainter::SmoothPixmapTransform, false);
        builder.drawImage(imageRect, output.image);
        builder.fillRect(QRectF(QPointF(0, 0), QSizeF(size)), QColor(0, 0, 0, 80));
        *cache = std::move(composite);
        *cacheKey = key;
    }
    painter->drawImage(QPointF(0, 0), *cache);
    return true;
}

void OverlayController::paint(CaptureOverlay *overlay, QPainter *painter)
{
    const OutputSession &output = overlay->output();
    const QRectF target(0, 0, overlay->width(), overlay->height());
    // The editor shows the session bounds (the image) at the selection, which
    // the pin editor lets the user drag around; region capture pins the
    // selection onto the frozen output, so the two coincide there.
    const LogicalRect imageArea =
        pinEdit_ && selection_.has_value() ? *selection_ : output.geometry;
    const QRectF imageRect = localRect(output, imageArea, overlay->size());
    painter->save();
    painter->setRenderHint(QPainter::SmoothPixmapTransform, false);
    // Picking shows the desktop live and never paints the session's frame:
    // that would freeze the very screen the user is choosing from.  Everything
    // but the hovered window is veiled so the pick stands out.  The hovered
    // window's outline and label pill are drawn below; the click ends the
    // session, and the frame it is captured into comes from Rust after that.
    const bool livePick = pickMode_;
    // In pin-edit mode the pinned window itself shows the image: the editor
    // only draws the marks on top, so there is exactly one copy on screen.
    if (livePick) {
        QPainterPath veil;
        veil.addRect(target);
        if (selection_.has_value()) {
            LogicalRect visible;
            if (intersection(*selection_, output.geometry, &visible)) {
                QPainterPath hole;
                hole.addRect(localRect(output, visible, overlay->size()));
                veil = veil.subtracted(hole);
            }
        }
        painter->fillPath(veil, QColor(0, 0, 0, 80));
    } else if (!pinEdit_) {
        // The frozen image and the veil over it do not change between repaints
        // (only the selection does), so they are composed once and blitted;
        // drawing them directly walks the whole frame twice per repaint.  The
        // helper refuses any painter whose transform it cannot reproduce
        // exactly, in which case the two operations are drawn as before.
        if (!drawCachedBaseLayer(painter, output, overlay->size(), imageRect, &baseComposite_,
                                 &baseCompositeKey_)) {
            painter->drawImage(imageRect, output.image);
            // The dim-out only makes sense around a selectable region.
            painter->fillRect(target, QColor(0, 0, 0, 80));
        }
        if (selection_.has_value()) {
            LogicalRect visible;
            if (intersection(*selection_, output.geometry, &visible)) {
                painter->drawImage(localRect(output, visible, overlay->size()), output.image,
                                   sourceRect(output, visible));
            }
        }
    }

    // The recognized characters of the selection, drawn where they were: above
    // the dim veil and below the marks.  It is drawn on every frame and never
    // composited into `baseComposite_`, whose key does not include the
    // selection -- a layer baked into that cache would freeze the highlight the
    // moment the range moved.
    if (textMode_ && textLayer_.has_value() && textLayer_->count() > 0) {
        const int count = textLayer_->count();
        const int low = textAnchor_ >= 0 ? std::min(textAnchor_, textFocus_) : -1;
        const int high = textAnchor_ >= 0 ? std::max(textAnchor_, textFocus_) : -1;
        // The fills first, so the outlines below stay visible on top of them.
        // Adjacent unit boxes tile exactly, so the fills join into one bar the
        // way a text selection does.
        if (low >= 0) {
            painter->setPen(Qt::NoPen);
            painter->setBrush(kTextSelectionFill);
            for (int index = low; index <= high; ++index) {
                painter->drawRect(localRect(output, textLayer_->unit(index).rect, overlay->size()));
            }
        }
        // One outline per line, around the union of its units: the user can see
        // what was recognized without a grid of touching boxes.
        painter->setPen(QPen(kTextOutline, 1.0));
        painter->setBrush(Qt::NoBrush);
        int lineStart = 0;
        while (lineStart < count) {
            const int line = textLayer_->unit(lineStart).line;
            int lineEnd = lineStart;
            while (lineEnd + 1 < count && textLayer_->unit(lineEnd + 1).line == line) {
                ++lineEnd;
            }
            bool haveBounds = false;
            QRectF lineBounds;
            for (int index = lineStart; index <= lineEnd; ++index) {
                const QRectF box =
                    localRect(output, textLayer_->unit(index).rect, overlay->size());
                lineBounds = haveBounds ? lineBounds.united(box) : box;
                haveBounds = true;
            }
            if (haveBounds) {
                painter->drawRect(lineBounds);
            }
            lineStart = lineEnd + 1;
        }
    }

    painter->setRenderHint(QPainter::Antialiasing, true);
    const QRectF outputBounds = target;
    painter->setClipRect(outputBounds);
    // Annotations are clipped to the image: dragging the pin around must not
    // leave marks floating on the transparent canvas, and the renderer only
    // ever composites them onto the image.
    if (pinEdit_) {
        painter->setClipRect(imageRect, Qt::IntersectClip);
    }
    painter->setBrush(Qt::NoBrush);
    auto drawAnnotation = [this, &output, overlay, painter](const Annotation &annotation) {
        const double scale = output.scale > 0 ? static_cast<double>(output.scale) : 1.0;
        const QPen annotationPen = penForAnnotation(annotation);
        if (annotation.kind == Annotation::Kind::Image) {
            if (annotation.pixels.isNull()) {
                return;
            }
            const QRectF target = localRect(output, annotation.rect, overlay->size());
            painter->setRenderHint(QPainter::SmoothPixmapTransform, true);
            painter->drawImage(target, annotation.pixels);
            return;
        }
        if (annotation.kind == Annotation::Kind::Shape) {
            if (annotation.tool == QStringLiteral("mosaic")) {
                drawMosaicAnnotation(painter, output, annotation.rect, annotation.mask,
                                     annotation.strength, overlay->size());
                return;
            }
            painter->setPen(annotationPen);
            painter->setBrush(Qt::NoBrush);
            const QRectF rect = localRect(output, annotation.rect, overlay->size());
            if (annotation.tool == QStringLiteral("ellipse")) {
                painter->drawEllipse(rect);
            } else if (annotation.dash != QStringLiteral("solid")) {
                // Match the final renderer's band-centerline dash walk.
                painter->drawPolyline(insetRectPolygon(rect, annotation.width));
            } else {
                painter->drawRect(rect);
            }
            return;
        }
        if (annotation.kind == Annotation::Kind::Text) {
            LogicalRect bounds;
            if (!annotationBounds(annotation, &bounds)) {
                return;
            }
            if (isNumberAnnotation(annotation)) {
                // The badge's box, drawn as the badge: same painter as the
                // cached raster and the bitmap, so the preview cannot drift from
                // the mark the user ends up with.
                paintNumberBadge(*painter, localRect(output, bounds, overlay->size()),
                                 QString::number(annotation.number), annotation.numberStyle,
                                 annotation.color, static_cast<int>(annotation.width));
                return;
            }
            QFont font = annotationFont(annotation);
            painter->setFont(font);
            painter->setPen(annotation.color);
            // Top-left anchored inside the measured bounds so the preview
            // matches the Rust glyph origin and the re-edit hit test.
            painter->drawText(localRect(output, bounds, overlay->size()),
                              Qt::AlignLeft | Qt::AlignTop, annotation.text);
            return;
        }
        if (annotation.points.isEmpty()) {
            return;
        }
        if (annotation.tool == QStringLiteral("mosaic")) {
            // Freehand mosaic brush: smear discs along the path.
            drawMosaicBrush(painter, output, annotation.points, annotation.width,
                            annotation.strength, overlay->size());
            return;
        }
        if (annotation.tool == QStringLiteral("bezier")) {
            // Drawn through the very helper the committed mark's rasterizer
            // uses, so letting go changes nothing on screen.
            QVector<QPointF> at;
            at.reserve(annotation.points.size());
            for (const Point &point : annotation.points) {
                at.append(localPoint(output, point, overlay->size()));
            }
            paintBezierInk(painter, at, annotation.closed, annotation.color,
                           static_cast<int>(annotation.width));
            return;
        }
        QPolygonF polygon;
        QPen pen = annotationPen;
        if (annotation.tool == QStringLiteral("wave") && annotation.points.size() >= 2) {
            // A wave is the sine sample of the segment between its two points,
            // not the segment itself: sample it here exactly as the committed
            // mark's rasterizer does -- solid, and from the same two points --
            // so letting go changes nothing on screen.
            const QVector<QPointF> wave = wavePolyline(
                localPoint(output, annotation.points.constFirst(), overlay->size()),
                localPoint(output, annotation.points.constLast(), overlay->size()),
                static_cast<int>(annotation.width), scale);
            polygon = QPolygonF(wave.begin(), wave.end());
            pen = wavePen(annotation);
        } else {
            for (const Point &point : annotation.points) {
                polygon.push_back(localPoint(output, point, overlay->size()));
            }
        }
        painter->setPen(pen);
        painter->drawPolyline(polygon);
        if (annotation.tool == QStringLiteral("arrow") && polygon.size() >= 2) {
            // The head is always solid and scales with the annotation's size.
            painter->setPen(QPen(annotation.color, static_cast<double>(annotation.width),
                                 Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
            const QPointF end = polygon.constLast();
            const QPointF start = polygon.at(polygon.size() - 2);
            const QLineF line(start, end);
            if (line.length() > 1.0) {
                const double widthDevice = static_cast<double>(annotation.width) * scale;
                const double headDevice = std::min(std::max(6.0, widthDevice * 4.0) *
                                                       static_cast<double>(annotation.size),
                                                   line.length() * scale);
                const double wingDevice = std::max(headDevice * 0.55, widthDevice);
                const double wing = wingDevice / scale;
                const double unitX = (end.x() - start.x()) / line.length();
                const double unitY = (end.y() - start.y()) / line.length();
                const double baseX = end.x() - unitX * headDevice / scale;
                const double baseY = end.y() - unitY * headDevice / scale;
                const QPointF first(baseX - unitY * wing, baseY + unitX * wing);
                const QPointF second(baseX + unitY * wing, baseY - unitX * wing);
                if (annotation.arrowStyle == QStringLiteral("filled")) {
                    QPolygonF head;
                    head << end << first << second;
                    painter->setBrush(annotation.color);
                    painter->drawPolygon(head);
                }
                painter->drawLine(end, first);
                painter->drawLine(end, second);
            }
        }
    };

    // Committed marks are drawn from their cached raster: each one redraws
    // itself only when something it draws changed, so a repaint (a selection
    // drag, a pointer move) blits the untouched ones instead of recomputing
    // them -- the mosaic in particular, which averages the source image.
    for (const Annotation &annotation : annotations_) {
        if (annotation.raster == nullptr) {
            annotation.raster = makeAnnotationRaster(annotation);
        }
        annotation.raster->paint(painter, annotation, output, overlay->size(),
                                 overlay->outputIndex());
    }
    if (gesture_->type == Gesture::Type::Bezier && gesture_->points.size() >= 2) {
        // The pen path so far, plus the rubber band from its last anchor to the
        // pointer.  The band is a straight segment: the curve the next segment
        // would take is not known until its anchor is placed, and a band drawn
        // through the last anchor's own handle would loop back on the anchor
        // while that handle is being dragged.
        drawAnnotation(previewAnnotation());
        const Point &anchor = gesture_->points.at(gesture_->points.size() - 2);
        const QPointF from = localPoint(output, anchor, overlay->size());
        const QPointF to = localPoint(output, gesture_->current, overlay->size());
        if (QLineF(from, to).length() > 0.0) {
            painter->setPen(QPen(currentColor_, static_cast<double>(currentWidth_), Qt::SolidLine,
                                 Qt::RoundCap, Qt::RoundJoin));
            painter->setBrush(Qt::NoBrush);
            painter->drawLine(from, to);
        }
    }
    if (gesture_->type == Gesture::Type::Drawing && !gesture_->points.isEmpty()) {
        // Rectangle, ellipse, the area mosaic and the arrow all depend on two
        // points, so drawing them straight is already cheap.  The freehand pen
        // and the mosaic brush grow a point per move and build up through the
        // incremental raster instead; a translucent pen would double-blend
        // where consecutive round caps overlap, so it keeps the straight draw.
        // The same predicate decides how much of the surface a move invalidates,
        // so both read it from one place.
        if (drawsGrowingStroke()) {
            paintLiveStroke(painter, output, overlay->size(), overlay->outputIndex());
        } else {
            Annotation preview;
            preview.tool = toolName(tool_);
            preview.color = currentColor_;
            preview.width = currentWidth_;
            preview.dash = currentDash_;
            preview.size = arrowSize_;
            preview.arrowStyle = currentArrowStyle_;
            preview.mask = mosaicShape_;
            preview.strength = mosaicStrength_;
            if (tool_ == Tool::Rectangle || tool_ == Tool::Ellipse ||
                (tool_ == Tool::Mosaic && mosaicShape_ != QStringLiteral("brush"))) {
                preview.kind = Annotation::Kind::Shape;
                preview.rect = selectionBetween(gesture_->points.constFirst(),
                                                gesture_->points.constLast());
                if (tool_ == Tool::Mosaic) {
                    preview.tool = QStringLiteral("mosaic");
                }
            } else {
                preview.kind = Annotation::Kind::Stroke;
                preview.points = gesture_->points;
                if (tool_ == Tool::Mosaic) {
                    preview.tool = QStringLiteral("mosaic");
                }
            }
            // Shapes, the arrow and a translucent pen draw straight: the first
            // three are two-point previews, and the last would double-blend
            // where consecutive round caps overlap.
            drawAnnotation(preview);
        }
    }

    // Highlight the selected annotation with handles while the Select tool is
    // manipulating it.
    if (tool_ == Tool::Select && selectedAnnotation_ >= 0 &&
        selectedAnnotation_ < annotations_.size() &&
        (gesture_->type == Gesture::Type::None ||
         gesture_->type == Gesture::Type::MovingAnnotation ||
         gesture_->type == Gesture::Type::ResizingAnnotation)) {
        const Annotation &annotation = annotations_.at(selectedAnnotation_);
        LogicalRect bounds;
        if (annotationBounds(annotation, &bounds)) {
            const QRectF local = localRect(output, bounds, overlay->size());
            painter->setPen(QPen(Qt::white, 1.0, Qt::DashLine));
            painter->setBrush(Qt::NoBrush);
            painter->drawRect(local);
            if (annotation.kind != Annotation::Kind::Text) {
                painter->setBrush(Qt::white);
                painter->setPen(QPen(Qt::black, 1.0));
                const QPointF midX((local.left() + local.right()) / 2.0, 0);
                const QPointF midY(0, (local.top() + local.bottom()) / 2.0);
                const QPointF corners[] = {
                    local.topLeft(), {midX.x(), local.top()}, local.topRight(),
                    {local.right(), midY.y()}, local.bottomRight(),
                    {midX.x(), local.bottom()}, local.bottomLeft(),
                    {local.left(), midY.y()},
                };
                for (const QPointF &corner : corners) {
                    painter->drawRect(QRectF(corner.x() - 4, corner.y() - 4, 8, 8));
                }
            }
        }
    }

    if (selection_.has_value() && !pinEdit_) {
        // Pin editing selects the whole image by construction: drawing the
        // selection rect and its handles would ring the pin with chrome the
        // user cannot act on.
        LogicalRect visible;
        if (intersection(*selection_, output.geometry, &visible)) {
            painter->setPen(QPen(Qt::white, 2.0, Qt::SolidLine));
            painter->setBrush(Qt::NoBrush);
            painter->drawRect(localRect(output, visible, overlay->size()));
            if (editing_) {
                painter->setBrush(Qt::white);
                painter->setPen(QPen(Qt::black, 1.0));
                const LogicalRect &selection = *selection_;
                const Point handles[] = {
                    {selection.x, selection.y},
                    {static_cast<std::int32_t>((static_cast<std::int64_t>(selection.x) + selection.right() - 1) / 2), selection.y},
                    {static_cast<std::int32_t>(selection.right() - 1), selection.y},
                    {static_cast<std::int32_t>(selection.right() - 1), static_cast<std::int32_t>((static_cast<std::int64_t>(selection.y) + selection.bottom() - 1) / 2)},
                    {static_cast<std::int32_t>(selection.right() - 1), static_cast<std::int32_t>(selection.bottom() - 1)},
                    {static_cast<std::int32_t>((static_cast<std::int64_t>(selection.x) + selection.right() - 1) / 2), static_cast<std::int32_t>(selection.bottom() - 1)},
                    {selection.x, static_cast<std::int32_t>(selection.bottom() - 1)},
                    {selection.x, static_cast<std::int32_t>((static_cast<std::int64_t>(selection.y) + selection.bottom() - 1) / 2)},
                };
                for (const Point &point : handles) {
                    if (point.x >= output.geometry.x && point.x < output.geometry.right() &&
                        point.y >= output.geometry.y && point.y < output.geometry.bottom()) {
                        const QPointF localPointValue = localPoint(output, point, overlay->size());
                        painter->drawRect(QRectF(localPointValue.x() - 4, localPointValue.y() - 4, 8, 8));
                    }
                }
            }
        }
    }

    // Window picking previews a whole window before it is committed, so its
    // pill names the window instead of just measuring it.
    const bool pickPreview = pickMode_ && !editing_ && selection_.has_value();
    if (selection_.has_value() && !pinEdit_ &&
        (pickPreview || gesture_->type == Gesture::Type::Selecting || editing_)) {
        const QString text = pickPreview
            ? candidatePillText()
            : QStringLiteral("%1 × %2").arg(selection_->width).arg(selection_->height);
        drawInfoPill(painter, localPoint(output, Point{selection_->x, selection_->y},
                                         overlay->size()),
                     text, target);
    }

    const bool loupeActive = gesture_->type == Gesture::Type::Selecting ||
        gesture_->type == Gesture::Type::Moving || gesture_->type == Gesture::Type::Resizing ||
        gesture_->type == Gesture::Type::MovingAnnotation ||
        gesture_->type == Gesture::Type::ResizingAnnotation;
    if (loupeActive && pointerOutput_ == overlay->outputIndex()) {
        drawLoupe(overlay, painter);
    }
    painter->restore();
}

void OverlayController::paintLiveStroke(QPainter *painter, const OutputSession &output,
                                        const QSize &size, int outputIndex)
{
    if (outputIndex != gesture_->liveOutput || gesture_->points.isEmpty()) {
        return;
    }
    const bool brush = tool_ == Tool::Mosaic;
    const int widthLogical = std::max(1, static_cast<int>(currentWidth_));
    const int scale = static_cast<int>(output.scale > 0 ? output.scale : 1);
    const double deviceRadius = brush
        ? std::clamp(brushRadiusForStrength(
                         mosaicStrength_,
                         std::clamp(static_cast<int>(widthLogical * scale / 2), 1, 512)),
                     1, 512)
        : 0.0;
    const double padding = liveStrokeMargin(brush, widthLogical, scale, mosaicStrength_);
    const double step = std::max(1.0, deviceRadius / 2.0);
    // The preview raster holds device pixels for the same reason the committed
    // rasters do; see `AnnotationRaster::deviceRatio`.
    const qreal ratio = AnnotationRaster::deviceRatio(painter);

    // A change of style, output or surface size invalidates the whole raster;
    // the growing point list deliberately does not, which is the point.
    QByteArray key;
    {
        QDataStream stream(&key, QIODevice::WriteOnly);
        const LogicalRect &surface = surfaceOf(output);
        stream << size.width() << size.height() << output.id << output.scale << surface.x
               << surface.y << surface.width << surface.height << toolName(tool_)
               << static_cast<quint32>(currentColor_.rgba()) << currentWidth_ << currentDash_
               << mosaicStrength_ << mosaicShape_ << ratio;
    }
    if (key != gesture_->liveKey) {
        gesture_->liveKey = key;
        gesture_->liveRaster = QImage();
        gesture_->liveOrigin = QPoint();
        gesture_->liveBaked = 1;
        gesture_->liveLength = 0.0;
    }

    const int count = gesture_->points.size();
    // The local-space room the segments added since the last paint need.
    QRectF fresh;
    for (int i = gesture_->liveBaked; i < count; ++i) {
        const QPointF a = localPoint(output, gesture_->points.at(i - 1), size);
        const QPointF b = localPoint(output, gesture_->points.at(i), size);
        const QRectF segment = QRectF(a, b).normalized();
        fresh = fresh.isNull() ? segment : fresh.united(segment);
    }
    const QRect needed = fresh.isNull()
        ? QRect()
        : fresh.adjusted(-padding, -padding, padding, padding)
              .toAlignedRect()
              .intersected(QRect(QPoint(0, 0), size));

    if (needed.isEmpty()) {
        // Nothing new is visible on this overlay, but the path length still has
        // to advance so a later visible segment's dash phase lines up.
        for (int i = gesture_->liveBaked; i < count; ++i) {
            const QPointF a = localPoint(output, gesture_->points.at(i - 1), size);
            const QPointF b = localPoint(output, gesture_->points.at(i), size);
            gesture_->liveLength += std::hypot(b.x() - a.x(), b.y() - a.y());
        }
        liveStrokeBakes_ += std::max(0, count - gesture_->liveBaked);
        gesture_->liveBaked = count;
        if (!gesture_->liveRaster.isNull()) {
            painter->drawImage(gesture_->liveOrigin, gesture_->liveRaster);
        }
        return;
    }

    // Grow the raster only when the stroke reaches past it: the old pixels are
    // copied into the larger image at their original offset.  The image holds
    // device pixels, so the logical rect it covers is its size over the ratio.
    const QRect current(gesture_->liveOrigin,
                        (QSizeF(gesture_->liveRaster.size()) / ratio).toSize());
    if (gesture_->liveRaster.isNull()) {
        gesture_->liveRaster = QImage((QSizeF(needed.size()) * ratio).toSize(),
                                      QImage::Format_ARGB32_Premultiplied);
        gesture_->liveRaster.setDevicePixelRatio(ratio);
        gesture_->liveRaster.fill(Qt::transparent);
        gesture_->liveOrigin = needed.topLeft();
    } else if (!current.contains(needed)) {
        const QRect grown = current.united(needed);
        QImage resized((QSizeF(grown.size()) * ratio).toSize(),
                       QImage::Format_ARGB32_Premultiplied);
        resized.setDevicePixelRatio(ratio);
        resized.fill(Qt::transparent);
        {
            QPainter copy(&resized);
            copy.scale(ratio, ratio);
            copy.drawImage(current.topLeft() - grown.topLeft(), gesture_->liveRaster);
        }
        gesture_->liveRaster = resized;
        gesture_->liveOrigin = grown.topLeft();
    }

    {
        QPainter raster(&gesture_->liveRaster);
        raster.setRenderHint(QPainter::Antialiasing, true);
        raster.scale(ratio, ratio);
        raster.translate(-gesture_->liveOrigin);
        if (brush) {
            for (int i = gesture_->liveBaked; i < count; ++i) {
                stampMosaicSegment(&raster, output, gesture_->points.at(i - 1),
                                   gesture_->points.at(i), i == 1, static_cast<int>(deviceRadius),
                                   static_cast<double>(scale), step, size);
            }
        } else {
            Annotation style;
            style.kind = Annotation::Kind::Stroke;
            style.tool = toolName(tool_);
            style.color = currentColor_;
            style.width = currentWidth_;
            style.dash = currentDash_;
            const QPen pen = penForAnnotation(style);
            const bool dashed = currentDash_ != QStringLiteral("solid");
            for (int i = gesture_->liveBaked; i < count; ++i) {
                const QPointF a = localPoint(output, gesture_->points.at(i - 1), size);
                const QPointF b = localPoint(output, gesture_->points.at(i), size);
                QPen segmentPen = pen;
                if (dashed) {
                    // Continue the dash pattern where the previous segment left
                    // it (`dashOffset` is measured in pen widths).  A solid pen
                    // must not be touched: setDashOffset would turn it into a
                    // custom-dash pen with no pattern, which draws nothing.
                    segmentPen.setDashOffset(gesture_->liveLength / widthLogical);
                }
                raster.setPen(segmentPen);
                raster.drawLine(a, b);
                gesture_->liveLength += std::hypot(b.x() - a.x(), b.y() - a.y());
            }
        }
    }
    liveStrokeBakes_ += std::max(0, count - gesture_->liveBaked);
    gesture_->liveBaked = count;
    painter->drawImage(gesture_->liveOrigin, gesture_->liveRaster);
}

void OverlayController::drawLoupe(CaptureOverlay *overlay, QPainter *painter)
{
    const OutputSession &output = overlay->output();
    const QPointF local = localPoint(output, pointer_, overlay->size());
    const std::uint32_t scale = output.scale > 0 ? output.scale : 1;
    const int sourceWidth = static_cast<int>(output.image.width());
    const int sourceHeight = static_cast<int>(output.image.height());
    if (sourceWidth <= 0 || sourceHeight <= 0) {
        return;
    }
    const int centerX = std::clamp(
        static_cast<int>(std::floor((pointer_.x - output.geometry.x) * static_cast<double>(scale))),
        0, sourceWidth - 1);
    const int centerY = std::clamp(
        static_cast<int>(std::floor((pointer_.y - output.geometry.y) * static_cast<double>(scale))),
        0, sourceHeight - 1);
    const int sampleLeft = std::clamp(centerX - kLoupeRadius, 0, sourceWidth - 1);
    const int sampleTop = std::clamp(centerY - kLoupeRadius, 0, sourceHeight - 1);
    const int sampleWidth = std::min(2 * kLoupeRadius + 1, sourceWidth - sampleLeft);
    const int sampleHeight = std::min(2 * kLoupeRadius + 1, sourceHeight - sampleTop);
    const QRect source(sampleLeft, sampleTop, sampleWidth, sampleHeight);
    const qreal radius = kLoupeDiameter / 2.0;

    QPointF center = local + QPointF(radius * 1.1, radius * 1.1);
    if (center.x() + radius > overlay->width() - kLoupeMargin ||
        center.y() + radius > overlay->height() - kLoupeMargin) {
        if (center.x() + radius > overlay->width() - kLoupeMargin) {
            center.setX(local.x() - radius * 1.1);
        }
        if (center.y() + radius > overlay->height() - kLoupeMargin) {
            center.setY(local.y() - radius * 1.1);
        }
    }

    painter->save();
    QPainterPath clipPath;
    clipPath.addEllipse(center, radius, radius);
    painter->setClipPath(clipPath);
    painter->setRenderHint(QPainter::SmoothPixmapTransform, false);
    const QRectF target(center.x() - radius + (sampleLeft - (centerX - kLoupeRadius)) * kLoupeZoom,
                        center.y() - radius + (sampleTop - (centerY - kLoupeRadius)) * kLoupeZoom,
                        sampleWidth * kLoupeZoom, sampleHeight * kLoupeZoom);
    painter->drawImage(target, output.image, source);
    painter->restore();

    painter->save();
    painter->setRenderHint(QPainter::Antialiasing, true);
    painter->setBrush(Qt::NoBrush);
    painter->setPen(QPen(QColor(0, 0, 0, 190), 4.0));
    painter->drawEllipse(center, radius, radius);
    painter->setPen(QPen(Qt::white, 1.5));
    painter->drawEllipse(center, radius, radius);
    painter->setPen(QPen(QColor(255, 255, 255, 200), 1.0));
    painter->drawLine(center - QPointF(radius / 2.5, 0), center + QPointF(radius / 2.5, 0));
    painter->drawLine(center, center - QPointF(0, radius / 2.5));
    painter->drawLine(center, center + QPointF(0, radius / 2.5));
    const QString coordinates = QStringLiteral("%1, %2").arg(centerX).arg(centerY);
    painter->restore();
    drawInfoPill(painter, center + QPointF(0, radius + 2.0), coordinates,
                 QRectF(0, 0, overlay->width(), overlay->height()));
}

CaptureOverlay::CaptureOverlay(int outputIndex, OverlayController *controller, QScreen *screen)
    : QWidget(nullptr)
    , outputIndex_(outputIndex)
    , controller_(controller)
    , screen_(screen)
{
    setAttribute(Qt::WA_NativeWindow);
    setAttribute(Qt::WA_TranslucentBackground);
    setAutoFillBackground(false);
    setWindowFlags(Qt::FramelessWindowHint | Qt::Tool);
    setFocusPolicy(Qt::StrongFocus);
    setMouseTracking(true);
    setCursor(Qt::CrossCursor);
    setAcceptDrops(false);
    const LogicalRect &surface = surfaceOf(output());
    resize(static_cast<int>(surface.width), static_cast<int>(surface.height));
}

CaptureOverlay::~CaptureOverlay() = default;

int CaptureOverlay::outputIndex() const
{
    return outputIndex_;
}

const OutputSession &CaptureOverlay::output() const
{
    return controller_->session().outputs.at(outputIndex_);
}

QPointF CaptureOverlay::localFromGlobal(Point point) const
{
    return localPoint(output(), point, size());
}

bool CaptureOverlay::showLayerSurface()
{
    if (layerWindow_ == nullptr) {
        winId();
        layerWindow_ = windowHandle();
        if (layerWindow_ == nullptr) {
            return false;
        }
        auto *layer = LayerShellQt::Window::get(layerWindow_);
        if (layer == nullptr) {
            return false;
        }
        layer->setLayer(LayerShellQt::Window::LayerOverlay);
        LayerShellQt::Window::Anchors anchors(LayerShellQt::Window::AnchorTop);
        anchors |= LayerShellQt::Window::AnchorBottom;
        anchors |= LayerShellQt::Window::AnchorLeft;
        anchors |= LayerShellQt::Window::AnchorRight;
        layer->setAnchors(anchors);
        layer->setExclusiveZone(-1);
        layer->setKeyboardInteractivity(LayerShellQt::Window::KeyboardInteractivityExclusive);
        layer->setActivateOnShow(true);
        layer->setScope(QStringLiteral("vshot-qt-ui"));
        layer->setDesiredSize(QSize(0, 0));
        layer->setScreen(screen_);
    }
    show();
    raise();
    activateWindow();
    setFocus(Qt::OtherFocusReason);
    return true;
}

bool CaptureOverlay::showLayerSurfaceAt(int globalX, int globalY, int width, int height)
{
    winId();
    layerWindow_ = windowHandle();
    if (layerWindow_ == nullptr) {
        return false;
    }
    auto *layer = LayerShellQt::Window::get(layerWindow_);
    if (layer == nullptr) {
        return false;
    }
    layer->setLayer(LayerShellQt::Window::LayerOverlay);
    // Anchor top-left only and carve the box out with margins, so the surface
    // lands exactly on the given global rect regardless of output origin.
    LayerShellQt::Window::Anchors anchors(LayerShellQt::Window::AnchorTop);
    anchors |= LayerShellQt::Window::AnchorLeft;
    layer->setAnchors(anchors);
    layer->setExclusiveZone(-1);
    layer->setKeyboardInteractivity(LayerShellQt::Window::KeyboardInteractivityExclusive);
    layer->setActivateOnShow(true);
    layer->setScope(QStringLiteral("vshot-pin-edit"));
    layer->setDesiredSize(QSize(width, height));
    layer->setScreen(screen_);
    const QRect output = screen_ != nullptr ? screen_->geometry() : QRect();
    layer->setMargins(QMargins(std::max(0, globalX - output.left()),
                               std::max(0, globalY - output.top()), 0, 0));
    resize(width, height);
    show();
    raise();
    // Exclusive keyboard interactivity only takes effect once the surface is
    // actually activated; without activateWindow() the compositor never routes
    // key events here and Escape/Enter/arrow keys are all silently dead.
    activateWindow();
    setFocus(Qt::OtherFocusReason);
    return true;
}

void CaptureOverlay::paintEvent(QPaintEvent *event)
{
    Q_UNUSED(event);
    QPainter painter(this);
    controller_->paint(this, &painter);
}

void CaptureOverlay::mousePressEvent(QMouseEvent *event)
{
    controller_->press(this, event->position(), event->button(), event->modifiers());
    event->accept();
}

void CaptureOverlay::mouseMoveEvent(QMouseEvent *event)
{
    controller_->move(this, event->position(), event->buttons(), event->modifiers());
    event->accept();
}

void CaptureOverlay::mouseReleaseEvent(QMouseEvent *event)
{
    controller_->release(this, event->position(), event->button(), event->modifiers());
    event->accept();
}

void CaptureOverlay::mouseDoubleClickEvent(QMouseEvent *event)
{
    controller_->doubleClick(this, event->position(), event->button());
    event->accept();
}

void CaptureOverlay::keyPressEvent(QKeyEvent *event)
{
    controller_->key(this, event->key(), event->modifiers());
    event->accept();
}

void CaptureOverlay::closeEvent(QCloseEvent *event)
{
    // The compositor (or a stray close request) must not hang the Rust side:
    // treat an externally closed overlay as a cancelled session.
    controller_->cancel();
    event->accept();
}

void CaptureOverlay::leaveEvent(QEvent *event)
{
    setCursor(Qt::CrossCursor);
    QWidget::leaveEvent(event);
}

} // namespace vshot
