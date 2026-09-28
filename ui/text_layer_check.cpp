// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

// Offline check for the text layer: the recognized text of a capture and the
// selection rules that turn a drag into a range of it.
//
// This is the whole of "select the text where it was" that is not painting:
// reading `vshot ocr --json`, mapping the engine's device pixels onto the
// overlay's logical ones, deciding which character the pointer is on, and
// turning two indices into the string a copy hands over.  None of it needs a
// compositor, an overlay or a recognition run, which is the point -- the
// interesting rules are the ones that are hard to reach through a real capture
// (a drag that leaves the text, a range that crosses a line, a line whose
// characters the engine could not separate) and they are all reachable here.
//
// Built only with `-DVSHOT_BUILD_CHECKS=ON`. There is no widget and no screen
// in it, so unlike the other checks it needs neither the offscreen platform
// plugin nor `QT_QPA_PLATFORM`; Qt Gui is on the link line only for the
// rectangle type it shares with the session reader.

#include "text_layer.hpp"

#include <QByteArray>
#include <QCoreApplication>
#include <QFile>
#include <QString>

#include <cstdint>
#include <cstdio>

namespace {

int failures = 0;

void expect(bool ok, const char *what, const QString &detail = QString())
{
    if (ok) {
        std::printf("ok    %s\n", what);
        return;
    }
    if (detail.isEmpty()) {
        std::printf("FAIL  %s\n", what);
    } else {
        std::printf("FAIL  %s -- %s\n", what, qPrintable(detail));
    }
    ++failures;
}

QString rectText(const vshot::LogicalRect &rect)
{
    return QStringLiteral("(%1,%2 %3x%4)")
        .arg(rect.x)
        .arg(rect.y)
        .arg(rect.width)
        .arg(rect.height);
}

bool sameRect(const vshot::LogicalRect &rect, std::int32_t x, std::int32_t y, std::uint32_t width,
              std::uint32_t height)
{
    return rect.x == x && rect.y == y && rect.width == width && rect.height == height;
}

// Two lines, two words, one space.  The engine's coordinates are device pixels
// counted from the crop it was handed; the placement below doubles them, so
// every expectation that follows is written in the overlay's own logical
// pixels and a mapping mistake cannot hide behind a scale of one.
const char *const kTwoLines = R"json(
{"version":1,"geometry":true,"lines":[
 {"text":"AB C","rect":{"x":10,"y":20,"width":90,"height":30},
  "chars":[{"ch":"A","rect":{"x":10,"y":20,"width":20,"height":30}},
           {"ch":"B","rect":{"x":30,"y":20,"width":20,"height":30}},
           {"ch":" ","rect":{"x":50,"y":20,"width":10,"height":30}},
           {"ch":"C","rect":{"x":60,"y":20,"width":40,"height":30}}]},
 {"text":"DE","rect":{"x":10,"y":60,"width":50,"height":30},
  "chars":[{"ch":"D","rect":{"x":10,"y":60,"width":20,"height":30}},
           {"ch":"E","rect":{"x":30,"y":60,"width":30,"height":30}}]}]}
)json";

const vshot::TextLayerPlacement kDoubled{100.0, 200.0, 2.0};

// A line whose last character is a space, followed by another line: the space
// is where the engine stopped reading, not part of the text.
const char *const kTrailingSpace = R"json(
{"version":1,"geometry":true,"lines":[
 {"text":"AB ","rect":{"x":0,"y":0,"width":50,"height":10},
  "chars":[{"ch":"A","rect":{"x":0,"y":0,"width":20,"height":10}},
           {"ch":"B","rect":{"x":20,"y":0,"width":20,"height":10}},
           {"ch":" ","rect":{"x":40,"y":0,"width":10,"height":10}}]},
 {"text":"CD","rect":{"x":0,"y":20,"width":40,"height":10},
  "chars":[{"ch":"C","rect":{"x":0,"y":20,"width":20,"height":10}},
           {"ch":"D","rect":{"x":20,"y":20,"width":20,"height":10}}]}]}
)json";

