// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#include "annotate_surface.hpp"

#include "i18n.hpp"

#include <LayerShellQt/Window>

#include <QApplication>
#include <QEnterEvent>
#include <QFont>
#include <QFontMetrics>
#include <QFrame>
#include <QHBoxLayout>
#include <QImage>
#include <QKeyEvent>
#include <QLineEdit>
#include <QLineF>
#include <QMouseEvent>
#include <QPainter>
#include <QPainterPath>
#include <QPair>
#include <QScreen>
#include <QSize>
#include <QSizePolicy>
#include <QTransform>
#include <QWindow>

#include <algorithm>
#include <cmath>
#include <functional>
#include <limits>

namespace vshot {

namespace {

// The palette, in logical pixels.  The panel is one horizontal row; the height
// is fixed so the toolbar never fights the canvas for the output's top edge.
constexpr int kToolbarHeight = 40;
constexpr int kToolbarPad = 8;
constexpr int kGripWidth = 16;
constexpr int kButtonHeight = 28;
constexpr int kLabelPadX = 9;
constexpr int kDotBox = 26;
constexpr qreal kPanelRadius = 12.0;
constexpr qreal kButtonRadius = 8.0;
constexpr qreal kSuperellipseExponent = 5.0;

// The label font of the panel's text buttons, at the size the capture
// overlay's QSS used for the same row.
QFont buttonFont()
{
    QFont font = QApplication::font();
    font.setPixelSize(12);
    return font;
}

// The annotation text font.  A text label is sized from the stroke width so the
// one width control covers both: 6x the pen width keeps a label legible beside
// the line it annotates.  Pixel size rather than point size, so a label is the
// same number of pixels on a 2x output as it is on a 1x one.
QFont annotationFont(int width)
{
    QFont font = QApplication::font();
    font.setPixelSize(std::max(1, 6 * width));
    return font;
}

// Squircle (superellipse) outline, corner radius with exponent 5, matching the
// capture overlay's palette.  The two files share no code on purpose, so this
// is a local copy rather than a shared header.
QPainterPath superellipsePath(const QRectF &bounds, qreal radius, qreal exponent)
{
    if (bounds.isEmpty()) {
        return QPainterPath();
    }
    const qreal most = std::min(bounds.width(), bounds.height()) / 2.0;
    radius = std::clamp(radius, 0.0, most);
    QPainterPath path;
    if (radius <= 0.0) {
        path.addRect(bounds);
        return path;
    }
    const int steps = 12;
    bool first = true;
    const auto corner = [&](qreal centerX, qreal centerY, qreal startDegrees, qreal endDegrees) {
        for (int i = 0; i <= steps; ++i) {
            const qreal angle =
                qDegreesToRadians(startDegrees + (endDegrees - startDegrees) * i / steps);
            const qreal cosine = std::cos(angle);
            const qreal sine = std::sin(angle);
            const qreal x = centerX
                + std::copysign(std::pow(std::abs(cosine), 2.0 / exponent), cosine) * radius;
            const qreal y = centerY
                + std::copysign(std::pow(std::abs(sine), 2.0 / exponent), sine) * radius;
            if (first) {
                path.moveTo(x, y);
                first = false;
            } else {
                path.lineTo(x, y);
            }
        }
    };
    corner(bounds.left() + radius, bounds.top() + radius, 180.0, 270.0);
    corner(bounds.right() - radius, bounds.top() + radius, 270.0, 360.0);
    corner(bounds.right() - radius, bounds.bottom() - radius, 0.0, 90.0);
    corner(bounds.left() + radius, bounds.bottom() - radius, 90.0, 180.0);
    path.closeSubpath();
    return path;
}

QRectF normalizedRect(const QPointF &a, const QPointF &b)
{
    return QRectF(QPointF(std::min(a.x(), b.x()), std::min(a.y(), b.y())),
                  QPointF(std::max(a.x(), b.x()), std::max(a.y(), b.y())));
}

double pointSegmentDistance(const QPointF &p, const QPointF &a, const QPointF &b)
{
    const QPointF ab = b - a;
    const double length2 = ab.x() * ab.x() + ab.y() * ab.y();
    if (length2 <= 0.0) {
        return std::hypot(p.x() - a.x(), p.y() - a.y());
    }
    double t = ((p.x() - a.x()) * ab.x() + (p.y() - a.y()) * ab.y()) / length2;
    t = std::clamp(t, 0.0, 1.0);
    const QPointF projection(a.x() + t * ab.x(), a.y() + t * ab.y());
    return std::hypot(p.x() - projection.x(), p.y() - projection.y());
}

// Distance from a point to a rectangle's outline, zero inside it.  A rectangle
// stroke is only its outline: the eraser must not be able to take it by
// brushing the empty middle.
double rectOutlineDistance(const QPointF &p, const QRectF &rect)
{
    if (rect.contains(p)) {
        return 0.0;
    }
    const QPointF topLeft(rect.left(), rect.top());
    const QPointF topRight(rect.right(), rect.top());
    const QPointF bottomRight(rect.right(), rect.bottom());
    const QPointF bottomLeft(rect.left(), rect.bottom());
    return std::min({pointSegmentDistance(p, topLeft, topRight),
                     pointSegmentDistance(p, topRight, bottomRight),
                     pointSegmentDistance(p, bottomRight, bottomLeft),
                     pointSegmentDistance(p, bottomLeft, topLeft)});
}

bool pointInTriangle(const QPointF &p, const QPointF &a, const QPointF &b, const QPointF &c)
{
    const auto side = [](const QPointF &p1, const QPointF &p2, const QPointF &p3) {
        return (p1.x() - p3.x()) * (p2.y() - p3.y()) - (p2.x() - p3.x()) * (p1.y() - p3.y());
    };
    const double d1 = side(p, a, b);
    const double d2 = side(p, b, c);
    const double d3 = side(p, c, a);
    const bool negative = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    const bool positive = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    return !(negative && positive);
}

// The three corners of an arrow's head: tip, then the two wings.  Sized from
// the stroke width exactly as the capture overlay's final renderer does, so
// the preview, the canvas and the eraser all agree on where the ink is.
QVector<QPointF> arrowHeadPoints(const QPointF &tip, const QPointF &tail, double width)
{
    const QLineF line(tail, tip);
    if (line.length() <= 1.0) {
        return {};
    }
    const double head = std::min(std::max(6.0, width * 4.0), line.length());
    const double wing = std::max(head * 0.55, width);
    const double unitX = (tip.x() - tail.x()) / line.length();
    const double unitY = (tip.y() - tail.y()) / line.length();
    const QPointF base(tip.x() - unitX * head, tip.y() - unitY * head);
    return {tip, QPointF(base.x() - unitY * wing, base.y() + unitX * wing),
            QPointF(base.x() + unitY * wing, base.y() - unitX * wing)};
}

// The rect a text stroke's glyphs occupy, top-left anchored at its first point.
QRectF textBounds(const QVector<QPointF> &points, int width, const QString &text)
{
    if (points.isEmpty()) {
        return QRectF();
    }
    const QFontMetricsF metrics(annotationFont(width));
    return QRectF(points.constFirst(),
                  QSizeF(metrics.horizontalAdvance(text), metrics.height()));
}

// The logical rect a stroke can have painted into, already grown by its width
// and, for an arrow, by its head.  Both the repaint region and the eraser's
// reach are asked from here.
QRectF strokeBounds(AnnotateSurface::Tool tool, const QVector<QPointF> &points, int width,
                    const QString &text)
{
    if (tool == AnnotateSurface::Tool::Text) {
        return textBounds(points, width, text);
    }
    if (points.isEmpty()) {
        return QRectF();
    }
    QRectF box(points.constFirst(), QSizeF(0, 0));
    for (qsizetype i = 1; i < points.size(); ++i) {
        box = box.united(normalizedRect(points.at(i - 1), points.at(i)));
    }
    qreal grow = width / 2.0 + 1.0;
    if (tool == AnnotateSurface::Tool::Arrow && points.size() >= 2) {
        const QVector<QPointF> head =
            arrowHeadPoints(points.constLast(), points.at(points.size() - 2), width);
        for (const QPointF &point : head) {
            box = box.united(QRectF(point, QSizeF(0, 0)));
        }
        grow = std::max(grow, static_cast<qreal>(width));
    }
    return box.adjusted(-grow, -grow, grow, grow);
}

// The one place a stroke is turned into ink.  The pen, the rectangle, the arrow
// and the text label are drawn here, whether into the backing image or straight
// onto the surface as a preview.
void paintStrokeInk(QPainter &painter, AnnotateSurface::Tool tool, const QVector<QPointF> &points,
                    const QColor &color, int width, const QString &text)
{
    painter.setPen(QPen(color, width, Qt::SolidLine, Qt::RoundCap, Qt::RoundJoin));
    painter.setBrush(Qt::NoBrush);
    switch (tool) {
    case AnnotateSurface::Tool::Pen:
        if (points.size() >= 2) {
            painter.drawPolyline(points.constData(), points.size());
        } else if (points.size() == 1) {
            // A click that never moved is still a dot of ink.
            painter.drawPoint(points.constFirst());
        }
        break;
    case AnnotateSurface::Tool::Rect:
        if (points.size() >= 2) {
            painter.drawRect(normalizedRect(points.constFirst(), points.constLast()));
        }
        break;
    case AnnotateSurface::Tool::Arrow:
        if (points.size() >= 2) {
            painter.drawPolyline(points.constData(), points.size());
            const QVector<QPointF> head =
                arrowHeadPoints(points.constLast(), points.at(points.size() - 2), width);
            if (head.size() == 3) {
                painter.setPen(Qt::NoPen);
                painter.setBrush(color);
                painter.drawPolygon(head.constData(), 3);
            }
        }
        break;
    case AnnotateSurface::Tool::Text:
        if (!points.isEmpty()) {
            painter.setFont(annotationFont(width));
            painter.setPen(color);
            painter.drawText(textBounds(points, width, text), Qt::AlignLeft | Qt::AlignTop, text);
        }
        break;
    case AnnotateSurface::Tool::Eraser:
        // Not a drawing tool: it removes whole strokes.
        break;
    }
}

QRect grownDirtyRect(const QRectF &box)
{
    if (box.isNull()) {
        return QRect();
    }
    return box.toAlignedRect().adjusted(-2, -2, 2, 2);
}

// One button of the panel.  QSS backgrounds are limited to circular corners and
// the capture overlay's ToolCardFrame paints by event-filtering other widgets,
// so a button that paints its own hover, press and active state is the smaller
// machine here.  Every button is a plain QWidget wired through std::function:
// no signal is emitted from this file.
class ToolbarButton final : public QWidget {
public:
    enum class Kind {
        Label,
        Swatch,
        Width,
        Cross,
    };

