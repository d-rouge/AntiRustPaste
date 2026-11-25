# Assets

- `app-icon/source.png` — the colour spray-can icon (silver tube, "Antitrust"
  label, hammer shield) supplied by the project owner on 2026-11-25, replacing
  the earlier monochrome highlighter artwork. It is project artwork, not copied
  from another product.
- `app-icon/AppIcon-1024.png` — the square master derived from `source.png`: the
  artwork on a rounded off-white tile with a thin border, white background
  removed.
- `app-icon/AppIcon.icns` — macOS bundle icon generated from the master.
- `app-icon/status-source.png` — the monochrome version of the spray can supplied
  with the colour artwork on 2026-11-25. It is the source for the menu-bar icon.
- `status-icon.png` — a 36 px monochrome template derived from
  `app-icon/status-source.png`, used for the menu bar. White areas become
  transparent, so the shield, hammer and label cut out and the can keeps the same
  emblem as the source artwork.

- `dmg/background.png` and `dmg/background@2x.png` — the neutral background with
  a dashed arrow used by the disk image window. Drawn in-house with CoreGraphics
  on 2026-11-25 for this project; no third-party artwork.

When regenerating the icon or the disk-image background, keep the same
provenance and update this file.
