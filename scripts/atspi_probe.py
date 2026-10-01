#!/usr/bin/env python3
"""Prototype probe: what does the AT-SPI element tree actually look like?

Dumps every accessible application, its frames, and each frame's element tree
with role, name, and screen extents.  Run it with a few apps open to see the
real shape of the data a Rust client would consume.

    python3 scripts/atspi_probe.py               # every app, focused-ish view
    python3 scripts/atspi_probe.py --app missioncenter
    python3 scripts/atspi_probe.py --depth 4
"""
import argparse
import sys

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402


def extents(acc, coord=Atspi.CoordType.SCREEN):
    """Rect of an accessible in the given coordinate space, or None."""
    comp = acc.get_component_iface()
    if comp is None:
        return None
    try:
        ext = comp.get_extents(coord)
    except Exception:
        return None
    if ext.width <= 0 or ext.height <= 0:
        return None
    return (ext.x, ext.y, ext.width, ext.height)


def fmt_ext(ext):
    if ext is None:
        return "no-geometry"
    x, y, w, h = ext
    return f"{w}x{h}+{x}+{y}"


def walk(acc, depth, max_depth):
    """Yield (depth, role, name, screen_extents, window_extents)."""
    if depth > max_depth:
        return
    try:
        role = acc.get_role_name()
    except Exception:
        role = "?"
    try:
        name = acc.get_name() or ""
    except Exception:
        name = ""
    yield depth, role, name, extents(acc, Atspi.CoordType.SCREEN), extents(
        acc, Atspi.CoordType.WINDOW
    )
    try:
        count = acc.get_child_count()
    except Exception:
        return
    for i in range(count):
        try:
            child = acc.get_child_at_index(i)
        except Exception:
            continue
        if child is None:
            continue
        yield from walk(child, depth + 1, max_depth)


def show(acc, max_depth):
    for depth, role, name, screen, window in walk(acc, 1, max_depth):
        pad = "    " + "  " * (depth - 1)
        label = role
        if name:
            label += f" {name!r}"
        print(f"{pad}- {label}")
        print(f"{pad}    screen=[{fmt_ext(screen)}] window=[{fmt_ext(window)}]")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--app", help="only this application name")
    ap.add_argument("--depth", type=int, default=5, help="max tree depth")
    args = ap.parse_args()

    desktop = Atspi.get_desktop(0)
    n_apps = desktop.get_child_count()
    print(f"desktop: {n_apps} application(s)\n")

    for i in range(n_apps):
        app = desktop.get_child_at_index(i)
        if app is None:
            continue
        app_name = app.get_name()
        if args.app and args.app.lower() not in app_name.lower():
            continue
        n_frames = app.get_child_count()
        print(f"app: {app_name!r}  ({n_frames} frame(s))")
        if n_frames == 0:
            print("    <no tree exposed>\n")
            continue
        for j in range(n_frames):
            frame = app.get_child_at_index(j)
            if frame is None:
                continue
            print(f"  frame {j}: {frame.get_name()!r}")
            show(frame, args.depth)
        print()


if __name__ == "__main__":
    sys.exit(main())