    ToolbarButton(QWidget *parent, Kind kind, const QString &label, const QColor &swatch,
                  int diameter)
        : QWidget(parent)
        , kind_(kind)
        , label_(label)
        , swatch_(swatch)
        , diameter_(diameter)
    {
        setAttribute(Qt::WA_TranslucentBackground);
        setCursor(Qt::PointingHandCursor);
        setFocusPolicy(Qt::NoFocus);
    }

    void setOnClick(std::function<void()> onClick) { onClick_ = std::move(onClick); }
    void setActive(bool active)
    {
        if (active_ != active) {
            active_ = active;
            update();
        }
    }
    void setEnabled(bool enabled)
    {
        if (enabled_ != enabled) {
            enabled_ = enabled;
            update();
        }
    }

    QSize sizeHint() const override
    {
        if (kind_ == Kind::Label) {
            const QFontMetrics metrics(buttonFont());
            return QSize(metrics.horizontalAdvance(label_) + 2 * kLabelPadX, kButtonHeight);
        }
        return QSize(kDotBox, kButtonHeight);
    }

protected:
    void enterEvent(QEnterEvent *event) override
    {
        hovered_ = true;
        update();
        QWidget::enterEvent(event);
    }

    void leaveEvent(QEvent *event) override
    {
        hovered_ = false;
        pressed_ = false;
        update();
        QWidget::leaveEvent(event);
    }

    // Every left press is consumed, disabled buttons included: an ignored press
    // would propagate to the panel and then to the surface, and clicking "Undo"
    // with nothing to undo would start drawing a stroke instead.
    void mousePressEvent(QMouseEvent *event) override
    {
        if (event->button() != Qt::LeftButton) {
            QWidget::mousePressEvent(event);
            return;
        }
        if (enabled_) {
            pressed_ = true;
        }
        update();
        event->accept();
    }

