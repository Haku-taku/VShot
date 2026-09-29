// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#pragma once

#include "session_protocol.hpp"
#include "text_layer.hpp"
#include "translation_layer.hpp"

#include <QByteArray>
#include <QColor>
#include <QElapsedTimer>
#include <QHash>
#include <QImage>
#include <QJsonDocument>
#include <QPointF>
#include <QRect>
#include <QString>
#include <QVector>
#include <QWidget>

#include <functional>
#include <memory>
#include <optional>

class QPainter;
class QScreen;
class QSlider;
class QSpinBox;
class QLabel;
class QWindow;
class QLocalSocket;
class QSocketNotifier;
class QTimer;

namespace vshot {

// Rasterizes one annotation and remembers the result.  Defined in the Qt
// helper; `Annotation` only holds its cache.  Forward-declared so the cache
// stays a `std::shared_ptr` and a copy of an annotation (an undo snapshot, a
// drag ghost) stays cheap.
class AnnotationRaster;

struct Point {
    std::int32_t x = 0;
    std::int32_t y = 0;
};

inline bool operator==(const Point &first, const Point &second)
{
    return first.x == second.x && first.y == second.y;
}

// The looks a numbered badge can be drawn with.  The number tool is one tool;
// these four are the styles the editor offers for it.
enum class NumberStyle {
    // A filled disc with the count knocked out of it -- the ①②③ look.
    FilledCircle,
    // A hollow ring whose line is the current stroke width.
    Ring,
    // A rounded square filled like the disc.
    Square,
    // No background at all: the glyphs alone, given a thin contrasting halo so
    // they stay readable over a busy frame.
    Plain,
};

struct Annotation {
    enum class Kind {
        Shape,
        Stroke,
        Text,
        // A pasted image. `pixels` holds the source at its own resolution and
        // `rect` is where it lands on the canvas: the two differ because a
        // pasted image is scaled to fit inside the selection and the user can
        // then resize it with the handles. The pixels travel to the renderer as
        // a raw RGBA8888 file, the same way a text label's bitmap does.
        Image,
        // A translated capture: one placed line per recognized line, drawn over
        // the text it replaces. `translation` holds the geometry, the text and
        // the colours, all settled when the translation came back, and `rect`
        // is the union of the filled boxes -- what the hit test and the drag
        // clamp read. It travels to the renderer as an image bitmap, so the
        // Rust side needs no knowledge of it at all.
        Translation,
    };

    Kind kind = Kind::Stroke;
    QString tool;
    LogicalRect rect;
    QVector<Point> points;
    Point origin;
    QString text;
    // Whether a bezier path was closed back onto its first anchor (`tool ==
    // "bezier"` only).  A closed path is filled as well as stroked, so this is
    // content rather than decoration: it changes the pixels and travels to the
    // renderer as its own field.
    bool closed = false;
    // Font height in logical pixels, exactly as the size box shows it.  The
    // legacy integer `scale` the JSON protocol carries is derived from this
    // only when the result is written out (`textPixelsToScale`).
    std::uint32_t textPixels = 14;
    QColor color{255, 64, 64, 255};
    std::uint32_t width = 1;
    // Line style: "solid" | "dashed" | "dotted".
    QString dash = QStringLiteral("solid");
    // Arrow head size multiplier.
    std::uint32_t size = 1;
    // Arrow head style: "open" | "filled".
    QString arrowStyle = QStringLiteral("open");
    // Mosaic area shape: "rect" | "ellipse".
    QString mask = QStringLiteral("rect");
    // Mosaic strength level 1..3 (block size / smear radius factor).
    std::uint32_t strength = 2;
    // Text font family; empty resolves to the application default font.
    QString font;
    // A numbered badge (`Kind::Text` with `tool == "number"`): which of the four
    // looks it is drawn with, and the count it carries.  `number` is the value
    // the digit string is written from -- the model keeps one source of truth
    // for it so that two badges at the same place with different counts are
    // plainly different marks, which is what the undo comparison needs.
    NumberStyle numberStyle = NumberStyle::FilledCircle;
    int number = 0;
    // Diameter of a numbered badge, in logical pixels.  The badge's size used to
    // be derived from the stroke width, so the width slider silently resized a
    // placed badge; it is a value of its own now, and `width` means nothing to a
    // badge.
    std::uint32_t numberSize = 18;
    // A wave's shape, in logical pixels: how far a crest leaves the line its two
    // points describe, and how long one full period is.  Zero means "derive it
    // from the stroke width" -- which is what a wave that was never tuned keeps,
    // so the default look is unchanged and the wire may leave both out.
    std::uint32_t amplitude = 0;
    std::uint32_t wavelength = 0;
    // How a bezier path is painted: "stroke" outlines it, "fill" fills it, and
    // "both" does both.  "both" fills only a path that was actually closed, so
    // it is exactly what the editor did before this field existed.
    QString fill = QStringLiteral("both");
    // Output scale the label was drawn on; the text bitmap is rasterized at
    // this device ratio.
    std::uint32_t deviceRatio = 1;
    // The pasted image itself, at its own resolution (`Kind::Image` only).
    QImage pixels;
    // The placed translation (`Kind::Translation` only): one entry per
    // recognized line, holding the rectangle it replaces, the fill and the text
    // drawn in it.  Everything the paint needs is here, so the preview and the
    // committed bitmap are drawn from the same numbers.
    QVector<TranslatedLine> translation;
    // The last rasterized form of this annotation, kept so a repaint can blit
    // it instead of drawing the mark again.  A mosaic preview averages the
    // source image block by block, so redrawing every mark on every pointer
    // move is what made a busy capture stutter; the raster rebuilds itself
    // only when something the mark draws changes.  Shared (so copies are
    // cheap) and deliberately ignored by `annotationEquals`: it is derived
    // state, not content.
    mutable std::shared_ptr<AnnotationRaster> raster;

