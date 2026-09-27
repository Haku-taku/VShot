// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#pragma once

#include <QString>

namespace vshot {

// Runs the resident annotation daemon: listens on a local socket, owns one
// annotation surface per output, and dispatches the
// toggle/show/hide/clear/quit/status/capture-begin/capture-end commands.
// Blocks until quit (event loop driven).
int runAnnotateServer(const QString &socketPath);

} // namespace vshot