// An external engine: text and no positions at all.
const char *const kNoGeometry = R"json(
{"version":1,"geometry":false,"lines":[{"text":"alpha"},{"text":"beta"}]}
)json";

// A line the engine read but could not place a character in.
const char *const kUnseparatedLine = R"json(
{"version":1,"geometry":true,"lines":[
 {"text":"whole","rect":{"x":5,"y":6,"width":7,"height":8},"chars":[]}]}
)json";

const char *const kIdeographs = R"json(
{"version":1,"geometry":true,"lines":[
 {"text":"中文测试","rect":{"x":0,"y":0,"width":80,"height":20},
  "chars":[{"ch":"中","rect":{"x":0,"y":0,"width":20,"height":20}},
           {"ch":"文","rect":{"x":20,"y":0,"width":20,"height":20}},
           {"ch":"测","rect":{"x":40,"y":0,"width":20,"height":20}},
           {"ch":"试","rect":{"x":60,"y":0,"width":20,"height":20}}]}]}
)json";

const char *const kWords = R"json(
{"version":1,"geometry":true,"lines":[
 {"text":"hi there","rect":{"x":0,"y":0,"width":160,"height":20},
  "chars":[{"ch":"h","rect":{"x":0,"y":0,"width":20,"height":20}},
           {"ch":"i","rect":{"x":20,"y":0,"width":20,"height":20}},
           {"ch":" ","rect":{"x":40,"y":0,"width":20,"height":20}},
           {"ch":"t","rect":{"x":60,"y":0,"width":20,"height":20}},
           {"ch":"h","rect":{"x":80,"y":0,"width":20,"height":20}},
           {"ch":"e","rect":{"x":100,"y":0,"width":20,"height":20}},
           {"ch":"r","rect":{"x":120,"y":0,"width":20,"height":20}},
           {"ch":"e","rect":{"x":140,"y":0,"width":20,"height":20}}]}]}
)json";

std::optional<vshot::TextLayer> parse(const char *document, const vshot::TextLayerPlacement &place,
                                      const char *what)
{
    QString error;
    std::optional<vshot::TextLayer> layer =
        vshot::TextLayer::fromJson(QByteArray(document), place, &error);
    if (!layer.has_value()) {
        expect(false, what, error);
    }
    return layer;
}

void checkPlacement()
{
    std::printf("--- the engine's pixels become the overlay's ----------------------\n");
    std::optional<vshot::TextLayer> layer = parse(kTwoLines, kDoubled, "the two-line document parses");
    if (!layer.has_value()) {
        return;
    }
    expect(layer->hasGeometry(), "a document with positions has geometry");
    expect(layer->count() == 6, "one unit per character", QString::number(layer->count()));
    expect(layer->lineCount() == 2, "two lines", QString::number(layer->lineCount()));
    expect(layer->plainText() == QStringLiteral("AB C\nDE"), "the plain text is the lines joined",
           layer->plainText());

    // Engine (10,20,20,30) at origin (100,200) and scale 2 is logical
    // (105,210,10,15): each edge is halved and only then placed, so the two
    // boxes that share an edge still share it afterwards.
    expect(sameRect(layer->unit(0).rect, 105, 210, 10, 15), "the first box is placed",
           rectText(layer->unit(0).rect));
    expect(sameRect(layer->unit(2).rect, 125, 210, 5, 15), "so is the space",
           rectText(layer->unit(2).rect));
    expect(sameRect(layer->unit(5).rect, 115, 230, 15, 15), "and one on the second line",
           rectText(layer->unit(5).rect));
    expect(layer->unit(3).rect.x == layer->unit(2).rect.x + static_cast<std::int32_t>(layer->unit(2).rect.width),
           "adjacent boxes still meet", rectText(layer->unit(3).rect));

    // A placement of scale one and no origin is the engine's own coordinates,
    // which is what a capture at 1x with no crop offset produces.
    std::optional<vshot::TextLayer> raw =
        parse(kTwoLines, vshot::TextLayerPlacement{}, "a default placement parses");
    if (raw.has_value()) {
        expect(sameRect(raw->unit(0).rect, 10, 20, 20, 30),
               "a default placement leaves the coordinates alone", rectText(raw->unit(0).rect));
    }
}

