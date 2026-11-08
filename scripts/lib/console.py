"""Console encoding helpers shared by repository scripts."""

from __future__ import annotations

import sys


def force_utf8_streams() -> None:
    """Make stdout/stderr UTF-8 so check scripts never die on emoji.

    A Chinese Windows console defaults to GBK (cp936), where printing the
    result icons raises `UnicodeEncodeError` and turns a passing check into a
    red one. CI (Linux) is already UTF-8, so the call is a no-op there.
    """
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure is not None:
            reconfigure(encoding="utf-8", errors="replace")