    // How many times `raster` has been built, or -1 when it has not been built
    // yet.  Lets the offline check tell a repaint that reused the cache from
    // one that rasterized the mark again, without seeing the raster's type.
    int rasterRebuilds() const;

    // The device-pixel ratio `raster` was built at, or 0 while it has not been
    // built.  Lets the offline check prove a high-DPI capture rasterizes at the
    // screen's resolution instead of blurring.
    qreal rasterDeviceRatio() const;
};

inline bool annotationEquals(const Annotation &first, const Annotation &second)
{
    if (first.kind != second.kind || first.tool != second.tool || first.dash != second.dash ||
        first.size != second.size || first.arrowStyle != second.arrowStyle ||
        first.mask != second.mask || first.strength != second.strength ||
        first.textPixels != second.textPixels || first.color != second.color ||
        first.width != second.width || first.font != second.font ||
        first.deviceRatio != second.deviceRatio ||
        // A numbered badge's count and style are content, not decoration: leave
        // either out and two badges that differ only in their number compare
        // equal, which silently collapses an undo step.
        first.numberStyle != second.numberStyle || first.number != second.number ||
        // A badge's diameter is the badge: two badges of different sizes are
        // plainly different marks.
        first.numberSize != second.numberSize ||
        // A wave's tuned shape is content too -- dragging the amplitude slider
        // must be an undoable change, not a repaint the comparison eats.
        first.amplitude != second.amplitude || first.wavelength != second.wavelength ||
        // And so is a pen path's paint mode: the same outline filled and merely
        // stroked are different pictures.
        first.fill != second.fill ||
        // Likewise for a bezier path's closure: an open curve and the closed
        // one that fills it are different marks, and leaving this out would let
        // an undo step that only closes a path look like no change at all.
        first.closed != second.closed ||
        // A translation's placed lines are its whole content: two of them that
        // differ in a background, a font or a box are different pictures.
        first.translation != second.translation) {
        return false;
    }
    if (first.rect.x != second.rect.x || first.rect.y != second.rect.y ||
        first.rect.width != second.rect.width || first.rect.height != second.rect.height) {
        return false;
    }
    if (first.points != second.points || first.origin.x != second.origin.x ||
        first.origin.y != second.origin.y || first.text != second.text) {
        return false;
    }
    // Comparing pixel buffers would copy megabytes per undo snapshot; the cache
    // key identifies the same image without touching it.
    if (first.pixels.cacheKey() != second.pixels.cacheKey()) {
        return false;
    }
    return true;
}

/// The style one tool draws with.
///
/// The editor used to keep a single colour and width for every tool, so moving
/// the rectangle's width slider also moved the pen's.  Each tool owns its
/// values now: the style row shows and edits the values of the tool it is
/// pointed at -- the selected annotation's tool while one is selected, and the
/// armed tool otherwise, which is what `styleTargetTool` answers.
///
/// `numberSize`, `amplitude` and `wavelength` mean something to one tool each
/// and are kept here anyway, so that "the style of a tool" stays one object
/// rather than a mix of shared and per-tool state.
struct ToolStyle {
    QColor color{255, 64, 64, 255};
    /// Stroke width in logical pixels: a shape's outline, a segment's
    /// thickness, the mosaic brush's radius base.
    std::uint32_t width = 2;
    /// Diameter of a numbered badge, in logical pixels.
    std::uint32_t numberSize = 18;
    /// A wave's crest offset and period, in logical pixels.
    std::uint32_t amplitude = 4;
    std::uint32_t wavelength = 18;
};

enum class Tool {
    Select,
    Rectangle,
    Ellipse,
    Arrow,
    // The two straight, two-point tools: a plain segment and the same segment
    // turned into a sine wave.  Both are drawn from exactly the two points the
    // drag made, like the arrow, so neither accumulates points as the pointer
    // wanders.
    Line,
    Wave,
    // The pen: clicks lay down anchors and the drag after each one pulls its
    // outgoing handle out, so every segment is a cubic and the handle is
    // symmetric by construction.  It is the one tool here whose gesture is not
    // a single drag -- a path spans as many presses as it has anchors, and ends
    // either closed back onto its first anchor or double-clicked open.
    Bezier,
    Pen,
    Text,
    // A numbered badge: one click places one badge and the count advances.  It
    // travels as a text annotation with a bitmap, so the renderer needs no
    // knowledge of it at all.
    Number,
    Mosaic,
};

class CaptureOverlay;

/// What the toolbar's text button has to say: the recognition is running, the
/// text mode is up and waiting for a gesture, the copy landed, or the last
/// attempt failed.  One callback carries all of them so the button never has to
/// guess which of its own clicks it is answering.
enum class TextOutcome {
    /// The recognition run is in flight; the button stays on this label until
    /// the outcome that follows replaces it.
    Busy,
    /// Nothing to report: the mode is up and the button goes back to its own
    /// label, waiting for the copy the user is about to ask for.
    Idle,
    Copied,
    Failed,
};

class OverlayController final {
public:
    explicit OverlayController(Session session);
    ~OverlayController();

