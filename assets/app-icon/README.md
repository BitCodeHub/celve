# Celve app icon

A selected spreadsheet cell, with its fill-handle corner, holding Sage's four-point sparkle and a
small purple companion star, on a deep-green rounded tile. Designed by **Astra** (AI image
generation, directed by Celve contributors) on 2026-10-11 from a written brief: original artwork,
deliberately unlike any vendor's spreadsheet logo (no letters, no "X", no folded page, no table
grid, no bar chart), readable at 16 px.

## Files

| File | What |
|---|---|
| `celve-master.png` | Canonical artwork, 1024 px, transparent corners |
| `celve-1024.png` | Full-bleed tile, 1024 px |
| `celve-macos-512.png` | Runtime window/Dock icon on macOS (Apple's 824/1024 grid) |
| `celve.icns` | macOS bundle icon |
| `celve.ico` | Windows icon (16–256 px) |
| `celve.svg`, `hicolor/scalable/apps/cloud.agentlumi.celve.svg` | SVG wrapper embedding the artwork (Linux scalable icon; appstream compose needs one) |
| `hicolor/<n>x<n>/apps/cloud.agentlumi.celve.png` | Linux hicolor sizes |

The web app's `favicon.png` and `icon-192.png` (in `apps/celve-web/assets`) come from the same run.

## Regenerating

Replace `celve-master.png`, then run `python3 packaging/icons.py` (needs Pillow; `iconutil` on
macOS for the `.icns`). Update the rows in `ATTRIBUTION.md` if the author changes.
