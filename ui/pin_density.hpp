// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#pragma once

#include <QByteArray>
#include <QString>

class QIODevice;

namespace vshot {

// The density a dots-per-inch pair means, or 0 when it is not one of ours.
//
// JFIF states its resolution this way rather than in pixels per metre, and
// EXIF states it this way in every container that carries one.  The rule is the
// same as the one above: 96 dots per inch is 1x, only a near-exact multiple of
// it within 1..4 counts, and a pair that disagrees with itself is not a device
// density at all.
int densityFromDpi(int dotsPerInchX, int dotsPerInchY);

// The density the image in `device` declares about itself, whichever of the
// formats this program writes it is in, or 0 when it declares none.
//
// One entry point rather than one per format: the daemon is handed whatever
// the user pinned, and which reader to use is a question the file's first bytes
// answer -- the same question `vshot` answers when it decides what to do with a
// path.  Every format puts the number where its container keeps a physical
// resolution: a PNG in `pHYs`, a JPEG in its JFIF header or its EXIF, a WebP in
// its `EXIF` chunk, a JPEG XL file in its `Exif` box.  AVIF and Radiance have
// nowhere standard for one -- AVIF's EXIF is an item rather than a box, and
// RGBE has no EXIF at all -- so each carries a marker this program writes:
// AVIF a `uuid` box beside the reference white, Radiance a `VSHOT_SCALE=`
// header line beside the primaries.
//
// 0 is the answer for a file this program did not write, which is the ordinary
// case: the daemon then sizes the pin from the output it lands on, which is
// what every pin did before any of this existed.
int imageDeclaredDensity(QIODevice &device);

// The same for bytes already in memory (a clipboard payload) and for a file on
// disk (a pinned one).
int imageDeclaredDensity(const QByteArray &bytes);
int imageDeclaredDensityOfFile(const QString &path);

// The density a PNG declares about itself — device pixels per logical pixel of
// the image, 1..4 — or 0 when it declares none.
//
// A PNG states this with a `pHYs` chunk in pixels per metre, and 96 DPI is a
// density like any other: it is what vshot writes for a capture taken on a
// scale-1 output, and the pin daemon has to read it back as 1x. Otherwise that
// capture gets sized from the output it lands on instead, and a 1080p crop
// pinned on a 4K screen comes out half the size it had on screen.
//
// The chunk has to be there for the declaration to count. A decoded PNG cannot
// show the difference: Qt reports 96 DPI for an image that declares nothing at
// all, and what exactly it reports there depends on the context (3780 dots per
// metre with a QGuiApplication around, 3937 without one), while an image that
// declares nothing has to keep falling through to the rules in the daemon,
// which size it from the output it lands on. Only the bytes say which is which.
//
// Anything that is not a near-exact multiple of 96 DPI within 1..4 is not a
// device density: a print resolution (300 DPI), an aspect-ratio-only `pHYs`
// (unit 0) and a non-square one all read as 0.
int densityFromPixelDensity(int pixelsPerMeterX, int pixelsPerMeterY, int unit);

// The PNG branch of that walk, on its own: the header chunks only, and the
// walk stops at `IDAT`, where the pixels begin, so it stays cheap on a large
// file. 0 when the bytes are not a PNG, the header is malformed, or nothing is
// declared.  Exported because it is the one reader with a rule of its own to
// get wrong; the others are exercised through `imageDeclaredDensity`.
int pngDeclaredDensity(QIODevice &device);

} // namespace vshot