    OverlayController(const OverlayController &) = delete;
    OverlayController &operator=(const OverlayController &) = delete;

    int outputCount() const;
    const Session &session() const;
    CaptureOverlay *addOverlay(int outputIndex, QScreen *screen, QString *error);
    // The overlay-local rect the last interactive step asked to be repainted,
    // or a null rect when that step asked for the whole surface, which happens
    // on the first step of a gesture.  Read by the offline check, which compares
    // two full renders around a step and proves the pixels that changed all fall
    // inside it; it runs a single output, so the last overlay the step reached
    // is the one that matters.
    QRect lastInteractiveUpdate() const;

    void paint(CaptureOverlay *overlay, QPainter *painter);
    void press(CaptureOverlay *overlay, const QPointF &local, Qt::MouseButton button,
               Qt::KeyboardModifiers modifiers);
    void move(CaptureOverlay *overlay, const QPointF &local, Qt::MouseButtons buttons,
              Qt::KeyboardModifiers modifiers);
    void release(CaptureOverlay *overlay, const QPointF &local, Qt::MouseButton button,
                 Qt::KeyboardModifiers modifiers);
    void doubleClick(CaptureOverlay *overlay, const QPointF &local, Qt::MouseButton button);
    void key(CaptureOverlay *overlay, int key, Qt::KeyboardModifiers modifiers);

