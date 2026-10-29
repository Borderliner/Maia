#!/usr/bin/env python3
"""Write src/solar_map.rs: the interface icons Maia draws, as SVG text, from a checkout of
the Solar icon set (https://github.com/saoudi-h/solar-icons, packages/core/svgs; the
icons are 480 Design's, CC BY 4.0). Each is recoloured to white, so Maia tints it with the
theme's colour, and its numbers are cut to two decimals (a hundredth of a unit is far below
a pixel at any size Maia draws them).

Usage:  tools/solar_icons.py <solar-icons checkout>
"""
import os
import re
import sys

# Maia's name for each icon: (Solar category/name, style). Linear for the interface, Bold
# where a filled shape reads better or marks something on, Bold Duotone for the large icons
# of empty states.
ICONS = {
    "a-arrow-down": ("search/magnifier-zoom-out", "Linear"),
    "a-arrow-up": ("search/magnifier-zoom-in", "Linear"),
    "arrow-left": ("arrows/arrow-left", "Linear"),
    "arrow-right": ("arrows/arrow-right", "Linear"),
    "arrow-up": ("arrows/arrow-up", "Linear"),
    "blame": ("users/user-id", "Linear"),
    "bookmark": ("school/bookmark", "Linear"),
    "bookmark@bold": ("school/bookmark", "Bold"),
    "chevron-down": ("arrows/alt-arrow-down", "Linear"),
    "chevron-left": ("arrows/alt-arrow-left", "Linear"),
    "chevron-right": ("arrows/alt-arrow-right", "Linear"),
    "chevron-up": ("arrows/alt-arrow-up", "Linear"),
    "clock": ("time/history", "Linear"),
    "cloud": ("weather/cloud", "Linear"),
    "code": ("it/code", "Linear"),
    "columns-2": ("settings/columns-2", "Linear"),
    "computer": ("devices/laptop-minimalistic", "Linear"),
    "copy": ("ui/copy", "Linear"),
    "copy-add": ("ui/copy-add", "Linear"),
    "diamond": ("map/map", "Linear"),
    "download": ("arrows-action/download-minimalistic", "Linear"),
    "file": ("files/file", "Linear"),
    "file-search": ("files/file-search", "Linear"),
    "file-text": ("notes/document-text", "Linear"),
    "folder": ("folders/folder", "Linear"),
    "folder@duo": ("folders/folder", "BoldDuotone"),
    "follow": ("arrows-action/arrow-down-to-line", "Linear"),
    "git-branch": ("it/git-branch", "Linear"),
    "git-compare": ("it/git-diff", "Linear"),
    "globe": ("map/global", "Linear"),
    "grid-3x3": ("settings/grid-3x3", "Linear"),
    "hard-drive": ("devices/ssd-round", "Linear"),
    "house": ("ui/home", "Linear"),
    "image": ("video/gallery", "Linear"),
    "info": ("ui/info-circle", "Linear"),
    "list": ("list/list", "Linear"),
    "list-ordered": ("list/list-ordered", "Linear"),
    "list-tree": ("folders/folder-tree", "Linear"),
    "lock": ("security/lock-keyhole-minimalistic", "Linear"),
    "lock@duo": ("security/lock-keyhole-minimalistic", "BoldDuotone"),
    "lock-open": ("security/lock-keyhole-minimalistic-unlocked", "Linear"),
    "maximize-2": ("arrows-action/maximize", "Linear"),
    "menu": ("ui/hamburger-menu", "Linear"),
    "minimize-2": ("arrows-action/minimize", "Linear"),
    "monitor": ("devices/monitor", "Linear"),
    "music": ("video/music-note", "Linear"),
    "panel-left-close": ("it/panel-left-close", "Linear"),
    "pause": ("video/pause", "Bold"),
    "pencil": ("messages/pen", "Linear"),
    "pin": ("ui/pin", "Linear"),
    "play": ("video/play", "Bold"),
    "plus": ("ui/add", "Linear"),
    "refresh": ("arrows/refresh", "Linear"),
    "replace": ("arrows/transfer-horizontal", "Linear"),
    "save": ("devices/diskette", "Linear"),
    "scan-eye": ("security/eye", "Linear"),
    "scan-eye@duo": ("security/eye", "BoldDuotone"),
    "scissors": ("ui/scissors", "Linear"),
    "search": ("search/magnifier", "Linear"),
    "search@duo": ("search/magnifier", "BoldDuotone"),
    "slash": ("it/slash-square", "Linear"),
    "smartphone": ("devices/smartphone", "Linear"),
    "square-terminal": ("files/file-terminal", "Linear"),
    "table": ("settings/rows-3", "Linear"),
    "tag": ("money/tag", "Linear"),
    "text-search": ("files/file-search", "Linear"),
    "trash-2": ("ui/trash-bin-minimalistic", "Linear"),
    "turn": ("arrows/restart", "Linear"),
    "undo": ("arrows-action/undo-left-round", "Linear"),
    "usb": ("devices/flash-drive", "Linear"),
    "user": ("users/user-rounded", "Linear"),
    "video": ("video/clapperboard-play", "Linear"),
    "view-columns": ("settings/columns-3", "Linear"),
    "view-icons": ("settings/widget", "Linear"),
    "view-list": ("settings/layout-list", "Linear"),
    "view-stack": ("tools/layers-minimalistic", "Linear"),
    "volume-2": ("video/volume-loud", "Bold"),
    "volume-x": ("video/volume-cross", "Bold"),
    "x": ("ui/close", "Linear"),
}

