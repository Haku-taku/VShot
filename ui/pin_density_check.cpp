// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

// Offline check for the density an image declares about itself.  That
// declaration is read from the container's own bytes -- a PNG's `pHYs`, a
// JPEG's JFIF header or EXIF, a WebP's `EXIF` chunk, a JPEG XL file's `Exif`
// box, an AVIF's marker -- and the point of reading the bytes rather than the
// decoded value is that 96 DPI has to be seen as the 1x it is: it is what
// vshot writes for a capture from a scale-1 output, and a pin has to keep that
// size when it lands on a denser screen.
//
// This is what makes the check worth having: Qt reports 3780 dots per metre
// both for a PNG that declares 96 DPI and for one that declares nothing, so
// the two are only told apart in the bytes. Built only with
// `-DVSHOT_BUILD_CHECKS=ON`; it needs Qt Gui for the image codecs and nothing
// else -- no compositor, no layer shell, no platform plugin.

#include "pin_density.hpp"

#include <QBuffer>
#include <QByteArray>
#include <QDataStream>
#include <QDir>
#include <QFile>
#include <QImage>
#include <QList>
#include <QtEndian>

#include <cstdio>

namespace {

int failures = 0;

void expectDensity(const char *what, int got, int want)
{
    if (got != want) {
        std::printf("FAIL  %-46s -> %d (wanted %d)\n", what, got, want);
        ++failures;
        return;
    }
    std::printf("ok    %-46s -> %d\n", what, got);
}

// PNG's CRC-32, over the chunk type and its data. It matters that this is the
// real thing: the last section hands these bytes to QImage, and libpng refuses
// a chunk whose CRC does not check out.
quint32 crc32(const QByteArray &bytes)
{
    quint32 crc = 0xffffffffu;
    for (const char raw : bytes) {
        crc ^= static_cast<quint8>(raw);
        for (int bit = 0; bit < 8; ++bit) {
            crc = (crc >> 1) ^ (0xedb88320u & (0u - (crc & 1u)));
        }
    }
    return crc ^ 0xffffffffu;
}

// A chunk header on its own: the length and the type, with no data behind it.
QByteArray chunkHeader(const char *type, quint32 length)
{
    QByteArray out;
    QDataStream stream(&out, QIODevice::WriteOnly);
    stream.setByteOrder(QDataStream::BigEndian);
    stream << length;
    out += QByteArray(type, 4);
    return out;
}

// One PNG chunk: length, type, data, CRC.
QByteArray chunk(const char *type, const QByteArray &data)
{
    const QByteArray kind(type, 4);
    QByteArray out;
    QDataStream size(&out, QIODevice::WriteOnly);
    size.setByteOrder(QDataStream::BigEndian);
    size << quint32(data.size());
    out += kind;
    out += data;
    QDataStream crc(&out, QIODevice::Append);
    crc.setByteOrder(QDataStream::BigEndian);
    crc << crc32(kind + data);
    return out;
}

// A syntax-checkable fake: the reader only walks chunk headers, so the pixel
// data of a PNG built here is deliberately meaningless.
QByteArray png(const QList<QByteArray> &chunks, const QByteArray &afterIdat = QByteArray())
{
    QByteArray out("\x89PNG\r\n\x1a\n", 8);
    out += chunk("IHDR", QByteArray(13, '\0'));
    for (const QByteArray &piece : chunks) {
        out += piece;
    }
    out += chunk("IDAT", QByteArray(4, '\0'));
    out += afterIdat;
    out += chunk("IEND", QByteArray());
    return out;
}

QByteArray pixelDensity(int x, int y, int unit)
{
    QByteArray data;
    QDataStream stream(&data, QIODevice::WriteOnly);
    stream.setByteOrder(QDataStream::BigEndian);
    stream << quint32(x) << quint32(y) << quint8(unit);
    return chunk("pHYs", data);
}

// Rewrites a real PNG's chunks, dropping its `pHYs` or replacing it with the
// one given. Used to build the two files below out of PNGs Qt itself wrote.
QByteArray rewritePixelDensity(const QByteArray &source, const QByteArray &replacement)
{
    if (source.size() < 8) {
        return QByteArray();
    }
    QByteArray out = source.left(8);
    qint64 offset = 8;
    while (offset + 8 <= source.size()) {
        const QByteArray header = source.mid(offset, 8);
        const quint32 length = qFromBigEndian<quint32>(header.constData());
        const QByteArray type = header.mid(4, 4);
        const qint64 total = 8 + qint64(length) + 4;
        if (type != QByteArrayLiteral("pHYs")) {
            if (type == QByteArrayLiteral("IDAT") && !replacement.isEmpty()) {
                out += replacement;
            }
            out += source.mid(offset, total);
        }
        offset += total;
        if (type == QByteArrayLiteral("IEND")) {
            break;
        }
    }
    return out;
}

// A value in the order one of the containers stores it in.
QByteArray big(quint32 value, int bytes)
{
    QByteArray out(bytes, '\0');
    for (int index = 0; index < bytes; ++index) {
        out[bytes - 1 - index] = char((value >> (8 * index)) & 0xff);
    }
    return out;
}

QByteArray little(quint32 value, int bytes)
{
    QByteArray out(bytes, '\0');
    for (int index = 0; index < bytes; ++index) {
        out[index] = char((value >> (8 * index)) & 0xff);
    }
    return out;
}

// The twelve bytes every JPEG XL container opens with.
QByteArray jxlSignature()
{
    return QByteArrayLiteral("\x00\x00\x00\x0cJXL \x0d\x0a\x87\x0a");
}

// The marker an AVIF carries the density in, beside the reference white.
QByteArray scaleUuid()
{
    return QByteArrayLiteral("vshot.scale00001");
}

// A bare EXIF blob: one IFD with the three tags that carry a physical
// resolution, and the two rationals they point at. The same 66 bytes the Rust
// side writes, built here from the field layout rather than from that code, so
// a change on either side shows up as a disagreement.
QByteArray exifBlob(int dpi, int unit = 2)
{
    QByteArray out = QByteArrayLiteral("MM") + big(42, 2) + big(8, 4);
    out += big(3, 2);
    out += big(0x011a, 2) + big(5, 2) + big(1, 4) + big(50, 4);
    out += big(0x011b, 2) + big(5, 2) + big(1, 4) + big(58, 4);
    out += big(0x0128, 2) + big(3, 2) + big(1, 4) + big(unit, 2) + big(0, 2);
    out += big(0, 4);
    out += big(dpi, 4) + big(1, 4) + big(dpi, 4) + big(1, 4);
    return out;
}

// A JPEG that opens with a JFIF APP0 segment and then goes straight to the
// scan. The walk never decodes it, so it does not have to be a picture.
QByteArray jpegWithJfif(int x, int y, int unit)
{
    const QByteArray body = QByteArrayLiteral("JFIF\0") + QByteArrayLiteral("\x01\x01")
                            + QByteArray(1, char(unit)) + big(x, 2) + big(y, 2)
                            + QByteArray(2, '\0');
    return QByteArrayLiteral("\xff\xd8") + QByteArrayLiteral("\xff\xe0") + big(body.size() + 2, 2)
           + body + QByteArrayLiteral("\xff\xda") + big(2, 2);
}

// The same, with the density in EXIF instead: what every camera writes, and
// the other place a JPEG states a resolution.
QByteArray jpegWithExif(const QByteArray &blob)
{
    const QByteArray body = QByteArrayLiteral("Exif\0\0") + blob;
    return QByteArrayLiteral("\xff\xd8") + QByteArrayLiteral("\xff\xe1") + big(body.size() + 2, 2)
           + body + QByteArrayLiteral("\xff\xda") + big(2, 2);
}

// A Radiance file with the scale in the header line this program writes.  Only
// the header matters to the reader; the pixels after the blank line are not
// read at all.
QByteArray radianceWith(const QByteArray &scaleLine)
{
    return QByteArrayLiteral("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\nPRIMARIES=0.64 0.33 0.30 0.60 "
                             "0.15 0.06 0.3127 0.3290\nREFERENCE_NITS=203.00\n")
           + scaleLine + QByteArrayLiteral("\n-Y 4 +X 4\n") + QByteArray(48, '\x81');
}

QByteArray webpWith(const QByteArray &chunkTag, const QByteArray &chunkBody, bool exifFlag)
{
    QByteArray header(1, char(exifFlag ? 0x08 : 0x00));
    header += QByteArray(3, '\0');
    header += little(63, 3) + little(63, 3);
    QByteArray chunks = QByteArrayLiteral("VP8X") + little(header.size(), 4) + header;
    chunks += chunkTag + little(chunkBody.size(), 4) + chunkBody;
    if (chunkBody.size() % 2 == 1) {
        chunks += '\0';
    }
    const QByteArray body = QByteArrayLiteral("WEBP") + chunks;
    return QByteArrayLiteral("RIFF") + little(body.size(), 4) + body;
}

QByteArray jxlWith(const QByteArray &tag, const QByteArray &payload)
{
    return jxlSignature() + big(payload.size() + 8, 4) + tag + payload;
}

// A container with the density in the box this program writes, spelled the way
// libjxl spells it: the offset of the TIFF header, then the EXIF as a JPEG's
// APP1 segment carries it.
QByteArray jxlWithExif(const QByteArray &blob)
{
    return jxlWith(QByteArrayLiteral("Exif"),
                   big(6, 4) + QByteArrayLiteral("Exif\0\0") + blob);
}

// An ISO base media file: `ftyp` declaring the AVIF brand, then the box this
// program records the scale in.
QByteArray avifWithScale(quint32 density)
{
    QByteArray out = big(20, 4) + QByteArrayLiteral("ftyp") + QByteArrayLiteral("avif")
                     + big(0, 4) + QByteArrayLiteral("avif");
    const QByteArray box = QByteArrayLiteral("uuid") + scaleUuid() + little(density, 4);
    out += big(box.size() + 8, 4) + box;
    return out;
}

} // namespace