    void chooseTool(Tool tool);
    void setCurrentColor(const QColor &color);
    void setCurrentFont(const QString &family);
    void setWidth(std::uint32_t width);
    void setDash(const QString &dash);
    void setArrowSize(std::uint32_t size);
    void setArrowStyle(const QString &style);
    void setTextSize(std::uint32_t size);
    void setMosaicShape(const QString &shape);
    void setMosaicStrength(std::uint32_t strength);
    /// Diameter of the next numbered badge, in logical pixels.  A badge's size
    /// is a value of its own now; the width control no longer reaches it.
    void setNumberSize(std::uint32_t size);
    /// The wave's crest offset and period, in logical pixels.
    void setWaveAmplitude(std::uint32_t amplitude);
    void setWaveWavelength(std::uint32_t wavelength);
    /// How the next pen path is painted: "stroke" | "fill" | "both".
    void setFill(const QString &fill);
    /// The style of one tool, by its wire name (see `toolName`).  Every tool has
    /// one from construction, so a read never has to invent a value.
    ToolStyle &toolStyle(const QString &tool);
    const ToolStyle &toolStyle(const QString &tool) const;
    // The badge style the next numbered mark is placed with, and any number
    // already selected.
    void setNumberStyle(NumberStyle style);
    NumberStyle numberStyle() const { return numberStyle_; }
    // Pastes an image into the selection: it lands centred at its natural size,
    // shrunk to fit if it is larger than the canvas, and is left selected so
    // the handles can resize it. `source` names the file it came from, empty
    // for clipboard pixels, and is only used to report where it came from.
    // Returns false when there is nothing to paste onto or the image is empty.
    bool pasteImage(const QImage &image, const QString &source = QString());
    // The same, reading whatever the clipboard holds: image data, or a local
    // file path or URI list that points at an image. `error` is filled with
    // why nothing was pasted, for the caller to report.
    bool pasteFromClipboard(QString *error);
    // Pastes an image chosen from disk. The dialog runs in a process of its
    // own -- this one draws a layer surface, which cannot parent a popup -- so
    // the file arrives back here asynchronously and the paste happens then.
    // `error` is filled when the dialog cannot even be started.
    bool pasteFromFile(QString *error);
    // Whether a paste would have anything to work with, so the toolbar can
    // disable its button rather than offering a no-op.
    bool canPaste() const;
    /// Runs recognition over the selection and enters the text mode.  Returns
    /// false and fills `error` when there is nothing to select.
    bool beginTextSelection(QString *error);
    /// The recognition came back: parse it and either enter the text mode or,
    /// when the engine reported no positions, hand the whole text over the way
    /// this used to.  Separate from `beginTextSelection` so a check can drive
    /// the mode without a recognition run.
    bool enterTextSelection(const QByteArray &document, QString *error);
    void leaveTextMode();
    bool textMode() const { return textMode_; }
    /// Runs OCR then translation over the selection and adds the result as one
    /// annotation, in place.  Returns false and fills `error` when there is
    /// nothing to read; reports its progress through
    /// `setTranslateResultCallback`, the way the text button does.
    bool translateSelection(QString *error);
    /// Whether the session is the standalone `translate` overlay: a region-only
    /// frame, the translation drawn over the frozen scene as soon as that frame
    /// is finished, and an Enter that accepts, writing the composited PNG.
    bool translateMode() const { return translateMode_; }
    /// The text the last translation produced, empty before one has run.
    QString translatedText() const { return translatedText_; }
    /// The text the current range would copy, empty when nothing is selected.
    QString selectedText() const;
    /// Told what the text button should show.  Called with `Busy` before the
    /// recognition run starts -- the wait for the engine is long enough that
    /// the button has to say so -- and once more with the outcome.
    void setTextResultCallback(std::function<void(TextOutcome, const QString &)> callback);
    /// The same for the translate button: `Busy` while the two subprocesses
    /// run, then the outcome.
    void setTranslateResultCallback(std::function<void(TextOutcome, const QString &)> callback);
    /// Replaces the clipboard write the text paths use.  It exists so a check
    /// can verify what would be copied without a clipboard; the default writes
    /// through `wl-copy`.
    void setClipboardWriter(std::function<bool(const QString &)> writer);
    void notifyPanelDragged();
    void undo();
    void redo();
    void confirm();
    void cancel();
    // Finishes the session asking for the image to be pinned on the screen
    // instead of saved.  The image is composed on the CLI side, so the request
    // travels back with the result; see `resultDocument`.
    void pin();

    bool isFinished() const;
    bool isCancelled() const;
    // Whether the user finished with the Pin button rather than OK.
    bool isPinResult() const { return pinResult_; }
    bool isPinEdit() const { return pinEdit_; }
    // Pin-edit mode: the whole session bounds is the editable canvas; the
    // selection is fixed and the toolbar shows immediately. Call before the
    // overlay is shown.
    void setPinEditMode(bool enabled) { pinEdit_ = enabled; }
    // Pin-edit mode: the editor drives the real pin window over the daemon
    // socket rather than drawing a second copy of the image. Call before the
    // overlay is shown.
    void setPinTarget(std::uint64_t pinId, const QString &socketPath);
    // Enters editing state over the fixed canvas (shows the toolbar).
    void beginPinEdit();
    // The same editor, opened on the text rather than on the marks: it does
    // what `beginPinEdit` does and then runs recognition over the whole pin,
    // entering the text-selection mode where the recognition succeeds. A
    // failure leaves the editor in the ordinary pin-editing state, reporting
    // through the same callback the toolbar's `Text+` button uses.
    void beginPinEditText();
    // Region sessions that arrive with a selection (window picking resolved
    // one) start in editing state with the toolbar up.  Call after the overlay
    // is shown; sessions without a selection are left alone.
    void beginPresetEdit();
    // Window-pick only: let the picker ask the CLI for a fresh candidate list
    // over the session pipes.  Picking runs on a live desktop, so the list it
    // started with goes stale as soon as the user switches workspace or a
    // window moves; the pointer asks again as it travels.  Call after the
    // overlay is shown, before the event loop runs.
    void enableCandidateRefresh();
    // Asks for that fresh list, at most every `kCandidateRefreshIntervalMs` and
    // never with a request already in flight.  A no-op unless the refresh was
    // enabled; the CLI may answer with nothing, which keeps the current list.
    void requestCandidateRefresh();
    bool hasValidSelection() const;
    // Whether the scrolling-capture action would do anything: the session
    // offers it, the selection is big enough, and it sits inside a single
    // output -- a scroll container never spans two monitors. The toolbar asks
    // this to decide whether to offer the button, and the action asks again
    // before it commits.
    bool canRequestLongCapture() const;
    // Takes the scrolling-capture action: the session ends the way a
    // confirmation does, but the CLI reads the answer as "scroll this region
    // and stitch it" rather than "keep this frame". A no-op when
    // `canRequestLongCapture` is false.
    void requestLongCapture();
    bool longRequested() const { return longRequested_; }
    const std::optional<LogicalRect> &selection() const;
    const QVector<Annotation> &annotations() const;
    // How many freehand segments the live preview has baked since the stroke
    // started.  Lets the offline check prove each segment is drawn once rather
    // than recomputed on every paint.
    int liveStrokeBakes() const;
    QJsonDocument resultDocument(const QString &bitmapDirectory = QString(), QString *error = nullptr) const;