    void mouseReleaseEvent(QMouseEvent *event) override
    {
        if (event->button() != Qt::LeftButton) {
            QWidget::mouseReleaseEvent(event);
            return;
        }
        const bool fire = pressed_ && enabled_ && rect().contains(event->position().toPoint());
        pressed_ = false;
        update();
        if (fire && onClick_) {
            onClick_();
        }
        event->accept();
    }

    void paintEvent(QPaintEvent *event) override
    {
        Q_UNUSED(event);
        QPainter painter(this);
        painter.setRenderHint(QPainter::Antialiasing, true);
        const QRectF box = QRectF(rect()).adjusted(1.0, 2.0, -1.0, -2.0);
        const bool contentActive = active_ && (kind_ == Kind::Label || kind_ == Kind::Cross);
        if (contentActive) {
            painter.setPen(Qt::NoPen);
            painter.setBrush(QColor(221, 225, 255));
            painter.drawPath(superellipsePath(box, kButtonRadius, kSuperellipseExponent));
        } else if (enabled_ && (hovered_ || pressed_)) {
            painter.setPen(Qt::NoPen);
            painter.setBrush(pressed_ ? QColor(53, 60, 70) : QColor(44, 50, 59));
            painter.drawPath(superellipsePath(box, kButtonRadius, kSuperellipseExponent));
        }

        const QColor content = !enabled_ ? QColor(111, 118, 128)
                                         : (contentActive ? QColor(0, 20, 92)
                                                          : QColor(230, 225, 229));
        const QPointF center = QRectF(rect()).center();
        switch (kind_) {
        case Kind::Label:
            painter.setFont(buttonFont());
            painter.setPen(content);
            painter.drawText(rect(), Qt::AlignCenter, label_);
            break;
        case Kind::Swatch: {
            const qreal radius = 9.0;
            if (active_) {
                // A ring rather than a light fill: the fill would hide the very
                // colour the button exists to show.
                painter.setPen(QPen(QColor(221, 225, 255), 2.0));
                painter.setBrush(Qt::NoBrush);
                painter.drawEllipse(center, radius + 3.0, radius + 3.0);
            }
            painter.setPen(QPen(QColor(255, 255, 255, 60), 1.0));
            painter.setBrush(enabled_ ? swatch_ : swatch_.darker(160));
            painter.drawEllipse(center, radius, radius);
            break;
        }
        case Kind::Width: {
            const qreal radius = diameter_ / 2.0;
            if (active_) {
                painter.setPen(QPen(QColor(221, 225, 255), 2.0));
                painter.setBrush(Qt::NoBrush);
                painter.drawEllipse(center, radius + 3.5, radius + 3.5);
            }
            painter.setPen(Qt::NoPen);
            painter.setBrush(content);
            painter.drawEllipse(center, radius, radius);
            break;
        }
        case Kind::Cross: {
            // A drawn glyph keeps the quit button independent of whichever font
            // the session happens to have.
            painter.setPen(QPen(content, 2.0, Qt::SolidLine, Qt::RoundCap));
            const qreal arm = 5.0;
            painter.drawLine(QPointF(center.x() - arm, center.y() - arm),
                             QPointF(center.x() + arm, center.y() + arm));
            painter.drawLine(QPointF(center.x() - arm, center.y() + arm),
                             QPointF(center.x() + arm, center.y() - arm));
            break;
        }
        }
    }

private:
    Kind kind_;
    QString label_;
    QColor swatch_;
    int diameter_ = 0;
    bool hovered_ = false;
    bool pressed_ = false;
    bool active_ = false;
    bool enabled_ = true;
    std::function<void()> onClick_;
};

} // namespace

// The floating palette.  A child widget of the surface, so its buttons keep
// their clicks apart from the canvas without the canvas losing a pixel; the
// blank space around them drags the panel, exactly like the capture overlay's
// FloatingToolbar.
class AnnotateSurface::Toolbar final : public QWidget {
public:
    explicit Toolbar(AnnotateSurface *surface)
        : QWidget(surface)
        , surface_(surface)
    {
        setObjectName(QStringLiteral("vshotAnnotateToolbar"));
        setAttribute(Qt::WA_TranslucentBackground);
        setAutoFillBackground(false);
        // Blank panel areas are the grip; interactive children override.
        setCursor(Qt::SizeAllCursor);

        auto *layout = new QHBoxLayout(this);
        layout->setContentsMargins(kToolbarPad, 6, kToolbarPad, 6);
        layout->setSpacing(4);
        layout->addWidget(new Grip(this));
        layout->addSpacing(4);

        addTool(layout, uiTr("Draw"), AnnotateSurface::Tool::Pen);
        addTool(layout, uiTr("Erase"), AnnotateSurface::Tool::Eraser);
        addTool(layout, uiTr("Rect"), AnnotateSurface::Tool::Rect);
        addTool(layout, uiTr("Arrow"), AnnotateSurface::Tool::Arrow);
        addTool(layout, uiTr("Text"), AnnotateSurface::Tool::Text);

        addDivider(layout);
        // The palette's order is the requirement's: the default red first, then
        // the two most legible colours on a dark panel, then black and white.
        static const QColor palette[] = {
            QColor(0xe5, 0x39, 0x35), QColor(0xfd, 0xd8, 0x35), QColor(0x43, 0xa0, 0x47),
            QColor(0x1e, 0x88, 0xe5), QColor(0x00, 0x00, 0x00), QColor(0xff, 0xff, 0xff),
        };
        for (const QColor &color : palette) {
            addSwatch(layout, color);
        }

        addDivider(layout);
        for (const int width : kWidths) {
            addWidth(layout, width);
        }

        addDivider(layout);
        auto *undo = addText(layout, uiTr("Undo"), [this] { surface_->undo(); });
        auto *redo = addText(layout, uiTr("Redo"), [this] { surface_->redo(); });
        undo_ = undo;
        redo_ = redo;
        addText(layout, uiTr("Clear"), [this] { surface_->clear(); });

        addDivider(layout);
        auto *quit = new ToolbarButton(this, ToolbarButton::Kind::Cross, QString(), QColor(), 0);
        quit->setToolTip(uiTr("Quit annotation"));
        quit->setOnClick([this] {
            if (surface_->quit_) {
                surface_->quit_();
            }
        });
        layout->addWidget(quit);

        setFixedHeight(kToolbarHeight);
        adjustSize();
        syncState();
    }

