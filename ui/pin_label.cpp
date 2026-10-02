// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

#include "pin_label.hpp"

#include <algorithm>

namespace vshot {

int paintRadius(std::uint32_t radius, const QSize &size)
{
    const int most = std::max(0, std::min(size.width(), size.height()) / 2);
    return std::min(static_cast<int>(radius), most);
}

QFont tagFont()
{
    QFont font;
    font.setPixelSize(kLabelPixelSize);
    font.setBold(true);
    return font;
}

QRect labelBox(const QFontMetrics &metrics, const QString &text)
{
    const int pad = metrics.height() / 3;
    return metrics.boundingRect(text).adjusted(-pad, -pad / 2, pad, pad / 2);
}

QRect tagBox(const QString &text, const QPoint &corner, bool atBottomRight, const QRect &bounds)
{
    const QFontMetrics metrics(tagFont());
    const int pad = metrics.height() / 3;
    QRect box = labelBox(metrics, text);
    if (atBottomRight) {
        box.moveBottomRight(corner - QPoint(pad, pad));
    } else {
        box.moveTopLeft(corner + QPoint(pad, pad));
    }
    return box.intersected(bounds.adjusted(0, 0, -1, -1));
}

} // namespace vshot