void checkHitTesting()
{
    std::printf("--- which character the pointer is on -----------------------------\n");
    std::optional<vshot::TextLayer> layer = parse(kTwoLines, kDoubled, "the two-line document parses");
    if (!layer.has_value()) {
        return;
    }
    expect(layer->indexAt(108, 215) == 0, "a point in the first box finds it",
           QString::number(layer->indexAt(108, 215)));
    expect(layer->indexAt(126, 215) == 2, "a point in the space finds the space",
           QString::number(layer->indexAt(126, 215)));
    expect(layer->indexAt(120, 240) == 5, "a point on the second line finds its own character",
           QString::number(layer->indexAt(120, 240)));
    expect(layer->indexAt(104, 215) == -1, "a point just left of the text finds nothing",
           QString::number(layer->indexAt(104, 215)));
    // The two lines do not touch: 210+15 is the bottom of the first and 230 the
    // top of the second, and the gap between them belongs to neither.
    expect(layer->indexAt(108, 228) == -1, "the gap between two lines finds nothing",
           QString::number(layer->indexAt(108, 228)));

    // A drag leaves the text, and still has to mean something.
    expect(layer->nearestIndex(1000, 215) == 3, "right of the first line stops at its last character",
           QString::number(layer->nearestIndex(1000, 215)));
    expect(layer->nearestIndex(-500, 240) == 4, "left of the second line stops at its first",
           QString::number(layer->nearestIndex(-500, 240)));
    // Below everything, the last line is the nearest one, and the character is
    // chosen on that line rather than on any other.
    expect(layer->nearestIndex(120, 5000) == 5, "far below lands on the last line",
           QString::number(layer->nearestIndex(120, 5000)));
    expect(layer->nearestIndex(108, 228) == 4, "a point in the gap lands on the nearer line",
           QString::number(layer->nearestIndex(108, 228)));

    // An empty layer has nothing to point at, and must not answer as if it did.
    vshot::TextLayer empty;
    expect(empty.nearestIndex(0, 0) == -1, "an empty layer has no nearest unit");
    expect(empty.indexAt(0, 0) == -1, "an empty layer has no hit");
}

void checkRanges()
{
    std::printf("--- two indices become a string ----------------------------------\n");
    std::optional<vshot::TextLayer> layer = parse(kTwoLines, kDoubled, "the two-line document parses");
    if (!layer.has_value()) {
        return;
    }
    expect(layer->rangeText(0, 3) == QStringLiteral("AB C"), "a range inside one line",
           layer->rangeText(0, 3));
    expect(layer->rangeText(3, 0) == QStringLiteral("AB C"), "a backwards drag is the same range",
           layer->rangeText(3, 0));
    expect(layer->rangeText(0, 5) == QStringLiteral("AB C\nDE"), "a range across lines gets a newline",
           layer->rangeText(0, 5));
    expect(layer->rangeText(1, 4) == QStringLiteral("B C\nD"), "a range starting mid-line",
           layer->rangeText(1, 4));
    expect(layer->rangeText(2, 2) == QStringLiteral(" "), "one unit is one string",
           QStringLiteral("[%1]").arg(layer->rangeText(2, 2)));
    // Out of range is clamped rather than refused: a drag that overshoots the
    // last character means "to the end", which is what the pointer said.
    expect(layer->rangeText(0, 999) == QStringLiteral("AB C\nDE"), "an overshooting range clamps",
           layer->rangeText(0, 999));

    std::optional<vshot::TextLayer> trailing =
        parse(kTrailingSpace, vshot::TextLayerPlacement{}, "the trailing-space document parses");
    if (trailing.has_value()) {
        expect(trailing->rangeText(0, 4) == QStringLiteral("AB\nCD"),
               "the space a line ended on is not carried into the next",
               QStringLiteral("[%1]").arg(trailing->rangeText(0, 4)));
        expect(trailing->rangeText(0, 2) == QStringLiteral("AB "),
               "but it is still there when the range stops on it",
               QStringLiteral("[%1]").arg(trailing->rangeText(0, 2)));
    }
}