    void syncState()
    {
        for (const auto &entry : toolButtons_) {
            entry.first->setActive(surface_->tool() == entry.second);
        }
        for (const auto &entry : swatchButtons_) {
            entry.first->setActive(surface_->color() == entry.second);
        }
        for (const auto &entry : widthButtons_) {
            entry.first->setActive(surface_->penWidth() == entry.second);
        }
        if (undo_ != nullptr) {
            undo_->setEnabled(surface_->canUndo());
        }
        if (redo_ != nullptr) {
            redo_->setEnabled(surface_->canRedo());
        }
    }

    // Puts the panel where it belongs: the last dragged position when there is
    // one, otherwise centred under the output's top edge.  Called on every
    // resize, so a dragged panel is pulled back inside a shrunken output.
    void reposition()
    {
        if (surface_->toolbarOrigin_.isNull()) {
            const QPoint centered((surface_->width() - width()) / 2, kToolbarMargin);
            move(clamped(centered));
        } else {
            move(clamped(surface_->toolbarOrigin_));
        }
    }

    void beginDrag(const QPoint &globalPos)
    {
        dragging_ = true;
        dragOffset_ = globalPos - mapToGlobal(QPoint(0, 0));
    }

    void dragTo(QMouseEvent *event)
    {
        if (!dragging_ || !(event->buttons() & Qt::LeftButton)) {
            return;
        }
        const QPoint target = event->globalPosition().toPoint() - dragOffset_;
        const QPoint placed = clamped(surface_->mapFromGlobal(target));
        surface_->toolbarOrigin_ = placed;
        move(placed);
    }

    void endDrag() { dragging_ = false; }

protected:
    void mousePressEvent(QMouseEvent *event) override
    {
        if (event->button() != Qt::LeftButton) {
            QWidget::mousePressEvent(event);
            return;
        }
        if (childAt(event->position().toPoint()) == nullptr) {
            beginDrag(event->globalPosition().toPoint());
        }
        event->accept();
    }

    void mouseMoveEvent(QMouseEvent *event) override
    {
        if (dragging_ && (event->buttons() & Qt::LeftButton)) {
            dragTo(event);
            event->accept();
            return;
        }
        QWidget::mouseMoveEvent(event);
    }

    void mouseReleaseEvent(QMouseEvent *event) override
    {
        if (event->button() != Qt::LeftButton) {
            QWidget::mouseReleaseEvent(event);
            return;
        }
        endDrag();
        event->accept();
    }

    void paintEvent(QPaintEvent *event) override
    {
        Q_UNUSED(event);
        QPainter painter(this);
        painter.setRenderHint(QPainter::Antialiasing, true);
        // A plain rim, no frosted glass and no drop shadow: frosted glass needs
        // a frozen frame (there is none over a live desktop), and a child widget
        // cannot paint outside its own rect, so a shadow would be clipped.
        const QPainterPath shape =
            superellipsePath(QRectF(rect()).adjusted(0.5, 0.5, -0.5, -0.5), kPanelRadius,
                             kSuperellipseExponent);
        painter.setPen(Qt::NoPen);
        painter.setBrush(QColor(30, 34, 41, 204));
        painter.drawPath(shape);
        painter.setBrush(Qt::NoBrush);
        painter.setPen(QPen(QColor(64, 71, 82), 1.0));
        painter.drawPath(shape);
    }

private:
    // The dotted drag handle.  It is its own widget only so it can carry the
    // "Drag to move" tooltip and start the drag itself; it paints nothing but
    // its dots, letting the panel's rim show through.
    class Grip final : public QWidget {
    public:
        explicit Grip(Toolbar *toolbar)
            : QWidget(toolbar)
            , toolbar_(toolbar)
        {
            setToolTip(uiTr("Drag to move"));
            setCursor(Qt::SizeAllCursor);
            setFixedWidth(kGripWidth);
            setSizePolicy(QSizePolicy::Fixed, QSizePolicy::Expanding);
        }

    protected:
        void mousePressEvent(QMouseEvent *event) override
        {
            if (event->button() != Qt::LeftButton) {
                QWidget::mousePressEvent(event);
                return;
            }
            toolbar_->beginDrag(event->globalPosition().toPoint());
            event->accept();
        }

        void mouseMoveEvent(QMouseEvent *event) override { toolbar_->dragTo(event); }

        void mouseReleaseEvent(QMouseEvent *event) override
        {
            if (event->button() == Qt::LeftButton) {
                toolbar_->endDrag();
                event->accept();
                return;
            }
            QWidget::mouseReleaseEvent(event);
        }

        void paintEvent(QPaintEvent *event) override
        {
            Q_UNUSED(event);
            QPainter painter(this);
            painter.setRenderHint(QPainter::Antialiasing, true);
            painter.setPen(Qt::NoPen);
            painter.setBrush(QColor(150, 157, 168));
            const QPointF center = QRectF(rect()).center();
            for (int row = -1; row <= 1; ++row) {
                for (int column = -1; column <= 1; ++column) {
                    painter.drawEllipse(
                        QPointF(center.x() + column * 5.0, center.y() + row * 5.0), 1.4, 1.4);
                }
            }
        }

    private:
        Toolbar *toolbar_;
    };

    QPoint clamped(const QPoint &point) const
    {
        const int maxX = std::max(0, surface_->width() - width());
        const int maxY = std::max(0, surface_->height() - height());
        return QPoint(std::clamp(point.x(), 0, maxX), std::clamp(point.y(), 0, maxY));
    }

    void addTool(QHBoxLayout *layout, const QString &label, AnnotateSurface::Tool tool)
    {
        auto *button = new ToolbarButton(this, ToolbarButton::Kind::Label, label, QColor(), 0);
        button->setAccessibleName(label);
        button->setOnClick([this, tool] { surface_->setTool(tool); });
        layout->addWidget(button);
        toolButtons_.append(qMakePair(button, tool));
    }

    void addSwatch(QHBoxLayout *layout, const QColor &color)
    {
        auto *button =
            new ToolbarButton(this, ToolbarButton::Kind::Swatch, QString(), color, 0);
        button->setOnClick([this, color] { surface_->setColor(color); });
        layout->addWidget(button);
        swatchButtons_.append(qMakePair(button, color));
    }

