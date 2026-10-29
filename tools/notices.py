#!/usr/bin/env python3
"""Write assets/THIRD-PARTY-NOTICES.txt: the licence texts of every crate built into
Maia on Linux, as each crate ships them, so a copy of the program carries the notices
its licences ask for. Identical texts are printed once with every crate that uses them.

Run after a dependency changes:  tools/notices.py
"""
import hashlib
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "assets", "THIRD-PARTY-NOTICES.txt")
NAMES = ("license", "licence", "copying", "notice", "unlicense", "copyright")
# Crates offering several licences that are taken under one: (its file, the licence,
# where to find the crate).
# What is built in besides the crates, with its licence beside this script's output.
ASSETS = ("OFL.txt", "SOLAR-LICENSE.txt", "SMITHAY-CLIPBOARD-LICENSE.txt")
CHOSEN = {"nohash-hasher": ("LICENSE-APACHE", "Apache-2.0", "https://crates.io/crates/nohash-hasher")}


def crates():
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--offline", "--filter-platform", "x86_64-unknown-linux-gnu"],
        cwd=ROOT, check=True, capture_output=True, text=True).stdout)
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    packages = {p["id"]: p for p in meta["packages"]}
    root = meta["resolve"]["root"]
    # What the program links: normal dependencies, not build scripts' or tests'.
    seen, todo = set(), [root]
    while todo:
        i = todo.pop()
        if i in seen:
            continue
        seen.add(i)
        for d in nodes[i]["deps"]:
            if any(k["kind"] is None for k in d["dep_kinds"]):
                todo.append(d["pkg"])
    seen.discard(root)
    # The metadata's graph also holds optional crates that another's feature names with
    # `dep?/feature` (egui-winit's `arboard?/wayland-data-control`), though nothing turns
    # them on; cargo tree lists what is built, so only those are kept.
    tree = subprocess.run(
        ["cargo", "tree", "--offline", "-e", "normal", "--target", "x86_64-unknown-linux-gnu", "--prefix", "none", "--format", "{p}"],
        cwd=ROOT, check=True, capture_output=True, text=True).stdout
    built = {tuple(line.split()[:2]) for line in tree.splitlines() if line.strip()}
    seen = {i for i in seen if (packages[i]["name"], "v" + packages[i]["version"]) in built}
    return sorted((packages[i] for i in seen), key=lambda p: (p["name"], p["version"]))


def texts(package):
    folder = os.path.dirname(package["manifest_path"])
    found = []
    if package.get("license_file"):
        found.append(os.path.join(folder, package["license_file"]))
    for name in sorted(os.listdir(folder)):
        path = os.path.join(folder, name)
        if os.path.isfile(path) and name.lower().startswith(NAMES) and path not in found:
            found.append(path)
    # 0BSD asks for no notice; a crate that offers it beside another licence is
    # taken under the other, whose text is here.
    rest = [p for p in found if "0bsd" not in os.path.basename(p).lower()]
    if rest:
        found = rest
    if package["name"] in CHOSEN:
        found = [p for p in found if os.path.basename(p) == CHOSEN[package["name"]][0]]
    out = []
    for path in found:
        with open(path, encoding="utf-8", errors="replace") as f:
            out.append(f.read().strip())
    return out


def main():
    groups, order, missing = {}, [], []
    listing = []
    for p in crates():
        label = f"{p['name']} {p['version']}"
        source = p.get("repository") or f"https://crates.io/crates/{p['name']}"
        licence = p.get("license") or "see its licence file"
        if p["name"] in CHOSEN:
            _, chosen, source = CHOSEN[p["name"]]
            licence = f"{licence}, taken as {chosen}"
        listing.append(f"  {label:<40} {licence}\n    {source}")
        bodies = texts(p)
        if not bodies:
            missing.append(p)
        for body in bodies:
            key = hashlib.sha256(body.encode()).hexdigest()
            if key not in groups:
                groups[key] = (body, [])
                order.append(key)
            groups[key][1].append(label)
    # A crate that ships no licence file is taken under the Apache-2.0 its licence
    # offers, whose text needs no copyright line of its own: listed with that text.
    apache = next((k for k in order if "Apache License" in groups[k][0] and "TERMS AND CONDITIONS FOR USE" in groups[k][0]), None)
    for p in missing:
        if apache is None or "Apache-2.0" not in (p.get("license") or ""):
            sys.exit(f"{p['name']}: no licence file and no Apache-2.0 option; add its notice by hand")
        # Names only: the addresses in a manifest are for crates.io, not for this file.
        authors = ", ".join(re.sub(r"\s*<[^>]*>", "", a) for a in p.get("authors") or []) or "its authors"
        groups[apache][1].append(f"{p['name']} {p['version']} (by {authors}; its package ships no licence file)")
    with open(OUT, "w", encoding="utf-8") as f:
        f.write("Maia is free software under the GNU General Public License, version 3 (LICENSE).\n")
        f.write("It is built with the crates below; each keeps its own licence, and their texts follow.\n")
        f.write("The bundled fonts, the interface icons (Solar, by 480 Design, CC BY 4.0) and the\n")
        f.write("clipboard code taken from smithay-clipboard keep their own licences; their texts\n")
        f.write("close this file.\n\n")
        f.write("\n".join(listing) + "\n")
        for key in order:
            body, users = groups[key]
            f.write("\n" + "=" * 78 + "\n")
            f.write("Used by: " + ", ".join(users) + "\n\n")
            f.write(body + "\n")
        # The files in assets/ for what is built in besides crates: the packages install this
        # file alone, so the texts go in it.
        for name in ASSETS:
            with open(os.path.join(ROOT, "assets", name), encoding="utf-8") as src:
                f.write("\n" + "=" * 78 + "\n")
                f.write(f"Built in: {name}\n\n")
                f.write(src.read().rstrip() + "\n")
    print(f"{OUT}: {len(listing)} crates, {len(order)} distinct texts")
    if missing:
        print("under Apache-2.0, shipping no licence file: " + ", ".join(p["name"] for p in missing))


if __name__ == "__main__":
    main()
