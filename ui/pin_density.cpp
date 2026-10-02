// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#include "pin_density.hpp"

#include <QBuffer>
#include <QFile>
#include <QIODevice>
#include <QtEndian>

#include <array>
#include <cmath>

namespace vshot {
namespace {

// The eight bytes every PNG starts with.
constexpr std::array<char, 8> kPngSignature{'\x89', 'P', 'N', 'G', '\r', '\n', '\x1a', '\n'};

// The twelve bytes every JPEG XL container opens with.
constexpr std::array<char, 12> kJxlSignature{'\0', '\0', '\0', '\x0c', 'J', 'X', 'L', ' ',
                                             '\r', '\n', '\x87', '\n'};

// The marker AVIF carries the density in, in the box this program writes beside
// the reference white.  AVIF is the one format here with nowhere standard to
// put it: its EXIF is a `meta` item rather than a box.
constexpr char kScaleUuid[16] = {'v', 's', 'h', 'o', 't', '.', 's', 'c', 'a',
                                 'l', 'e', '0', '0', '0', '0', '1'};

// IFD0 tags EXIF keeps a physical resolution in, and the TIFF types of the
// values: a rational for each resolution, a short for the unit.
constexpr quint32 kTagXResolution = 0x011a;
constexpr quint32 kTagYResolution = 0x011b;
constexpr quint32 kTagResolutionUnit = 0x0128;
constexpr quint32 kTypeShort = 3;
constexpr quint32 kTypeRational = 5;
// `ResolutionUnit` 2 is inches, the unit 96-DPI-per-step is stated in.
constexpr quint32 kUnitInches = 2;

// A well-formed header is a handful of chunks. A file that never reaches its
// pixels — malformed, or hostile — must not keep the walk going.
constexpr quint32 kMaxHeaderChunks = 64;

// The most of a single chunk's payload that is read to look at it.  The two
// that matter are a JFIF header and an EXIF blob, both of them hundreds of
// bytes at most.
constexpr qint64 kMaxProbe = 1 << 16;

// Two densities are the same when their DPI agree to a twentieth: a writer
// that rounds 96 DPI to whole pixels per metre lands within that.
constexpr double kPiPerMetrePerDpi = 1.0 / 0.0254;
constexpr double kDensityTolerance = 0.05;

// Reads exactly `count` bytes into `buffer`, false when the device runs out.
bool readExact(QIODevice &device, QByteArray &buffer, qint64 count)
{
    if (count < 0) {
        return false;
    }
    buffer = device.read(count);
    return buffer.size() == count;
}

// The density a four-byte little-endian marker carries, or 0 when it is not one
// of ours.  This is the form the AVIF box holds.
int densityFromMarker(const QByteArray &payload)
{
    if (payload.size() < 4) {
        return 0;
    }
    const quint32 density = qFromLittleEndian<quint32>(payload.constData());
    return (density >= 1 && density <= 4) ? static_cast<int>(density) : 0;
}

// One TIFF value, false when the blob ends before it.  Every read is
// bounds-checked: the blob came out of a file that may be anything at all, and
// an offset inside it is a number the writer chose.
bool tiffShort(const QByteArray &tiff, qint64 at, bool little, quint32 *out)
{
    if (at < 0 || at + 2 > tiff.size()) {
        return false;
    }
    const auto *bytes = reinterpret_cast<const uchar *>(tiff.constData()) + at;
    *out = little ? qFromLittleEndian<quint16>(bytes) : qFromBigEndian<quint16>(bytes);
    return true;
}

bool tiffLong(const QByteArray &tiff, qint64 at, bool little, quint32 *out)
{
    if (at < 0 || at + 4 > tiff.size()) {
        return false;
    }
    const auto *bytes = reinterpret_cast<const uchar *>(tiff.constData()) + at;
    *out = little ? qFromLittleEndian<quint32>(bytes) : qFromBigEndian<quint32>(bytes);
    return true;
}

// The density a bare TIFF blob declares, or 0 when it declares none of ours.
//
// The three tags are looked up by number wherever they sit in the IFD: one a
// camera wrote is full of others, in an order this program did not choose.
int densityFromExif(const QByteArray &tiff)
{
    if (tiff.size() < 8) {
        return 0;
    }
    bool little = false;
    if (tiff.startsWith(QByteArrayLiteral("II"))) {
        little = true;
    } else if (!tiff.startsWith(QByteArrayLiteral("MM"))) {
        return 0;
    }
    quint32 magic = 0;
    if (!tiffShort(tiff, 2, little, &magic) || magic != 42) {
        return 0;
    }
    quint32 ifd = 0;
    if (!tiffLong(tiff, 4, little, &ifd)) {
        return 0;
    }
    quint32 count = 0;
    if (!tiffShort(tiff, ifd, little, &count)) {
        return 0;
    }
    // A bound on the walk, for the same reason the chunk walks have one.
    const quint32 entries = std::min<quint32>(count, 64);
    quint32 unit = 0;
    double x = 0.0;
    double y = 0.0;
    for (quint32 index = 0; index < entries; ++index) {
        const qint64 at = static_cast<qint64>(ifd) + 2 + static_cast<qint64>(index) * 12;
        // An entry is the tag, the type, the value count and -- for a value
        // that does not fit in four bytes -- where it is.  A count of zero is
        // not an entry, and a resolution is always one value.
        quint32 tag = 0;
        quint32 type = 0;
        quint32 count = 0;
        quint32 at_value = 0;
        if (!tiffShort(tiff, at, little, &tag) || !tiffShort(tiff, at + 2, little, &type)
            || !tiffLong(tiff, at + 4, little, &count) || count == 0
            || !tiffLong(tiff, at + 8, little, &at_value)) {
            return 0;
        }
        if (tag == kTagResolutionUnit && type == kTypeShort) {
            // A SHORT is two bytes and the value field is four, so the number
            // sits in the first two of them: read as a word, not as a long.
            if (!tiffShort(tiff, at + 8, little, &unit)) {
                return 0;
            }
            continue;
        }
        if ((tag == kTagXResolution || tag == kTagYResolution) && type == kTypeRational) {
            quint32 numerator = 0;
            quint32 denominator = 0;
            if (!tiffLong(tiff, at_value, little, &numerator) ||
                !tiffLong(tiff, static_cast<qint64>(at_value) + 4, little, &denominator)
                || denominator == 0) {
                continue;
            }
            const double value = static_cast<double>(numerator) / denominator;
            if (tag == kTagXResolution) {
                x = value;
            } else {
                y = value;
            }
        }
    }
    if (unit != kUnitInches || x <= 0.0 || y <= 0.0 || std::abs(x - y) > 0.5) {
        return 0;
    }
    return densityFromDpi(qRound(x), qRound(y));
}

// The same for an EXIF payload, with or without the six-byte `Exif\0\0`
// identifier in front of it.  Both spellings are in the wild: a JPEG's APP1
// segment carries the identifier and WebP's `EXIF` chunk does not.
int densityFromExifPayload(const QByteArray &payload)
{
    const int bare = densityFromExif(payload);
    if (bare > 0) {
        return bare;
    }
    return payload.startsWith(QByteArrayLiteral("Exif\0\0"))
               ? densityFromExif(payload.mid(6))
               : 0;
}

// The density a JPEG declares: from its JFIF APP0 segment, or from an EXIF
// APP1 one, or 0 when it declares none before its pixels begin.
//
// Read from the device's current position, which is expected to be the start of
// the file.
int jpegDeclaredDensity(QIODevice &device)
{
    if (!device.seek(2)) { // past the SOI
        return 0;
    }
    for (quint32 seen = 0; seen < kMaxHeaderChunks; ++seen) {
        QByteArray marker;
        if (!readExact(device, marker, 2) || marker.at(0) != '\xff') {
            return 0;
        }
        const auto kind = static_cast<unsigned char>(marker.at(1));
        // The start of the entropy-coded data, and the end of the image: either
        // way nothing before it declared a density.
        if (kind == 0xda || kind == 0xd9) {
            return 0;
        }
        // Standalone markers, which carry no length.
        if (kind == 0x01 || (kind >= 0xd0 && kind <= 0xd7)) {
            if (!device.seek(device.pos() - 2)) {
                return 0;
            }
            continue;
        }
        QByteArray header;
        if (!readExact(device, header, 2)) {
            return 0;
        }
        const int length = qFromBigEndian<quint16>(header.constData());
        if (length < 2 || length - 2 > kMaxProbe) {
            return 0;
        }
        QByteArray payload;
        if (!readExact(device, payload, length - 2)) {
            return 0;
        }
        // JFIF APP0: the identifier, the version, the unit, then the two
        // densities and the thumbnail's dimensions.
        if (kind == 0xe0 && payload.startsWith(QByteArrayLiteral("JFIF\0")) && payload.size() >= 12) {
            const int unit = static_cast<unsigned char>(payload.at(7));
            const int x = qFromBigEndian<quint16>(
                reinterpret_cast<const uchar *>(payload.constData()) + 8);
            const int y = qFromBigEndian<quint16>(
                reinterpret_cast<const uchar *>(payload.constData()) + 10);
            if (unit == 1) {
                // Dots per inch, the unit this program writes.
                const int declared = densityFromDpi(x, y);
                if (declared > 0) {
                    return declared;
                }
            } else if (unit == 2) {
                // Dots per centimetre, which JFIF allows and this program never
                // writes.  Taken through the pixels-per-metre rule so the same
                // tolerance and the same range check apply.
                const int declared =
                    densityFromPixelDensity(qRound(x * 100.0), qRound(y * 100.0), 1);
                if (declared > 0) {
                    return declared;
                }
            }
        }
        if (kind == 0xe1 && payload.startsWith(QByteArrayLiteral("Exif\0\0"))) {
            const int declared = densityFromExifPayload(payload);
            if (declared > 0) {
                return declared;
            }
        }
    }
    return 0;
}

// The density a Radiance file declares, or 0.
//
// RGBE has no field for a physical resolution and no EXIF either, so the number
// goes in a header line of this program's own, beside the primaries and the
// reference white it already writes there.  The name is prefixed because
// `SCALE=` is a name a picture's own tooling could be using for something else.
int radianceDeclaredDensity(QIODevice &device)
{
    if (!device.seek(0)) {
        return 0;
    }
    // The header only.  It is text and the pixels after it are not, so the
    // split stops at the blank line that ends it rather than reading further;
    // a header longer than this is not answering the question anyway.
    const QByteArray header = device.read(kMaxProbe);
    for (const QByteArray &line : header.split('\n')) {
        if (line.isEmpty()) {
            return 0;
        }
        if (line.startsWith(QByteArrayLiteral("VSHOT_SCALE="))) {
            bool ok = false;
            const int density = line.mid(12).trimmed().toInt(&ok);
            return ok && density >= 1 && density <= 4 ? density : 0;
        }
    }
    return 0;
}

// The density a WebP declares in its `EXIF` chunk, or 0.
//
// From the device's current position, which is expected to be the start of the
// file.
int webpDeclaredDensity(QIODevice &device)
{
    if (!device.seek(12)) { // past `RIFF`, the size and `WEBP`
        return 0;
    }
    for (quint32 seen = 0; seen < kMaxHeaderChunks; ++seen) {
        QByteArray header;
        if (!readExact(device, header, 8)) {
            return 0;
        }
        const quint32 length =
            qFromLittleEndian<quint32>(reinterpret_cast<const uchar *>(header.constData()) + 4);
        if (header.startsWith(QByteArrayLiteral("EXIF"))) {
            if (length > kMaxProbe) {
                return 0;
            }
            QByteArray payload;
            if (!readExact(device, payload, length)) {
                return 0;
            }
            return densityFromExifPayload(payload);
        }
        // Every chunk is padded to an even length.
        if (!device.seek(device.pos() + length + (length & 1))) {
            return 0;
        }
    }
    return 0;
}

// The density a JPEG XL container declares in its `Exif` box, or 0.
//
// From the device's current position, which is expected to be the start of the
// file.
int jxlDeclaredDensity(QIODevice &device)
{
    if (!device.seek(12)) { // past the signature
        return 0;
    }
    for (quint32 seen = 0; seen < kMaxHeaderChunks; ++seen) {
        QByteArray header;
        if (!readExact(device, header, 8)) {
            return 0;
        }
        const quint32 length =
            qFromBigEndian<quint32>(reinterpret_cast<const uchar *>(header.constData()));
        if (length < 8) {
            return 0;
        }
        if (header.mid(4, 4) == QByteArrayLiteral("Exif")) {
            if (length - 8 > kMaxProbe) {
                return 0;
            }
            QByteArray payload;
            if (!readExact(device, payload, length - 8)) {
                return 0;
            }
            // The payload opens with the offset of the TIFF header from the end
            // of that field -- six for the `Exif\0\0` identifier, which is
            // what libjxl writes.  The box's own business, and this is where it
            // is undone.
            if (payload.size() < 4) {
                return 0;
            }
            const quint32 start =
                qFromBigEndian<quint32>(reinterpret_cast<const uchar *>(payload.constData()));
            return densityFromExifPayload(payload.mid(static_cast<qint64>(4) + start));
        }
        if (!device.seek(device.pos() + length)) {
            return 0;
        }
    }
    return 0;
}

// The density an AVIF declares, out of the box this program writes it in, or 0.
//
// The marker is found by name rather than by walking to it: which box holds it
// is the muxer's decision, and the Rust side that writes it searches the same
// way.  Only the first few megabytes are looked at, so a file that is not one
// of ours is not read to its end to find that out.
int avifDeclaredDensity(QIODevice &device)
{
    constexpr qint64 kMaxScan = 64 * 1024 * 1024;
    constexpr int kBlock = 1 << 16;
    if (!device.seek(0)) {
        return 0;
    }
    QByteArray window;
    qint64 scanned = 0;
    while (scanned < kMaxScan) {
        const QByteArray block = device.read(kBlock);
        if (block.isEmpty()) {
            return 0;
        }
        // The last fifteen bytes of the previous block could be the start of
        // the marker, so they are kept and searched again.
        window += block;
        const int at = window.indexOf(QByteArray(kScaleUuid, 16));
        if (at >= 0) {
            const qint64 needed = static_cast<qint64>(at) + 16 + 4;
            if (window.size() < needed) {
                window += device.read(needed - window.size());
            }
            return densityFromMarker(window.mid(at + 16, 4));
        }
        window = window.right(15);
        scanned += block.size();
    }
    return 0;
}

} // namespace

int densityFromDpi(int dotsPerInchX, int dotsPerInchY)
{
    if (dotsPerInchX <= 0 || dotsPerInchX != dotsPerInchY) {
        return 0;
    }
    const double ratio = static_cast<double>(dotsPerInchX) / 96.0;
    const int rounded = qRound(ratio);
    if (rounded < 1 || rounded > 4 || std::abs(ratio - rounded) > kDensityTolerance) {
        return 0;
    }
    return rounded;
}

int imageDeclaredDensity(QIODevice &device)
{
    // Each walk reads through the file, so it has to be one that can be seeked
    // in: a pipe would have to be read into memory first.
    if (device.isSequential()) {
        return 0;
    }
    QByteArray head;
    if (!device.seek(0) || !readExact(device, head, 12)) {
        return 0;
    }
    if (head.left(8) == QByteArray(kPngSignature.data(), kPngSignature.size())) {
        return pngDeclaredDensity(device);
    }
    if (head.startsWith(QByteArrayLiteral("RIFF")) && head.mid(8, 4) == QByteArrayLiteral("WEBP")) {
        return webpDeclaredDensity(device);
    }
    if (head == QByteArray(kJxlSignature.data(), kJxlSignature.size())) {
        return jxlDeclaredDensity(device);
    }
    if (head.startsWith(QByteArrayLiteral("\xff\xd8"))) {
        return jpegDeclaredDensity(device);
    }
    if (head.startsWith(QByteArrayLiteral("#?RADIANCE"))) {
        return radianceDeclaredDensity(device);
    }
    // ISO base media: the size, then `ftyp` and the brand it declares.  Only
    // the AVIF brands are looked at; an MP4 of the same shape declares no
    // density and has no marker of this program's to find.
    if (head.mid(4, 4) == QByteArrayLiteral("ftyp")
        && (head.mid(8, 4) == QByteArrayLiteral("avif")
            || head.mid(8, 4) == QByteArrayLiteral("avis"))) {
        return avifDeclaredDensity(device);
    }
    return 0;
}

int imageDeclaredDensity(const QByteArray &bytes)
{
    // QBuffer does not copy the array it is handed, and the walks only read.
    QBuffer buffer(const_cast<QByteArray *>(&bytes));
    if (!buffer.open(QIODevice::ReadOnly)) {
        return 0;
    }
    return imageDeclaredDensity(buffer);
}

int imageDeclaredDensityOfFile(const QString &path)
{
    if (path.isEmpty()) {
        return 0;
    }
    QFile file(path);
    if (!file.open(QIODevice::ReadOnly)) {
        return 0;
    }
    return imageDeclaredDensity(file);
}

int densityFromPixelDensity(int pixelsPerMeterX, int pixelsPerMeterY, int unit)
{
    // Unit 0 is "aspect ratio only" and says nothing about density; a
    // non-square declaration is not a device density either.
    if (unit != 1 || pixelsPerMeterX <= 0 || pixelsPerMeterX != pixelsPerMeterY) {
        return 0;
    }
    const double ratio = static_cast<double>(pixelsPerMeterX) / kPiPerMetrePerDpi / 96.0;
    const int rounded = qRound(ratio);
    if (rounded < 1 || rounded > 4 || std::abs(ratio - rounded) > kDensityTolerance) {
        return 0;
    }
    return rounded;
}

int pngDeclaredDensity(QIODevice &device)
{
    QByteArray chunk;
    if (!device.seek(0)) {
        return 0;
    }
    constexpr qint64 kSignatureSize = static_cast<qint64>(kPngSignature.size());
    if (!readExact(device, chunk, kSignatureSize) ||
        chunk != QByteArray(kPngSignature.data(), kPngSignature.size())) {
        return 0;
    }
    for (quint32 seen = 0; seen < kMaxHeaderChunks; ++seen) {
        if (!readExact(device, chunk, 8)) {
            return 0; // the header ends before the pixels start
        }
        const quint32 length = qFromBigEndian<quint32>(chunk.constData());
        const QByteArray type = chunk.mid(4, 4);
        if (type == QByteArrayLiteral("IDAT") || type == QByteArrayLiteral("IEND")) {
            return 0; // the pixels begin here and nothing was declared before them
        }
        if (type == QByteArrayLiteral("pHYs")) {
            if (length < 9 || !readExact(device, chunk, 9)) {
                return 0;
            }
            const quint32 x = qFromBigEndian<quint32>(chunk.constData());
            const quint32 y = qFromBigEndian<quint32>(chunk.constData() + 4);
            const int unit = static_cast<unsigned char>(chunk.at(8));
            // A declaration this large cannot be a device density, and the
            // narrowing below would be undefined behaviour.
            constexpr quint32 kSanePixelsPerMetre = 1u << 20;
            if (x > kSanePixelsPerMetre || y > kSanePixelsPerMetre) {
                return 0;
            }
            return densityFromPixelDensity(static_cast<int>(x), static_cast<int>(y), unit);
        }
        // Skip this chunk's data and its CRC without reading either: a big
        // header chunk (an ICC profile, a long comment) costs a seek. A length
        // that does not fit in what is left is a malformed file, not a reason
        // to seek somewhere undefined.
        const qint64 skip = static_cast<qint64>(length) + 4;
        const qint64 available = device.size() - device.pos();
        if (skip > available || !device.seek(device.pos() + skip)) {
            return 0;
        }
    }
    return 0;
}

} // namespace vshot