    void addWidth(QHBoxLayout *layout, int width)
    {
        // The dot's diameter is the width made visible, so the three dots read
        // as thin, medium and thick before they are clicked.
        auto *button = new ToolbarButton(this, ToolbarButton::Kind::Width, QString(), QColor(),
                                         width + 5);
        button->setOnClick([this, width] { surface_->setPenWidth(width); });
        layout->addWidget(button);
        widthButtons_.append(qMakePair(button, width));
    }

    ToolbarButton *addText(QHBoxLayout *layout, const QString &label, std::function<void()> onClick)
    {
        auto *button = new ToolbarButton(this, ToolbarButton::Kind::Label, label, QColor(), 0);
        button->setAccessibleName(label);
        button->setOnClick(std::move(onClick));
        layout->addWidget(button);
        return button;
    }

    void addDivider(QHBoxLayout *layout)
    {
        auto *line = new QFrame(this);
        line->setFixedSize(1, 20);
        line->setStyleSheet(QStringLiteral("background: #3a414b; border: none;"));
        // Transparent to the pointer: a click on the separator is a click on
        // blank panel, and drags the panel like any other gap.
        line->setAttribute(Qt::WA_TransparentForMouseEvents);
        layout->addWidget(line);
    }

    AnnotateSurface *surface_ = nullptr;
    QVector<QPair<ToolbarButton *, AnnotateSurface::Tool>> toolButtons_;
    QVector<QPair<ToolbarButton *, QColor>> swatchButtons_;
    QVector<QPair<ToolbarButton *, int>> widthButtons_;
    ToolbarButton *undo_ = nullptr;
    ToolbarButton *redo_ = nullptr;
    bool dragging_ = false;
    QPoint dragOffset_;
};

// The inline text editor.  Enter commits, Escape cancels, losing focus commits.
// No input validator: an ASCII validator here would silently drop input-method
// commits, and a Chinese label has to survive entry.
class AnnotateTextEdit final : public QLineEdit {
public:
    using Finished = std::function<void(bool)>;