    void setTerminalCallback(std::function<void()> callback);

private:
    class FloatingToolbar;
    class InlineTextEdit;
    struct Gesture;

    Session session_;
    QVector<CaptureOverlay *> overlays_;
    FloatingToolbar *toolbar_ = nullptr;
    InlineTextEdit *textEdit_ = nullptr;
    std::optional<LogicalRect> selection_;
    QVector<Annotation> annotations_;
    // Whether the session offered the scrolling-capture action, and whether
    // the user took it.  The action ends the session like a confirmation, so
    // the answer travels back beside the selection.
    bool longAllowed_ = false;
    bool longRequested_ = false;
    // Window picking: the session's candidate windows are what the pointer may
    // snap to, so the first click replaces the free-hand drag that region
    // capture starts with.
    bool pickMode_ = false;
    /// The text-selection mode: the recognized characters of the selection are
    /// drawn where they were and the pointer selects a range of them.  It is a
    /// mode rather than a tool because the selection it works on is the one the
    /// capture already has, and because Escape has to leave it before it means
    /// "cancel the capture" -- the same shape `pickMode_` has.
    bool textMode_ = false;
    std::optional<TextLayer> textLayer_;
    int textAnchor_ = -1;
    int textFocus_ = -1;
    bool textDragging_ = false;
    /// When the last text-mode double click landed, so a second one in quick
    /// succession -- Qt's third press, reported as another double click -- can
    /// widen the word it took to the whole line.
    QElapsedTimer textClickClock_;
    /// Told when the recognition starts, when the text mode starts, and when a
    /// copy finishes, so the toolbar can say so on the button the user pressed.
    /// The copy can be triggered by a key, which the controller sees and the
    /// toolbar does not.
    std::function<void(TextOutcome outcome, const QString &error)> textResultCallback_;
    /// Writes the text the mode copies.  The default is `wl-copy`; a check
    /// replaces it so the copy can be verified without a clipboard.
    std::function<bool(const QString &)> clipboardWriter_;
    /// The standalone `translate` overlay: a region-only frame, a translation
    /// drawn over the frozen scene, then an accept that writes the PNG.  It is
    /// a mode of its own rather than the editor because it never shows a
    /// toolbar and its Enter key means two different things in turn.
    bool translateMode_ = false;
    /// Whether a translation is up over the framing, and what it holds.
    bool translated_ = false;
    QVector<TranslatedLine> translatedLines_;
    QString translatedText_;
    /// Where the accepted translation was written, for the result document.
    QString resultImagePath_;
    /// The absolute path the session named for it, from `result_path`.
    QString resultPath_;
    /// Told when a translation starts and how it ended, so the button the user
    /// pressed can say so.
    std::function<void(TextOutcome outcome, const QString &error)> translateResultCallback_;
    QVector<WindowCandidate> candidates_;
    int hoveredCandidate_ = -1;
    // Live candidate refresh: the picker's stdin carries fresh lists from the
    // CLI, and one request may be in flight at a time.
    QSocketNotifier *candidateReader_ = nullptr;
    QTimer *candidateTimer_ = nullptr;
    QByteArray candidateReplies_;
    QElapsedTimer candidateClock_;
    bool candidateRefreshEnabled_ = false;
    bool candidateRefreshPending_ = false;
    QVector<QVector<Annotation>> undoStack_;
    QVector<QVector<Annotation>> redoStack_;
    std::optional<Annotation> cancelledText_;
    int editingTextIndex_ = -1;
    bool textEditSnapshot_ = false;
    int toolbarOutput_ = -1;
    int textOutput_ = -1;
    std::uint32_t textDeviceRatio_ = 1;
    Point textOrigin_;
    QString textEditFont_;
    // Font height the open inline editor is drawing at, kept in step with the
    // size box so changing the size while a label is being typed resizes it
    // live instead of leaving the editor at the old height.
    std::uint32_t textEditPixels_ = 0;
    Point pointer_;
    int pointerOutput_ = -1;
    Tool tool_ = Tool::Select;
    QString currentFont_;
    // Per-tool colour and numeric parameters, keyed by `toolName`.  A painter
    // path reads the tool it is drawing with; the style row reads and writes
    // the selected annotation's tool, or the armed one.
    QHash<QString, ToolStyle> toolStyles_;
    // Font height for the next label, in logical pixels -- the same number the
    // size box shows.
    std::uint32_t textSize_ = 14;
    QString currentDash_ = QStringLiteral("solid");
    // How the next pen path is painted: "stroke" | "fill" | "both".  Like the
    // dash and the arrow head, this is a choice rather than a number, so it
    // stays shared instead of travelling per tool.
    QString currentFill_ = QStringLiteral("both");
    std::uint32_t arrowSize_ = 1;
    QString currentArrowStyle_ = QStringLiteral("open");
    QString mosaicShape_ = QStringLiteral("rect");
    std::uint32_t mosaicStrength_ = 2;
    NumberStyle numberStyle_ = NumberStyle::FilledCircle;
    bool panelPinned_ = false;
    // Automatic toolbar placement anchor: while the selection stays put, the
    // side of the selection the command bar was placed on stays fixed, so a
    // style-row toggle only grows the panel the other way.
    QRect toolbarAnchorSelection_;
    QPoint toolbarAnchor_;
    bool toolbarAnchorBelow_ = false;
    bool toolbarAnchorValid_ = false;
    // Selected annotation adjustment (move/resize under the Select tool).
    int selectedAnnotation_ = -1;
    /// `editor.selectMode == "loose"`: a press that is not on a handle or
    /// another mark is held back until it moves, and then moves the selected
    /// mark from wherever it started.  The point is the one the button went
    /// down at, which is the anchor the move is measured from.
    bool looseSelect_ = false;
    std::optional<Point> looseDrag_;
    Annotation dragAnnotation_;
    QVector<Annotation> dragSnapshot_;
    bool dragMoved_ = false;
    bool styleAdjustmentActive_ = false;
    bool styleAdjustmentChanged_ = false;
    QVector<Annotation> styleAdjustmentSnapshot_;
    Gesture *gesture_ = nullptr;
    // Freehand segments the live preview has baked for the current stroke.
    int liveStrokeBakes_ = 0;
    // The session-space rect the last interactive step invalidated, and whether
    // there is one.  A step has to erase what the step before painted, and the
    // only record of that is this rect; `updateAll` clears it, a full repaint
    // being its own eraser.  See `updateTouch`.
    LogicalRect lastTouch_{};
    bool hasLastTouch_ = false;
    // The overlay-local rect that step handed to the widget, for the offline
    // checks; see `lastInteractiveUpdate`.
    QRect lastTouchLocal_;
    bool editing_ = false;
    bool pinEdit_ = false;
    // Set by the Pin button and reported in the result document.
    bool pinResult_ = false;
    /// `region-only`: a finished drag ends the session with the rectangle
    /// instead of opening the editor.  Scrolling capture asks for this, since
    /// the pixels it will annotate do not exist until the stitch is done.
    bool selectOnly_ = false;
    bool finished_ = false;
    bool cancelled_ = false;
    std::function<void()> terminalCallback_;
    mutable int textBitmapIndex_ = 0;
    // The same counter for pasted images' pixel files, in the same directory.
    mutable int imageBitmapIndex_ = 0;
    // Region editor base layer: the session image with the dim veil already
    // composited, at device resolution.  It only depends on the output, its
    // pixel buffer, the surface and the display ratio, so a repaint (a pointer
    // move, a selection drag) blits it rather than drawing the frame and the
    // veil again.  `baseCompositeKey_` says when it has to be rebuilt.
    QImage baseComposite_;
    QByteArray baseCompositeKey_;
    // Live pin window the editor drives in pin-edit mode.  One connection
    // serves the whole drag: opening a socket costs a connect, a server accept
    // and a fresh object on both sides, and paying that per motion event was
    // most of the drag's latency.  The protocol is newline-delimited, so a move
    // is just a line on the connection the first one opened.
    std::uint64_t pinId_ = 0;
    QString pinSocketPath_;
    QLocalSocket *pinSocket_ = nullptr;
    QByteArray pinReplyBuffer_;
    // Positions waiting for a free slot and the number already written but not
    // yet answered.  A few may be in flight at once -- the position is absolute
    // and the newest wins, so a small queue only keeps the daemon busy instead
    // of letting it idle between replies -- but bounded, so a stalled daemon
    // cannot grow it without end.
    std::optional<Point> pendingPinOrigin_;
    int pinMovesInFlight_ = 0;
    // Set from VSHOT_PIN_DEBUG: traces the drag's round trip to stderr.
    bool pinDebug_ = false;
    QElapsedTimer pinMoveClock_;
    // The position the marks are anchored to in pin-edit mode: the last
    // confirmed reply from the daemon, not the optimistic cursor position.  The
    // FP16 helper surface shows the image at this same position, so clipping
    // marks to it keeps them in sync with the image rather than ahead of it.
    std::optional<LogicalRect> marksOrigin_;