# Maia's own, for what the set has no icon for, drawn the way its Linear icons are: on the
# same 24-unit grid with a 1.5 stroke and round ends.
OWN = {
    "eject": '<svg width="24" height="24" viewBox="0 0 24 24" fill="none"><path d="M6.12 14.5h11.76c.86 0 1.32-1.01.76-1.66l-5.88-6.79a1 1 0 0 0-1.52 0l-5.88 6.79c-.56.65-.1 1.66.76 1.66Z" stroke="#fff" stroke-width="1.5" stroke-linejoin="round"/><path d="M6 18.5h12" stroke="#fff" stroke-width="1.5" stroke-linecap="round"/></svg>',
    "wrap": '<svg width="24" height="24" viewBox="0 0 24 24" fill="none"><path d="M3 6h18M3 18h6" stroke="#fff" stroke-width="1.5" stroke-linecap="round"/><path d="M3 12h14.5a3 3 0 0 1 0 6H13m0 0 2-2m-2 2 2 2" stroke="#fff" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>',
}

NUM = re.compile(r"-?\d*\.\d+")


def tidy(svg):
    svg = svg.replace("#1C274C", "#fff").replace("#1c274c", "#fff")
    svg = re.sub(r'\s+xmlns="[^"]*"', "", svg)
    svg = NUM.sub(lambda m: ("%.2f" % float(m.group(0))).rstrip("0").rstrip(".").replace("-0", "-0") or "0", svg)
    svg = re.sub(r">\s+<", "><", svg.strip())
    return re.sub(r"\s+", " ", svg)


def main():
    root = os.path.join(sys.argv[1], "packages", "core", "svgs")
    out = {}
    for name, (src, style) in ICONS.items():
        cat, icon = src.split("/")
        with open(os.path.join(root, cat, style, icon + ".svg")) as f:
            svg = tidy(f.read())
        if "#" in svg.replace("#fff", ""):
            sys.exit("%s: a colour other than the set's own: %s" % (name, svg))
        out[name] = svg
    for name, svg in OWN.items():
        out[name] = tidy(svg)
    dst = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "src", "solar_map.rs")
    with open(dst, "w") as f:
        f.write("// Generated by tools/solar_icons.py from the Solar icon set by 480 Design (CC BY 4.0;\n")
        f.write("// see assets/SOLAR-LICENSE.txt), but for eject and wrap, which are Maia's. Do not edit.\n")
        f.write("pub const SOLAR: &[(&str, &str)] = &[\n")
        for name in sorted(out):
            f.write('    ("%s", r##"%s"##),\n' % (name, out[name]))
        f.write("];\n")
    print("%d icons, %d bytes of SVG" % (len(out), sum(len(v) for v in out.values())))


main()
