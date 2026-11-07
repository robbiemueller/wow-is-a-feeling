#!/usr/bin/env python3
"""Fetch the app's fonts from google/fonts and trim the big one.

    pip install fonttools
    python tools/subset_fonts.py

Marcellus and Almendra declare Reserved Font Names, so they ship byte-for-byte.
Fira Sans Condensed has no reserved name, so it's safe to subset. It covers
Latin, Greek and Cyrillic; anything else falls back to a system font.
"""
from __future__ import annotations

import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "assets" / "fonts"
BASE = "https://raw.githubusercontent.com/google/fonts/main/ofl"

KEEP = {
    "Marcellus-Regular.ttf": "marcellus/Marcellus-Regular.ttf",
    "Almendra-Bold.ttf": "almendra/Almendra-Bold.ttf",
}
SUBSET = {
    "FiraSansCondensed-Regular.ttf": "firasanscondensed/FiraSansCondensed-Regular.ttf",
    "FiraSansCondensed-Medium.ttf": "firasanscondensed/FiraSansCondensed-Medium.ttf",
}
LICENSES = {
    "OFL-Marcellus.txt": "marcellus/OFL.txt",
    "OFL-Almendra.txt": "almendra/OFL.txt",
    "OFL-FiraSansCondensed.txt": "firasanscondensed/OFL.txt",
}
UNICODES = "U+0020-007E,U+00A0-024F,U+0370-03FF,U+0400-04FF,U+2010-2027,U+2030-205E,U+20AC,U+2122,U+2190-2193,U+2212,U+25A0-25FF"


def fetch(path: str) -> bytes:
    with urllib.request.urlopen(f"{BASE}/{path}") as resp:
        return resp.read()


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for name, path in {**KEEP, **LICENSES}.items():
        (OUT / name).write_bytes(fetch(path))
    with tempfile.TemporaryDirectory() as tmp:
        for name, path in SUBSET.items():
            src = Path(tmp) / name
            src.write_bytes(fetch(path))
            subprocess.run(
                [
                    sys.executable, "-m", "fontTools.subset", str(src),
                    f"--unicodes={UNICODES}",
                    "--layout-features=kern,liga,mark,mkmk,locl",
                    "--no-hinting",
                    f"--output-file={OUT / name}",
                ],
                check=True,
            )
    for f in sorted(OUT.iterdir()):
        print(f"{f.name}: {f.stat().st_size // 1024} KB")


if __name__ == "__main__":
    main()