void checkWordAndLine()
{
    std::printf("--- a double click is a word, a triple click a line --------------\n");
    std::optional<vshot::TextLayer> words =
        parse(kWords, vshot::TextLayerPlacement{}, "the word document parses");
    if (words.has_value()) {
        int first = -1;
        int last = -1;
        words->wordRange(0, &first, &last);
        expect(first == 0 && last == 1, "a double click takes the whole word",
               QStringLiteral("%1..%2").arg(first).arg(last));
        words->wordRange(5, &first, &last);
        expect(first == 3 && last == 7, "and takes it from either end",
               QStringLiteral("%1..%2").arg(first).arg(last));
        words->wordRange(2, &first, &last);
        expect(first == 2 && last == 2, "on the space it takes the space",
               QStringLiteral("%1..%2").arg(first).arg(last));
    }

    std::optional<vshot::TextLayer> ideographs =
        parse(kIdeographs, vshot::TextLayerPlacement{}, "the Chinese document parses");
    if (ideographs.has_value()) {
        int first = -1;
        int last = -1;
        ideographs->wordRange(1, &first, &last);
        expect(first == 1 && last == 1,
               "a double click in Chinese takes one character, not the sentence",
               QStringLiteral("%1..%2").arg(first).arg(last));
    }

    std::optional<vshot::TextLayer> layer = parse(kTwoLines, kDoubled, "the two-line document parses");
    if (layer.has_value()) {
        int first = -1;
        int last = -1;
        layer->wordRange(0, &first, &last);
        expect(first == 0 && last == 1, "a word does not run past the space",
               QStringLiteral("%1..%2").arg(first).arg(last));
        layer->lineRange(0, &first, &last);
        expect(first == 0 && last == 3, "a line is every unit on it",
               QStringLiteral("%1..%2").arg(first).arg(last));
        layer->lineRange(4, &first, &last);
        expect(first == 4 && last == 5, "and the second line is its own",
               QStringLiteral("%1..%2").arg(first).arg(last));
        // A word never crosses a line break, however the text reads.
        layer->wordRange(3, &first, &last);
        expect(first == 3 && last == 3, "a single-character word is itself",
               QStringLiteral("%1..%2").arg(first).arg(last));
    }

    vshot::TextLayer empty;
    int first = 0;
    int last = 0;
    empty.wordRange(0, &first, &last);
    expect(first == -1 && last == -1, "an empty layer has no word");
    empty.lineRange(0, &first, &last);
    expect(first == -1 && last == -1, "an empty layer has no line");
}

