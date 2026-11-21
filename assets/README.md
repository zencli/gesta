# The mark

<img src="gesta.svg" alt="gesta" width="128">

A glass tile over a dark field, a geometric single-storey `g`, and a measurement axis across its bowl.
The axis is the part that says what the tool is: a changeset is a point on a line, and the dots are the
revisions either side of it. The sparkle is the changeset that has just been made.

## Three drawings, because one does not serve every size

| File | Sizes | What it is |
| --- | --- | --- |
| `gesta.svg` | 128, 256, 512, 1024 | the full mark: dotted axis, sparkle, glass bevel, faint grid |
| `gesta-small.svg` | 24, 32, 48, 64 | axis and sparkle dropped, letter enlarged and stroked heavier |
| `gesta-16.svg` | 16 | full-bleed tile, letter at 46 of 64 units |

Measured, not assumed: the full mark at 16 px collapses into a blob. The bowl is 7 pixels tall, so the
counter is one pixel and the descender fuses with it. A mark that survives at 128 px is not evidence
that it survives at 16, and the taskbar is 16.

The 24 px check also found that the dotted axis, which is the identity of the mark at full size, turns
into noise over the counter at that size. Dropping it below 128 is the trade: the letter has to win.

## Raster sizes

`gesta-16.png` through `gesta-1024.png`, one per size, and `gesta.ico` packing 16, 32, 48, 64, 128 and
256 for the Windows executable.

An ICO is a directory followed by the images, each entry naming its width as one byte, with 256 written
as 0 because it does not fit. Shipping several sizes is what makes the icon sharp in the taskbar, in
Explorer's list view and on the desktop, since Windows picks the closest one and scales only when none
fits.

## Regenerating

```
python3 bench/render_icons.py
python3 bench/make_ico.py assets
```

Rendering goes through Chrome, which is where the SVG is known to render correctly. A second parser
would disagree about gradients and filters, and the disagreement would show up in the shipped icon
rather than in a diff.

The ICO's PNG payloads are the same files, so the executable's icon and the README's differ only in
scale.