    AnnotateTextEdit(QWidget *parent, Finished finished)
        : QLineEdit(parent)
        , finished_(std::move(finished))
    {
        setAttribute(Qt::WA_DeleteOnClose, false);
        setFrame(true);
        setPlaceholderText(uiTr("Text"));
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

    void focusOutEvent(QFocusEvent *event) override
    {
        QLineEdit::focusOutEvent(event);
        if (finished_) {
            finished_(true);
        }
    }

private:
    Finished finished_;
};

AnnotateSurface::AnnotateSurface(QScreen *screen)
    : QWidget(nullptr, Qt::Tool | Qt::FramelessWindowHint)
    , screen_(screen)
{
    setAttribute(Qt::WA_TranslucentBackground);
    setAttribute(Qt::WA_DeleteOnClose);
    setMouseTracking(true);
    setFocusPolicy(Qt::ClickFocus);
    setCursor(Qt::CrossCursor);
    if (screen_ != nullptr) {
        // One surface per output, covering all of it.  The compositor confirms
        // this through the layer configure; resizing here only avoids a
        // wrong-size first frame.
        resize(screen_->geometry().size());
    }
    toolbar_ = new Toolbar(this);
    toolbar_->reposition();
    toolbar_->show();
}

AnnotateSurface::~AnnotateSurface() = default;

bool AnnotateSurface::showLayerSurface()
{
    winId();
    QWindow *window = windowHandle();
    if (window == nullptr) {
        return false;
    }
    auto *layer = LayerShellQt::Window::get(window);
    if (layer == nullptr) {
        return false;
    }
    layer->setLayer(LayerShellQt::Window::LayerOverlay);
    // All four edges anchored with zero margins: the canvas is the whole output,
    // so nothing about the annotation ever involves a compositor round trip.
    LayerShellQt::Window::Anchors anchors(LayerShellQt::Window::AnchorTop);
    anchors |= LayerShellQt::Window::AnchorBottom;
    anchors |= LayerShellQt::Window::AnchorLeft;
    anchors |= LayerShellQt::Window::AnchorRight;
    layer->setAnchors(anchors);
    layer->setExclusiveZone(-1);
    // No keyboard to begin with.  This surface covers every window on the
    // output, and a surface that held the keyboard would stop the user typing
    // in whatever is underneath; drawing a stroke needs no keys.  Only the
    // text editor raises it, through setKeyboardWanted().
    layer->setKeyboardInteractivity(LayerShellQt::Window::KeyboardInteractivityNone);
    layer_ = layer;
    keyboardWanted_ = false;
    layer->setScope(QStringLiteral("vshot-annotate"));
    layer->setDesiredSize(QSize(0, 0)); // follow the anchored edges
    layer->setScreen(screen_);
    surfaceReady_ = true;
    show();
    return true;
}

void AnnotateSurface::setTool(Tool tool)
{
    if (tool_ == tool) {
        return;
    }
    tool_ = tool;
    if (toolbar_ != nullptr) {
        toolbar_->syncState();
    }
}

void AnnotateSurface::setColor(const QColor &color)
{
    color_ = color;
    if (toolbar_ != nullptr) {
        toolbar_->syncState();
    }
}

void AnnotateSurface::setPenWidth(int width)
{
    // Clamped to the range the capture editor's own width control uses, so a
    // value arriving from outside the toolbar cannot shrink a stroke to
    // nothing or blow it up past a usable line.
    width_ = std::clamp(width, 1, 64);
    if (toolbar_ != nullptr) {
        toolbar_->syncState();
    }
}

int AnnotateSurface::strokeCount() const
{
    return static_cast<int>(strokes_.size());
}

bool AnnotateSurface::isEmpty() const
{
    return strokes_.isEmpty();
}

bool AnnotateSurface::canUndo() const
{
    return !undoStack_.isEmpty();
}

bool AnnotateSurface::canRedo() const
{
    return !redoStack_.isEmpty();
}

QRect AnnotateSurface::toolbarRect() const
{
    if (toolbarHidden_ || toolbar_ == nullptr) {
        return QRect();
    }
    return toolbar_->geometry();
}

void AnnotateSurface::setToolbarHidden(bool hidden)
{
    if (toolbarHidden_ == hidden) {
        return;
    }
    toolbarHidden_ = hidden;
    if (toolbar_ == nullptr) {
        return;
    }
    const QRect rect = toolbar_->geometry();
    toolbar_->setVisible(!hidden);
    // Only the panel's rect: hiding it must not repaint the canvas, and showing
    // it must have the canvas underneath it repainted before the button glow.
    if (rect.isValid()) {
        update(rect);
    }
}

double AnnotateSurface::deviceRatio() const
{
    return screen_ != nullptr ? screen_->devicePixelRatio() : 1.0;
}

void AnnotateSurface::paintStroke(QPainter &painter, const Stroke &stroke) const
{
    paintStrokeInk(painter, stroke.tool, stroke.points, stroke.color, stroke.width, stroke.text);
}

const AnnotateSurface::StrokeRaster *AnnotateSurface::rasterFor(Stroke &stroke)
{
    if (stroke.tool == Tool::Eraser) {
        return nullptr;
    }
    const QRectF bounds = strokeBounds(stroke.tool, stroke.points, stroke.width, stroke.text);
    if (bounds.isNull()) {
        return nullptr;
    }
    const double ratio = deviceRatio();
    if (stroke.raster != nullptr && stroke.raster->ratio == ratio) {
        return stroke.raster.get();
    }
    const QRect device = deviceDirtyRect(stroke);
    if (device.isEmpty()) {
        return nullptr;
    }
    auto raster = std::make_shared<StrokeRaster>();
    raster->logicalBounds = bounds;
    raster->origin = device.topLeft();
    raster->ratio = ratio;
    raster->image = QImage(device.size(), QImage::Format_ARGB32_Premultiplied);
    raster->image.fill(Qt::transparent);
    {
        QPainter painter(&raster->image);
        painter.setRenderHint(QPainter::Antialiasing, true);
        // Scaled by the device ratio and shifted by whole device pixels, so the
        // ink lands on exactly the pixels it landed on when every stroke was
        // drawn into one canvas: same scale, same antialiasing phase.
        painter.setTransform(QTransform(ratio, 0.0, 0.0, ratio,
                                        -static_cast<qreal>(device.x()),
                                        -static_cast<qreal>(device.y())));
        paintStroke(painter, stroke);
    }
    ++rasterBuilds_;
    stroke.raster = raster;
    return stroke.raster.get();
}

QRect AnnotateSurface::deviceDirtyRect(const Stroke &stroke) const
{
    const QRectF box =
        strokeBounds(stroke.tool, stroke.points, stroke.width, stroke.text);
    if (box.isNull()) {
        return QRect();
    }
    const double ratio = deviceRatio();
    const int left = static_cast<int>(std::floor(box.left() * ratio)) - 1;
    const int top = static_cast<int>(std::floor(box.top() * ratio)) - 1;
    const int right = static_cast<int>(std::ceil(box.right() * ratio)) + 1;
    const int bottom = static_cast<int>(std::ceil(box.bottom() * ratio)) + 1;
    return QRect(QPoint(left, top), QPoint(right, bottom));
}

void AnnotateSurface::touch(const QRect &logical)
{
    if (!logical.isValid()) {
        return;
    }
    // Accumulated as well as sent: the checks compare one interactive step's
    // repaint region against the pixels that step changed.
    invalidated_ = invalidated_.united(logical);
    update(logical);
}

bool AnnotateSurface::strokeHits(const Stroke &stroke, const QPointF &local) const
{
    if (stroke.tool == Tool::Text) {
        return rectOutlineDistance(local, textBounds(stroke.points, stroke.width, stroke.text))
            <= kEraserRadius;
    }
    if (stroke.points.isEmpty()) {
        return false;
    }
    if (stroke.tool == Tool::Rect && stroke.points.size() >= 2) {
        return rectOutlineDistance(
                   local, normalizedRect(stroke.points.constFirst(), stroke.points.constLast()))
            <= kEraserRadius;
    }
    double nearest = std::numeric_limits<double>::infinity();
    if (stroke.points.size() == 1) {
        nearest = QLineF(local, stroke.points.constFirst()).length();
    } else {
        for (qsizetype i = 1; i < stroke.points.size(); ++i) {
            nearest = std::min(
                nearest,
                pointSegmentDistance(local, stroke.points.at(i - 1), stroke.points.at(i)));
        }
        if (stroke.tool == Tool::Arrow) {
            // The filled head is ink too: take a stroke the eraser lands on the
            // head of, not just one it lands on the shaft of.
            const QVector<QPointF> head = arrowHeadPoints(
                stroke.points.constLast(), stroke.points.at(stroke.points.size() - 2),
                stroke.width);
            if (head.size() == 3) {
                if (pointInTriangle(local, head.at(0), head.at(1), head.at(2))) {
                    return true;
                }
                nearest = std::min(
                    {nearest, pointSegmentDistance(local, head.at(0), head.at(1)),
                     pointSegmentDistance(local, head.at(1), head.at(2)),
                     pointSegmentDistance(local, head.at(2), head.at(0))});
            }
        }
    }
    return nearest <= kEraserRadius;
}

bool AnnotateSurface::eraseStrokeAt(const QPointF &local)
{
    // Frontmost first: the ink on top is the ink the eraser would visibly take.
    for (qsizetype i = strokes_.size(); i-- > 0;) {
        const Stroke &stroke = strokes_.at(i);
        if (!strokeHits(stroke, local)) {
            continue;
        }
        const QRectF box =
            strokeBounds(stroke.tool, stroke.points, stroke.width, stroke.text);
        const QRect dirty = grownDirtyRect(box);
        pushHistory(dirty);
        // Dropping the stroke out of the list is the whole change: every other
        // stroke owns its own cached ink, so nothing else has to be rebuilt.
        strokes_.removeAt(i);
        touch(dirty);
        if (toolbar_ != nullptr) {
            toolbar_->syncState();
        }
        return true;
    }
    return false;
}

void AnnotateSurface::pushHistory(const QRect &dirty)
{
    undoStack_.append(HistoryEntry{strokes_, dirty});
    // Any fresh change makes the redo branch unreachable.
    redoStack_.clear();
}

void AnnotateSurface::undo()
{
    if (undoStack_.isEmpty()) {
        return;
    }
    const HistoryEntry entry = undoStack_.takeLast();
    // The state being replaced keeps the entry's rect: the region that differs
    // is the same in both directions.
    redoStack_.append(HistoryEntry{strokes_, entry.dirty});
    strokes_ = entry.strokes;
    touch(entry.dirty);
    if (toolbar_ != nullptr) {
        toolbar_->syncState();
    }
}

void AnnotateSurface::redo()
{
    if (redoStack_.isEmpty()) {
        return;
    }
    const HistoryEntry entry = redoStack_.takeLast();
    // Symmetric to undo: the entry's rect is the region that has to be
    // repainted, whichever way the step goes.
    undoStack_.append(HistoryEntry{strokes_, entry.dirty});
    strokes_ = entry.strokes;
    touch(entry.dirty);
    if (toolbar_ != nullptr) {
        toolbar_->syncState();
    }
}

void AnnotateSurface::clear()
{
    if (strokes_.isEmpty()) {
        return;
    }
    QRect dirty;
    for (const Stroke &stroke : strokes_) {
        dirty = dirty.united(grownDirtyRect(
            strokeBounds(stroke.tool, stroke.points, stroke.width, stroke.text)));
    }
    pushHistory(dirty);
    strokes_.clear();
    touch(dirty);
    if (toolbar_ != nullptr) {
        toolbar_->syncState();
    }
}

void AnnotateSurface::setKeyboardWanted(bool wanted)
{
    if (layer_ == nullptr || keyboardWanted_ == wanted) {
        return;
    }
    keyboardWanted_ = wanted;
    // Exclusive while a label is being typed, none otherwise: the change only
    // reaches the compositor with the next commit, which requestUpdate
    // schedules so the keyboard arrives (or leaves) now rather than at some
    // later repaint.
    layer_->setKeyboardInteractivity(wanted
                                         ? LayerShellQt::Window::KeyboardInteractivityExclusive
                                         : LayerShellQt::Window::KeyboardInteractivityNone);
    if (QWindow *window = windowHandle()) {
        window->requestUpdate();
    }
}

void AnnotateSurface::beginText(const QPointF &local)
{
    const QFont font = annotationFont(width_);
    const QFontMetrics metrics(font);
    const int boxWidth = std::min(360, std::max(160, width() - 16));
    const int boxHeight = std::max(20, metrics.height() + 6);
    const int x = std::clamp(static_cast<int>(std::lround(local.x())), 4,
                             std::max(4, width() - boxWidth - 4));
    const int y = std::clamp(static_cast<int>(std::lround(local.y())), 4,
                             std::max(4, height() - boxHeight - 4));
    textOrigin_ = local;
    if (textEdit_ != nullptr) {
        // Only one label is being typed at a time: a second click moves the box
        // instead of opening another.
        textEdit_->setFont(font);
        textEdit_->setGeometry(x, y, boxWidth, boxHeight);
        textEdit_->setFocus(Qt::OtherFocusReason);
        return;
    }
    textEdit_ = new AnnotateTextEdit(this, [this](bool accept) { finishText(accept); });
    textEdit_->setFont(font);
    textEdit_->setStyleSheet(QStringLiteral("QLineEdit { color: %1; background: rgba(0, 0, 0, 140); "
                                            "border: 1px solid #888888; padding: 0 3px; }")
                                 .arg(color_.name()));
    textEdit_->setGeometry(x, y, boxWidth, boxHeight);
    textEdit_->show();
    textEdit_->raise();
    // Ask for the keyboard before focusing, or the compositor never gives this
    // surface the keys the editor needs.
    setKeyboardWanted(true);
    textEdit_->setFocus(Qt::OtherFocusReason);
}

void AnnotateSurface::finishText(bool accept)
{
    if (textEdit_ == nullptr) {
        return;
    }
    const QString value = textEdit_->text();
    QLineEdit *editor = textEdit_;
    // Cleared before the editor is hidden: hiding fires focus-out, whose
    // commit would otherwise re-enter this function.
    textEdit_ = nullptr;
    editor->hide();
    editor->deleteLater();
    setKeyboardWanted(false);
    if (!accept || value.isEmpty()) {
        return;
    }
    Stroke stroke;
    stroke.tool = Tool::Text;
    stroke.color = color_;
    stroke.width = width_;
    stroke.points = {textOrigin_};
    stroke.text = value;
    const QRect dirty = grownDirtyRect(strokeBounds(stroke.tool, stroke.points, stroke.width,
                                                    stroke.text));
    pushHistory(dirty);
    // The label's ink is built on the next paint by `rasterFor`, like any other
    // stroke's.
    strokes_.append(stroke);
    touch(dirty);
    if (toolbar_ != nullptr) {
        toolbar_->syncState();
    }
}

void AnnotateSurface::paintEvent(QPaintEvent *event)
{
    QPainter painter(this);
    // Clear the repainted region to transparent first: the strokes are blitted
    // with SourceOver, and apart from its ink this surface is see-through.
    painter.setCompositionMode(QPainter::CompositionMode_Source);
    painter.fillRect(event->rect(), Qt::transparent);
    painter.setCompositionMode(QPainter::CompositionMode_SourceOver);
    // Each stroke is a device-pixel image blitted at 1:1, so smoothing would
    // only cost time and soften the ink.
    painter.setRenderHint(QPainter::SmoothPixmapTransform, false);
    // A repaint narrowed to what a gesture touched must not pay for the strokes
    // outside it.  The clip is in this widget's logical coordinates, the same
    // ones a stroke's bounds are in.
    const QRectF exposed = painter.hasClipping() ? painter.clipBoundingRect() : QRectF(rect());
    for (Stroke &stroke : strokes_) {
        const StrokeRaster *raster = rasterFor(stroke);
        if (raster == nullptr || !raster->logicalBounds.intersects(exposed)) {
            continue;
        }
        const double ratio = raster->ratio;
        const QRectF target(static_cast<qreal>(raster->origin.x()) / ratio,
                            static_cast<qreal>(raster->origin.y()) / ratio,
                            static_cast<qreal>(raster->image.width()) / ratio,
                            static_cast<qreal>(raster->image.height()) / ratio);
        painter.drawImage(target, raster->image, QRectF(raster->image.rect()));
    }
    // The stroke being drawn lives only in `pending_`: the preview is the one
    // thing this surface paints from the model rather than from cached ink, and
    // every tool goes through it now.
    if (drawing_) {
        painter.setRenderHint(QPainter::Antialiasing, true);
        paintStroke(painter, pending_);
    }
}

void AnnotateSurface::mousePressEvent(QMouseEvent *event)
{
    if (event->button() != Qt::LeftButton) {
        QWidget::mousePressEvent(event);
        return;
    }
    const QPointF local = event->position();
    // A click on the canvas commits any open label first.  The editor is a
    // child widget, so a click inside it never reaches here at all.
    if (textEdit_ != nullptr) {
        finishText(true);
    }
    switch (tool_) {
    case Tool::Pen:
        pending_ = Stroke();
        pending_.tool = Tool::Pen;
        pending_.color = color_;
        pending_.width = width_;
        pending_.points = {local};
        drawing_ = true;
        // The preview paints the first dot straight away; nothing is committed
        // until the button comes up.
        touch(grownDirtyRect(
            strokeBounds(pending_.tool, pending_.points, pending_.width, pending_.text)));
        break;
    case Tool::Rect:
    case Tool::Arrow:
        pending_ = Stroke();
        pending_.tool = tool_;
        pending_.color = color_;
        pending_.width = width_;
        pending_.points = {local, local};
        drawing_ = true;
        break;
    case Tool::Text:
        beginText(local);
        break;
    case Tool::Eraser:
        erasing_ = true;
        eraseFrom_ = local;
        eraseStrokeAt(local);
        break;
    }
}

void AnnotateSurface::mouseMoveEvent(QMouseEvent *event)
{
    const QPointF local = event->position();
    if (erasing_ && (event->buttons() & Qt::LeftButton)) {
        // The eraser's own path is walked, not just its current point: a fast
        // drag sends few events, and a stroke between two of them must still be
        // taken.  Half the eraser's radius keeps the sampled discs overlapping.
        const QPointF from = eraseFrom_;
        const double distance = QLineF(from, local).length();
        const int steps =
            std::max(1, static_cast<int>(std::ceil(distance / (kEraserRadius * 0.5))));
        for (int i = 1; i <= steps; ++i) {
            const double t = static_cast<double>(i) / steps;
            eraseStrokeAt(QPointF(from.x() + (local.x() - from.x()) * t,
                                  from.y() + (local.y() - from.y()) * t));
        }
        eraseFrom_ = local;
        return;
    }
    if (!drawing_ || !(event->buttons() & Qt::LeftButton)) {
        QWidget::mouseMoveEvent(event);
        return;
    }
    if (tool_ == Tool::Pen) {
        const QPointF from = pending_.points.constLast();
        pending_.points.append(local);
        // The preview paints the whole polyline from the model, so only the part
        // this step can have changed needs repainting.
        touch(grownDirtyRect(
            normalizedRect(from, local)
                .adjusted(-pending_.width, -pending_.width, pending_.width, pending_.width)));
    } else if (tool_ == Tool::Rect || tool_ == Tool::Arrow) {
        const QRect before = grownDirtyRect(
            strokeBounds(pending_.tool, pending_.points, pending_.width, pending_.text));
        if (pending_.points.size() >= 2) {
            pending_.points.last() = local;
        }
        const QRect after = grownDirtyRect(
            strokeBounds(pending_.tool, pending_.points, pending_.width, pending_.text));
        if (after != before) {
            // Repaint what the shape left before drawing the new preview.
            touch(before);
            touch(after);
        }
    }
}

void AnnotateSurface::mouseReleaseEvent(QMouseEvent *event)
{
    if (erasing_ && event->button() == Qt::LeftButton) {
        erasing_ = false;
        eraseFrom_ = QPointF();
    }
    if (event->button() != Qt::LeftButton) {
        QWidget::mouseReleaseEvent(event);
        return;
    }
    if (!drawing_) {
        QWidget::mouseReleaseEvent(event);
        return;
    }
    drawing_ = false;
    QRect stalePreview;
    if (tool_ == Tool::Rect || tool_ == Tool::Arrow) {
        // The preview on screen was drawn where the pointer was at the last
        // motion event.  Releasing a little away from it can shrink the shape,
        // leaving the old outline outside the committed rect -- so the rect the
        // preview occupied is remembered and repainted as well.
        stalePreview = grownDirtyRect(
            strokeBounds(pending_.tool, pending_.points, pending_.width, pending_.text));
        if (pending_.points.size() >= 2) {
            pending_.points.last() = event->position();
        }
    }
    if (!pending_.points.isEmpty()) {
        const Stroke committed = pending_;
        pending_ = Stroke();
        QRect dirty = grownDirtyRect(strokeBounds(committed.tool, committed.points,
                                                  committed.width, committed.text));
        if (stalePreview.isValid()) {
            dirty = dirty.united(stalePreview);
        }
        pushHistory(dirty);
        strokes_.append(committed);
        // The stroke appears in one go at release, so its whole rect has to be
        // repainted: the preview is gone and the committed ink is drawn instead.
        touch(dirty);
        if (toolbar_ != nullptr) {
            toolbar_->syncState();
        }
        return;
    }
    pending_ = Stroke();
}

void AnnotateSurface::resizeEvent(QResizeEvent *event)
{
    QWidget::resizeEvent(event);
    // Nothing to rebuild: a stroke's cached ink is sized in device pixels and
    // does not depend on the surface's size, so a resize only moves the toolbar.
    if (toolbar_ != nullptr) {
        toolbar_->reposition();
    }
}

void AnnotateSurface::keyPressEvent(QKeyEvent *event)
{
    if (event->key() == Qt::Key_Escape) {
        if (textEdit_ != nullptr) {
            finishText(false);
            event->accept();
            return;
        }
        if (drawing_) {
            // Drop the in-progress stroke.  It lives only in `pending_`, which
            // the preview paints, so repainting its rect is the whole undo.
            const QRect dirty = grownDirtyRect(
                strokeBounds(pending_.tool, pending_.points, pending_.width, pending_.text));
            drawing_ = false;
            pending_ = Stroke();
            touch(dirty);
            event->accept();
            return;
        }
    }
    const bool control = event->modifiers() & Qt::ControlModifier;
    if (control && event->key() == Qt::Key_Z) {
        if (event->modifiers() & Qt::ShiftModifier) {
            redo();
        } else {
            undo();
        }
        event->accept();
        return;
    }
    if (control && event->key() == Qt::Key_Y) {
        redo();
        event->accept();
        return;
    }
    QWidget::keyPressEvent(event);
}

} // namespace vshot