void checkDegenerateDocuments()
{
    std::printf("--- what the engine cannot say ----------------------------------\n");
    QString error;
    std::optional<vshot::TextLayer> none =
        vshot::TextLayer::fromJson(QByteArray(kNoGeometry), vshot::TextLayerPlacement{}, &error);
    if (!none.has_value()) {
        expect(false, "a document without geometry still parses", error);
    } else {
        expect(!none->hasGeometry(), "a document without geometry reports none");
        expect(none->count() == 0, "and has nothing to select",
               QString::number(none->count()));
        // The fallback the caller uses: the text is still there to hand over.
        expect(none->plainText() == QStringLiteral("alpha\nbeta"),
               "but keeps the text the engine did read", none->plainText());
    }

    std::optional<vshot::TextLayer> whole =
        parse(kUnseparatedLine, vshot::TextLayerPlacement{}, "a line without character boxes parses");
    if (whole.has_value()) {
        expect(whole->hasGeometry(), "the line is still placed");
        expect(whole->count() == 1, "as one unit", QString::number(whole->count()));
        expect(whole->unit(0).text == QStringLiteral("whole"), "holding the whole line's text",
               whole->unit(0).text);
        expect(sameRect(whole->unit(0).rect, 5, 6, 7, 8), "over the line's own rect",
               rectText(whole->unit(0).rect));
    }

    // Every way the document can be wrong, and none of them may crash or be
    // quietly accepted: the caller has to be able to tell a rejection from a
    // document that simply has no positions.
    const struct {
        const char *what;
        const char *document;
    } malformed[] = {
        {"text that is not JSON", "not json at all"},
        {"JSON that is not an object", "[1,2,3]"},
        {"a document with no version", R"({"geometry":true,"lines":[]})"},
        {"a document from another version", R"({"version":2,"geometry":true,"lines":[]})"},
        {"a document that does not say whether it has geometry", R"({"version":1,"lines":[]})"},
        {"a document with no lines", R"({"version":1,"geometry":true})"},
        {"a line that is not an object", R"({"version":1,"geometry":true,"lines":[7]})"},
        {"a line with no text", R"({"version":1,"geometry":true,"lines":[{"rect":{"x":0,"y":0,"width":1,"height":1},"chars":[]}]})"},
        {"a line with no rectangle", R"({"version":1,"geometry":true,"lines":[{"text":"a","chars":[]}]})"},
        {"a line with no characters list", R"({"version":1,"geometry":true,"lines":[{"text":"a","rect":{"x":0,"y":0,"width":1,"height":1}}]})"},
        {"a character with no glyph", R"({"version":1,"geometry":true,"lines":[{"text":"a","rect":{"x":0,"y":0,"width":1,"height":1},"chars":[{"rect":{"x":0,"y":0,"width":1,"height":1}}]}]})"},
        {"a character with no rectangle", R"({"version":1,"geometry":true,"lines":[{"text":"a","rect":{"x":0,"y":0,"width":1,"height":1},"chars":[{"ch":"a"}]}]})"},
    };
    for (const auto &entry : malformed) {
        QString failure;
        std::optional<vshot::TextLayer> rejected = vshot::TextLayer::fromJson(
            QByteArray(entry.document), vshot::TextLayerPlacement{}, &failure);
        expect(!rejected.has_value() && !failure.isEmpty(), entry.what, failure);
    }
}

/// Parses a real `vshot ocr --json` capture when one is named on the command
/// line.
///
/// Every document above is written by hand and shaped like the engine's, which
/// means they would all keep passing if the Rust side renamed a field: this is
/// the one part that reads bytes the other language actually produced.  It
/// needs a file and no compositor, so it stays optional --
/// `vshot ocr --input shot.png --json > /tmp/ocr.json` and pass that path.
void checkRealCapture(const QString &path)
{
    std::printf("--- a capture the CLI actually wrote -------------------------------\n");
    QFile file(path);
    if (!file.open(QIODevice::ReadOnly)) {
        expect(false, "the named capture opens", file.errorString());
        return;
    }
    QString error;
    std::optional<vshot::TextLayer> layer =
        vshot::TextLayer::fromJson(file.readAll(), vshot::TextLayerPlacement{}, &error);
    if (!layer.has_value()) {
        expect(false, "the CLI's own output parses", error);
        return;
    }
    expect(layer->hasGeometry(), "a real capture has geometry");
    expect(layer->count() > 0, "and characters to select", QString::number(layer->count()));
    for (int index = 0; index < layer->count(); ++index) {
        if (layer->unit(index).text.isEmpty()) {
            expect(false, "no unit came back empty", QString::number(index));
            return;
        }
    }
    // A field read from the wrong place shows up here as text that is not what
    // the same run prints without `--json`.
    expect(layer->plainText().contains(QLatin1Char('\n')) || layer->lineCount() == 1,
           "the lines came through", QString::number(layer->lineCount()));
    std::printf("      %d unit(s) over %d line(s): %s\n", layer->count(), layer->lineCount(),
                qPrintable(layer->plainText().left(60).replace(QLatin1Char('\n'), QLatin1Char(' '))));
}

} // namespace

int main(int argc, char **argv)
{
    QCoreApplication app(argc, argv);
    checkPlacement();
    checkHitTesting();
    checkRanges();
    checkWordAndLine();
    checkDegenerateDocuments();
    if (argc > 1) {
        checkRealCapture(QString::fromLocal8Bit(argv[1]));
    }

    std::printf("--- result ---------------------------------------------------------\n");
    std::printf("%s (%d failure(s))\n", failures == 0 ? "ALL PASS" : "FAILURES", failures);
    return failures == 0 ? 0 : 1;
}
