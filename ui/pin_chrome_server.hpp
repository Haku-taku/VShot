// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#pragma once

#include <QString>

namespace vshot {

// The label-drawing side of the pin stack, driven by the daemon over a socket
// of its own.
//
// It exists because the pictures and the text cannot live in one process: the
// pictures are half-float surfaces the daemon draws itself, and a glyph in one
// would have to be laid out and rasterised by hand.  So this process draws the
// labels and nothing else -- no pins, no gestures, no state worth keeping --
// and answers the daemon's one-way stream of what to put on screen.
//
// It ends when the daemon does.  A label says what a pin is, and with no pins
// there is nothing to say.
int runPinChrome(const QString &socketPath);

} // namespace vshot