    Point globalPoint(CaptureOverlay *overlay, const QPointF &local) const;
    Point unclampedGlobalPoint(CaptureOverlay *overlay, const QPointF &local) const;
    // Index of the output whose geometry holds the middle of `rect`, for
    // placing the toolbar next to a selection nobody dragged.
    int outputContaining(const LogicalRect &rect) const;
    const LogicalRect &annotationLimits() const;
    LogicalRect selectionLimits() const;
    void translateAnnotations(std::int32_t dx, std::int32_t dy);
    void applySelectionMove(LogicalRect origin, Point anchor, Point current);
    void requestPinMove(Point globalTopLeft);
    void flushPinMove();
    void openPinSocket();
    void dropPinSocket();
    void readPinReplies();
    void applyPinReply(QByteArray line);
    void applyPinRect(const LogicalRect &rect);
    // Repaints exactly the overlays' part of `region` (global logical pixels),
    // without touching the "last touch" bookkeeping a gesture's steps share.
    void invalidateLogicalRegion(const LogicalRect &region);
    Point clampPoint(Point point) const;
    int candidateIndexAt(Point point) const;
    QString candidatePillText() const;
    bool applyCandidateHover(Point point, CaptureOverlay *overlay);
    // Replaces the candidate list with a fresh one and points the hover at
    // whatever the (unmoved) pointer is over now.
    void applyCandidates(QVector<WindowCandidate> candidates);
    void readCandidateReplies();
    LogicalRect selectionBetween(Point first, Point second) const;
    LogicalRect moveSelection(LogicalRect origin, Point anchor, Point current) const;
    LogicalRect resizeSelection(LogicalRect origin, int handle, Point current) const;
    int hitHandle(Point point) const;
    // What a press on the Select tool does when it is not aimed at an
    // annotation: resize the selection by its handle, move it from inside, or
    // start a new one.  Shared with the loose drag's click path, which has to
    // reach the same selection logic once it has let go of the mark.
    void beginSelectionGesture(Point point);
    void startSelection(Point point);
    void updateSelection(Point point);
    void finishSelection(Point point);
    void beginDrawing(Point point);
    void updateDrawing(Point point);
    void finishDrawing(Point point);
    // The pen path: a press adds an anchor, the drag after it bends the segment
    // arriving at that anchor, and the path is only finished by closing it or
    // double-clicking.  It therefore outlives the release that ends a normal
    // drag, which is why it has its own three steps rather than reusing
    // `beginDrawing`/`finishDrawing`.
    void beginBezier(Point point);
    // Extends the path in progress: `dragging` pulls the last anchor's outgoing
    // handle to `point`, otherwise `point` is only where the rubber band reaches.
    void updateBezier(Point point, bool dragging);
    // Commits the path in progress, closed or open.
    void finishBezier(bool closed);
    void beginText(CaptureOverlay *overlay, Point point);
    // Places one numbered badge at `point` and advances the count.  The whole
    // tool is a press: there is no drag to preview.
    void placeNumber(Point point);
    void startTextEditor(CaptureOverlay *overlay, int index, Point origin);
    void finishText(bool accept);
    int annotationHitAt(Point point) const;
    int annotationHandleAt(Point point) const;
    void beginAnnotationDrag(Point point, bool resize);
    void updateAnnotationDrag(Point point);
    void finishAnnotationDrag(CaptureOverlay *overlay, Point point);
    Annotation translatedAnnotation(const Annotation &original, int dx, int dy) const;
    Annotation scaledAnnotation(const Annotation &original, const LogicalRect &bounds) const;
    void selectAnnotation(int index);
    void deleteSelectedAnnotation();
    void applyStyleToSelected(const std::function<void(Annotation &)> &mutate);
    void beginStyleAdjustment();
    void endStyleAdjustment();
    QString styleTargetTool() const;
    int sceneScale() const;
    void showToolbar();
    void hideToolbar();
    void updateToolbarGeometry();
    int outputIndexForSelection() const;
    void settlePanelAtGlobal(QPoint topLeft);
    void repaintEverything();
    void updateAll();
    // Invalidates only the part of the surface an interactive step changed.  A
    // repaint of a 4K overlay costs about 1.7 ms per full-frame blit and the
    // marks sit on top of two of them, so a step that touches a few hundred
    // pixels must not ask for the whole surface; `touched` is in session
    // coordinates and every overlay gets its own share of it.  The region the
    // step before invalidated is included as well -- that is what erases where
    // a dragged mark used to be -- and an empty rect falls back to `updateAll`.
    void updateTouch(const LogicalRect &touched);
    // The rects one interactive step can have changed, in session coordinates.
    LogicalRect selectionTouch() const;
    LogicalRect annotationTouch() const;
    LogicalRect drawingTouch(int pointsBefore) const;
    // The rects a step of the pen path can have changed, in session coordinates:
    // the path so far plus the rubber band from its last anchor to the pointer.
    LogicalRect bezierTouch() const;
    // The mark the pen path in progress would commit, as it stands.  The
    // preview and its bounds both read the shape from here.
    Annotation previewAnnotation() const;
    // The magnifier the editor draws around the pointer while a gesture drags
    // something, in session coordinates.
    LogicalRect pointerTouch() const;
    // True for the tools whose preview builds up through the incremental raster
    // rather than being redrawn whole from the anchor every step.
    bool drawsGrowingStroke() const;
    void terminal(bool cancelled);
    void removeTextEditor();
    // The text-selection mode's own steps: turning two indices into the range
    // the pointer described, widening one to a word or a line, copying what the
    // range holds, and the pointer shape the mode shows while it is idle.
    void copyTextSelection();
    void textSelectAll();
    void textSelectWord(int index);
    void textSelectLine(int index);
    void selectTextRange(int anchor, int focus);
    void updateTextModeCursor();
    // The one place the mode's text reaches the clipboard: the injected writer
    // when there is one, `wl-copy` otherwise.
    bool writeClipboard(const QString &text);
    // The translation path, shared by the editor's button and the standalone
    // overlay: read the selection, run it through the CLI's two steps, and
    // place the result.  `computeTranslation` does the subprocesses and the
    // parse; the two callers differ only in what they keep afterwards.
    bool computeTranslation(QVector<TranslatedLine> *lines, QString *text, QString *error);
    bool runTranslationPipeline(const QImage &pixels, QByteArray *document,
                                QString *error) const;
    bool runTranslateStage(QString *error);
    bool acceptTranslation(QString *error);
    void mutateAnnotations(QVector<Annotation> next);
    void drawLoupe(CaptureOverlay *overlay, QPainter *painter);
    // Draws the in-progress freehand stroke from a raster that only grows by the
    // points appended since the last paint.
    void paintLiveStroke(QPainter *painter, const OutputSession &output, const QSize &size,
                         int outputIndex);
    bool annotationBounds(const Annotation &annotation, LogicalRect *bounds) const;
    bool canDrawAt(Point point) const;
};

class CaptureOverlay final : public QWidget {
public:
    CaptureOverlay(int outputIndex, OverlayController *controller, QScreen *screen);
    ~CaptureOverlay() override;

    int outputIndex() const;
    const OutputSession &output() const;
    QPointF localFromGlobal(Point point) const;
    bool showLayerSurface();
    // Floating layer surface carved to a specific global logical rect
    // (top-left anchored + margins): used by the pin editor.
    bool showLayerSurfaceAt(int globalX, int globalY, int width, int height);

protected:
    void paintEvent(QPaintEvent *event) override;
    void mousePressEvent(QMouseEvent *event) override;
    void mouseMoveEvent(QMouseEvent *event) override;
    void mouseReleaseEvent(QMouseEvent *event) override;
    void mouseDoubleClickEvent(QMouseEvent *event) override;
    void keyPressEvent(QKeyEvent *event) override;
    void closeEvent(QCloseEvent *event) override;
    void leaveEvent(QEvent *event) override;

private:
    int outputIndex_;
    OverlayController *controller_;
    QScreen *screen_;
    QWindow *layerWindow_ = nullptr;
};

} // namespace vshot
