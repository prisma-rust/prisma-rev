"""Console setup shared by the scripts in `scripts/`.

Windows consoles default to the legacy ANSI code page (cp936 on Chinese
Windows). Printing a status glyph such as `✅` then raises
`UnicodeEncodeError`, the process exits non-zero, and `pr_ready.py` reports a
failing check even though every command succeeded (#1019). Every script that
prints such glyphs calls `force_utf8_streams()` before writing anything.

Importing this module has no side effects; call the helper explicitly so the
order of imports does not decide when the streams are reconfigured.
"""

from __future__ import annotations

import sys


def force_utf8_streams() -> None:
    """Switch stdout/stderr to UTF-8, replacing anything the code page lacks."""
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure is not None:
            reconfigure(encoding="utf-8", errors="replace")