int main(int argc, char **argv)
{
    // Optional: a directory to leave the fixture PNGs in, so a reader can
    // compare what Qt reports for each of them.
    const QString dump = argc > 1 ? QString::fromLocal8Bit(argv[1]) : QString();

    std::printf("--- the chunk, field by field --------------------------------------\n");
    using vshot::densityFromPixelDensity;
    expectDensity("3780 dots per metre, metre unit: 96 DPI", densityFromPixelDensity(3780, 3780, 1), 1);
    expectDensity("7559: 192 DPI", densityFromPixelDensity(7559, 7559, 1), 2);
    expectDensity("11339: 288 DPI", densityFromPixelDensity(11339, 11339, 1), 3);
    expectDensity("15118: 384 DPI", densityFromPixelDensity(15118, 15118, 1), 4);
    expectDensity("a unit of 0 is aspect ratio only", densityFromPixelDensity(7559, 7559, 0), 0);
    expectDensity("non-square is not a device density", densityFromPixelDensity(7559, 3780, 1), 0);
    expectDensity("48 DPI is not a screen density", densityFromPixelDensity(1890, 1890, 1), 0);
    expectDensity("72 DPI is not a screen density", densityFromPixelDensity(2835, 2835, 1), 0);
    expectDensity("300 DPI is a print resolution", densityFromPixelDensity(11811, 11811, 1), 0);
    expectDensity("480 DPI is beyond any screen here", densityFromPixelDensity(18900, 18900, 1), 0);
    expectDensity("zero declares nothing", densityFromPixelDensity(0, 0, 1), 0);

    std::printf("--- walking a PNG, in memory and on disk ---------------------------\n");
    using vshot::imageDeclaredDensity;
    using vshot::imageDeclaredDensityOfFile;
    using vshot::pngDeclaredDensity;
    expectDensity("no pHYs chunk at all", imageDeclaredDensity(png({})), 0);
    expectDensity("pHYs 3780 (a 1x capture)", imageDeclaredDensity(png({pixelDensity(3780, 3780, 1)})), 1);
    expectDensity("pHYs 7559 (a 2x capture)", imageDeclaredDensity(png({pixelDensity(7559, 7559, 1)})), 2);
    expectDensity("pHYs declaring no unit", imageDeclaredDensity(png({pixelDensity(3780, 3780, 0)})), 0);
    expectDensity("pHYs 300 DPI", imageDeclaredDensity(png({pixelDensity(11811, 11811, 1)})), 0);
    expectDensity("pHYs after IDAT is not a declaration",
                  imageDeclaredDensity(png({}, pixelDensity(7559, 7559, 1))), 0);
    expectDensity("pHYs with a truncated payload",
                  imageDeclaredDensity(png({chunk("pHYs", QByteArray(4, '\0'))})), 0);
    expectDensity("an IEND where the header should be",
                  imageDeclaredDensity(png({chunk("IEND", QByteArray())})), 0);
    expectDensity("a length field longer than the file",
                  imageDeclaredDensity(png({chunkHeader("tEXt", 1u << 20)})), 0);
    expectDensity("200 KB of comment before pHYs",
                  imageDeclaredDensity(png({chunk("tEXt", QByteArray(200 * 1024, 'x')),
                                            pixelDensity(7559, 7559, 1)})), 2);
    expectDensity("nothing but a chunk cap full of chunks",
                  imageDeclaredDensity(png(QList<QByteArray>(80, chunk("tEXt", QByteArray(1, 'x'))))), 0);
    expectDensity("a truncated PNG", imageDeclaredDensity(png({}).left(6)), 0);
    expectDensity("not an image at all", imageDeclaredDensity(QByteArray("GIF89a and then some", 20)), 0);
    expectDensity("empty bytes", imageDeclaredDensity(QByteArray()), 0);
    expectDensity("a path that does not exist",
                  imageDeclaredDensityOfFile(QStringLiteral("/nonexistent/x.png")), 0);
    expectDensity("an empty path", imageDeclaredDensityOfFile(QString()), 0);

    const QString directory = QDir::tempPath();
    const auto writeProbe = [&directory](const QString &name, const QByteArray &bytes) {
        const QString path = directory + QLatin1Char('/') + name;
        QFile file(path);
        if (!file.open(QIODevice::WriteOnly)) {
            return QString();
        }
        file.write(bytes);
        file.close();
        return path;
    };
    const QString onePath = writeProbe(QStringLiteral("vshot-density-1x.png"),
                                       png({pixelDensity(3780, 3780, 1)}));
    const QString twoPath = writeProbe(QStringLiteral("vshot-density-2x.png"),
                                       png({pixelDensity(7559, 7559, 1)}));
    const QString webpPath = writeProbe(QStringLiteral("vshot-density-2x.webp"),
                                        webpWith(QByteArrayLiteral("EXIF"), exifBlob(192), true));
    expectDensity("the same PNG read from disk, 1x", imageDeclaredDensityOfFile(onePath), 1);
    expectDensity("the same PNG read from disk, 2x", imageDeclaredDensityOfFile(twoPath), 2);
    expectDensity("and a WebP read from disk, 2x", imageDeclaredDensityOfFile(webpPath), 2);

    std::printf("--- the other containers, each in its own field ----------------\n");
    // JFIF states the resolution in dots per inch, and unit 1 is the spelling
    // this program writes; unit 0 is "aspect ratio only" and says nothing.
    expectDensity("a JFIF header at 96 DPI", imageDeclaredDensity(jpegWithJfif(96, 96, 1)), 1);
    expectDensity("a JFIF header at 192 DPI", imageDeclaredDensity(jpegWithJfif(192, 192, 1)), 2);
    expectDensity("a JFIF header at 288 DPI", imageDeclaredDensity(jpegWithJfif(288, 288, 1)), 3);
    expectDensity("a JFIF header declaring no unit", imageDeclaredDensity(jpegWithJfif(96, 96, 0)), 0);
    expectDensity("a JFIF header that disagrees with itself",
                  imageDeclaredDensity(jpegWithJfif(96, 192, 1)), 0);
    expectDensity("a JFIF header at 300 DPI", imageDeclaredDensity(jpegWithJfif(300, 300, 1)), 0);
    // Dots per centimetre, the other unit JFIF allows: 2x is 75.59 of them.
    expectDensity("a JFIF header in dots per centimetre",
                  imageDeclaredDensity(jpegWithJfif(76, 76, 2)), 2);
    expectDensity("a JPEG declaring nothing", imageDeclaredDensity(QByteArrayLiteral("\xff\xd8\xff\xda\x00\x02")), 0);
    expectDensity("a JPEG whose EXIF carries it",
                  imageDeclaredDensity(jpegWithExif(exifBlob(192))), 2);

    // WebP: the `EXIF` chunk, holding a bare TIFF blob.
    expectDensity("a WebP EXIF chunk at 96 DPI",
                  imageDeclaredDensity(webpWith(QByteArrayLiteral("EXIF"), exifBlob(96), true)), 1);
    expectDensity("a WebP EXIF chunk at 192 DPI",
                  imageDeclaredDensity(webpWith(QByteArrayLiteral("EXIF"), exifBlob(192), true)), 2);
    expectDensity("a WebP EXIF chunk carrying the `Exif\\0\\0` identifier too",
                  imageDeclaredDensity(webpWith(QByteArrayLiteral("EXIF"),
                                                QByteArrayLiteral("Exif\0\0") + exifBlob(288),
                                                true)),
                  3);
    expectDensity("a WebP with no EXIF chunk",
                  imageDeclaredDensity(webpWith(QByteArrayLiteral("VP8 "), QByteArray(8, 'x'), false)), 0);
    expectDensity("a WebP whose EXIF is not a TIFF blob",
                  imageDeclaredDensity(webpWith(QByteArrayLiteral("EXIF"), QByteArray(20, 'x'), true)), 0);

    // JPEG XL: the `Exif` box, with its offset in front.
    expectDensity("a JXL Exif box at 96 DPI", imageDeclaredDensity(jxlWithExif(exifBlob(96))), 1);
    expectDensity("a JXL Exif box at 288 DPI", imageDeclaredDensity(jxlWithExif(exifBlob(288))), 3);
    expectDensity("a JXL Exif box at 384 DPI", imageDeclaredDensity(jxlWithExif(exifBlob(384))), 4);
    expectDensity("a JXL with no Exif box",
                  imageDeclaredDensity(jxlWith(QByteArrayLiteral("jxlc"), QByteArray(16, 'x'))), 0);
    expectDensity("a JXL box whose length runs past the file",
                  imageDeclaredDensity(jxlSignature() + big(1u << 20, 4)
                                       + QByteArrayLiteral("Exif") + QByteArray(4, '\0')),
                  0);

    // AVIF: the marker this program writes, beside the reference white.
    expectDensity("an AVIF marker at 1x", imageDeclaredDensity(avifWithScale(1)), 1);
    expectDensity("an AVIF marker at 2x", imageDeclaredDensity(avifWithScale(2)), 2);
    expectDensity("an AVIF marker declaring 0", imageDeclaredDensity(avifWithScale(0)), 0);
    expectDensity("an AVIF marker beyond any scale", imageDeclaredDensity(avifWithScale(9)), 0);
    expectDensity("an AVIF with no marker",
                  imageDeclaredDensity(big(20, 4) + QByteArrayLiteral("ftyp")
                                       + QByteArrayLiteral("avif") + big(0, 4)
                                       + QByteArrayLiteral("avif") + QByteArray(32, 'x')),
                  0);
    // A file that ends inside the marker: the four bytes of its payload are
    // what the reader wants, and a file without them declares nothing.
    const QByteArray whole = avifWithScale(2);
    expectDensity("an AVIF marker with its payload cut off",
                  imageDeclaredDensity(whole.left(whole.size() - 3)), 0);

    // Radiance: a header line of this program's own, beside the primaries.
    expectDensity("a Radiance scale header at 1x",
                  imageDeclaredDensity(radianceWith(QByteArrayLiteral("VSHOT_SCALE=1"))), 1);
    expectDensity("a Radiance scale header at 4x",
                  imageDeclaredDensity(radianceWith(QByteArrayLiteral("VSHOT_SCALE=4"))), 4);
    expectDensity("a Radiance file another writer made",
                  imageDeclaredDensity(radianceWith(QByteArrayLiteral("EXPOSURE=1.0"))), 0);
    expectDensity("a Radiance header declaring no scale",
                  imageDeclaredDensity(radianceWith(QByteArrayLiteral("VSHOT_SCALE=0"))), 0);
    expectDensity("a Radiance header declaring a word",
                  imageDeclaredDensity(radianceWith(QByteArrayLiteral("VSHOT_SCALE=two"))), 0);
    // Past the blank line it is pixel data, not a header, and the walk stops
    // there: the line below it is not a declaration.
    expectDensity("a Radiance scale line below the pixels",
                  imageDeclaredDensity(QByteArrayLiteral("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n")
                                       + QByteArrayLiteral("VSHOT_SCALE=2\n")),
                  0);

    // The three tags read the same way whichever container carries them.
    expectDensity("EXIF at 300 DPI is a print resolution", imageDeclaredDensity(jxlWithExif(exifBlob(11811))), 0);
    expectDensity("EXIF declaring centimetres", imageDeclaredDensity(jxlWithExif(exifBlob(96, 3))), 0);
    expectDensity("EXIF with no resolution in it",
                  imageDeclaredDensity(jxlWithExif(QByteArrayLiteral("MM") + big(42, 2) + big(8, 4)
                                                   + big(0, 2) + big(0, 4))),
                  0);

    std::printf("--- why the bytes have to be read at all ---------------------------\n");
    // Real, decodable PNGs: Qt's own writer always emits a pHYs, so the
    // undeclared one is built by dropping that chunk again.
    QImage image(64, 48, QImage::Format_RGBA8888);
    image.fill(Qt::darkGreen);
    QByteArray encoded;
    {
        QBuffer buffer(&encoded);
        buffer.open(QIODevice::WriteOnly);
        image.save(&buffer, "PNG");
    }
    const QByteArray declaringOne = rewritePixelDensity(encoded, pixelDensity(3780, 3780, 1));
    const QByteArray declaringTwo = rewritePixelDensity(encoded, pixelDensity(7559, 7559, 1));
    const QByteArray undeclared = rewritePixelDensity(encoded, QByteArray());
    if (!dump.isEmpty()) {
        const QDir directory(dump);
        const auto leave = [&directory](const QString &name, const QByteArray &bytes) {
            QFile file(directory.filePath(name));
            if (file.open(QIODevice::WriteOnly)) {
                file.write(bytes);
            }
        };
        leave(QStringLiteral("density-1x.png"), declaringOne);
        leave(QStringLiteral("density-2x.png"), declaringTwo);
        leave(QStringLiteral("density-undeclared.png"), undeclared);
    }
    // The decoded value is what the daemon used to go by, and it is not enough
    // to tell a 1x declaration from no declaration at all. What Qt reports for
    // the latter is even context-dependent -- 3780 dots per metre with a
    // QGuiApplication around, 3937 (100 DPI) without one -- while both mean
    // "not a density" to any rule that wants 2..4. What the chunk says is
    // exact.
    const auto reportDecoded = [](const char *what, const QByteArray &bytes) {
        const QImage decoded = QImage::fromData(bytes);
        const int dotsPerMeter = decoded.isNull() ? 0 : decoded.dotsPerMeterX();
        const double ratio = dotsPerMeter / (1.0 / 0.0254) / 96.0;
        std::printf("ok    %-46s -> %d dpm (%.3f x, not a density)\n", what, dotsPerMeter, ratio);
        if (decoded.isNull() || ratio > 1.5) {
            std::printf("FAIL  %-46s expected a decode in the 1x-or-nothing band\n", what);
            ++failures;
        }
    };
    reportDecoded("decoded: the 1x capture", declaringOne);
    reportDecoded("decoded: the undeclared PNG", undeclared);
    expectDensity("chunks of the 1x capture", imageDeclaredDensity(declaringOne), 1);
    expectDensity("chunks of the 2x capture", imageDeclaredDensity(declaringTwo), 2);
    expectDensity("chunks of the undeclared PNG", imageDeclaredDensity(undeclared), 0);
    expectDensity("the 2x capture still reads as 2x decoded",
                  QImage::fromData(declaringTwo).dotsPerMeterX(), 7559);

    std::printf("--- result ---------------------------------------------------------\n");
    std::printf("%s (%d failure(s))\n", failures == 0 ? "ALL PASS" : "FAILURES", failures);
    return failures == 0 ? 0 : 1;
}
