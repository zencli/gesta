#!/usr/bin/env python3
"""Render the icon sources to the PNG sizes a release ships.

Three sources, because one drawing does not serve every size:

  gesta.svg          the full mark: dotted axis, sparkle, glass bevel, faint grid
  gesta-small.svg    axis and sparkle dropped, letter enlarged, for 24-64
  gesta-16.svg       full-bleed tile, letter at 46 of 64 units, for 16 only

Measured, not assumed: the full mark at 16 px collapses into a blob. Its bowl is 7 pixels tall, so the
counter is one pixel and the descender fuses with it. A mark that survives at 128 px is not evidence
that it survives at 16, and the taskbar is 16.

Rendering runs in Chrome, which is where the SVG is known to render correctly. A second parser would
disagree about gradients and filters, and the disagreement would show up in the shipped icon rather
than in a diff.

`render_all(js)` takes the browser harness's `js` helper, so this is driven from the harness rather
than run as a standalone script: the harness owns the browser, and a subprocess would start a second
one.
"""
import base64
import pathlib
import time

ASSETS = pathlib.Path(__file__).resolve().parent.parent / "assets"

# size -> source file
PLAN = {
    16: "gesta-16.svg",
    24: "gesta-small.svg",
    32: "gesta-small.svg",
    48: "gesta-small.svg",
    64: "gesta-small.svg",
    128: "gesta.svg",
    256: "gesta.svg",
    512: "gesta.svg",
    1024: "gesta.svg",
}


def draw(js, size, svg_path):
    """Draw one SVG at one size and return the PNG bytes."""
    b64 = base64.b64encode(pathlib.Path(svg_path).read_bytes()).decode()
    js(
        "(function(){"
        f"var sz = {size};"
        "var c = document.createElement('canvas'); c.width = sz; c.height = sz;"
        "var x = c.getContext('2d');"
        "window.__done = false; window.__png = null;"
        "var img = new Image();"
        "img.onload = function(){ x.drawImage(img,0,0,sz,sz);"
        "  window.__png = c.toDataURL('image/png'); window.__done = true; };"
        "img.onerror = function(){ window.__done = 'error'; };"
        f"img.src = 'data:image/svg+xml;base64,{b64}';"
        "})()"
    )
    for _ in range(100):
        state = js("window.__done")
        if state is True:
            break
        if state == "error":
            raise RuntimeError(f"{svg_path} failed to render at {size}")
        time.sleep(0.1)
    else:
        raise RuntimeError(f"{svg_path} did not finish rendering at {size}")

    data = js("window.__png")
    if not data or not data.startswith("data:image/png;base64,"):
        raise RuntimeError(f"{svg_path} produced no PNG at {size}")
    return base64.b64decode(data.split(",", 1)[1])


def render_all(js):
    """Write every size and return {size: bytes written}."""
    out = {}
    for size, src in sorted(PLAN.items()):
        raw = draw(js, size, ASSETS / src)
        (ASSETS / f"gesta-{size}.png").write_bytes(raw)
        out[size] = len(raw)
    return out
