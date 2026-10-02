// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#pragma once

#include <QList>
#include <QString>
#include <QStringList>

class QFileDialog;

namespace vshot {

// The two file dialogs vshot needs, each shown in a process of its own and each
// reporting its answer on stdout as one JSON object.
//
// They run in a process of their own because the caller cannot host them: the
// pin daemon and the capture overlay are layer-shell clients, and the dialog
// has to be one too.  Every surface vshot shows is a layer surface, and the
// compositor draws those above all ordinary windows -- so a dialog opened as a
// toplevel from inside a capture ends up under the frozen frame the user is
// working on: invisible, and unreachable by the pointer, which is what made
// the paste and the save look like dead buttons.  The protocol orders the
// surfaces of a layer by map time, so a dialog mapped after the overlay stacks
// above it.  The pixels never travel: the caller picks a path here and does the
// writing or reading itself.

// One format the save dialog offers, as the codec registry describes it: the
// name the command line and the config use, and the suffixes its files carry.
//
// The list is the *caller's*, not this file's: which formats a build can write
// is the codec registry's answer, and a dialog that knew names of its own would
// eventually offer one the binary cannot write.
struct SaveFormat {
    QString name;          //< `png`, `jpeg`, `jxl`: what the registry calls it
    QStringList suffixes;  //< `jpg` and `jpeg` for JPEG; the first is the plain one
};

// `path` with the suffix of `format`.
//
// The two have to agree.  The bytes written are the format's, and a name that
// says otherwise is what makes another program opening it by extension, a later
// `vshot pin shot.png`, and the file URI a capture puts on the clipboard all
// talk about a PNG that is not there.
//
// A suffix the format itself uses is left alone, so `.jpeg` survives for a user
// who typed it -- it is not a second format.  A suffix of some other format is
// replaced rather than appended to: `shot.png` saved as WebP is `shot.webp`,
// not `shot.png.webp`.
QString withFormatSuffix(const QString &path, const SaveFormat &format);

// The save formats out of one command-line argument, `name/suffix/suffix,...`.
// Empty when the argument is empty, which is a caller that has no registry to
// offer and gets the format the dialog has always offered.
//
// A single argument rather than a run of them: an argv slot per format would
// make the two lists below indistinguishable from the screen name beside them.
QList<SaveFormat> saveFormatsFromArgument(const QString &argument);

// `{"ok":true,"path":"...","format":"...","sdrCopy":true}`, or `{"ok":false}`
// when the user cancelled.
//
// `formats` is what the content at hand can be saved as -- the SDR formats for
// a pin that is a plain picture, the HDR ones for a pin that came from an HDR
// capture.  `hdr` is which of those two it is, and it is what puts the "also
// save the SDR copy" switch on the dialog: an HDR file written alone leaves
// nothing for a reader that cannot show HDR, so the choice is the user's every
// time.
//
// `screenName` is the output the dialog should open on, as Qt names it; empty
// falls back to the primary one.
int runSaveDialog(const QString &suggestedPath, const QList<SaveFormat> &formats, bool hdr,
                  const QString &screenName = QString());

// The same shape, for picking an existing image to open.
int runOpenDialog(const QString &suggestedPath, const QString &screenName = QString());

// Builds the same dialog without showing it, for the offline check that drives
// its widgets.  The check has to be able to tell a dialog that was dressed
// (stylesheet applied, thumbnail grid set) from one that was not, and the only
// honest way to do that is on the widget tree this function returns rather than
// on a second dialog built to look like it.  The caller owns the dialog.
QFileDialog *createFileDialog(bool saving, const QString &suggestedPath,
                              const QList<SaveFormat> &formats = {}, bool hdr = false);

// One place in the file dialogs' sidebar.
struct Place {
    QString path;  //< an absolute local directory
    QString title; //< what the file manager calls it, empty when it has no name
};

// Reads the places out of the given bookmark files rather than out of the ones
// this machine happens to have, so the check can drive the reader with fixtures
// of its own: the formats are the file managers', not ours, and getting one
// wrong is invisible until a user's sidebar comes up with a row that does
// nothing.
//
// KDE's file is XBEL; GTK's is one `file://` URI per line.  Which is which is
// decided by the `.xbel` suffix, which is what the two actually differ in.
QList<Place> readPlaces(const QStringList &bookmarkFiles);

// The files this machine's file managers record their bookmarks in, whether or
// not any of them exists: the KDE one, then GTK 3's and GTK 4's.
QStringList bookmarkFiles();

} // namespace vshot
