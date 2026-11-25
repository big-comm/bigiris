# Screenshots

Images referenced by the `<screenshots>` element in
[`data/com.biglinux.Iris.metainfo.xml`](../../data/com.biglinux.Iris.metainfo.xml).
Flathub and KDE Discover fetch these URLs (resolved through
raw.githubusercontent) for the application page, so changing them changes what
users see in the store.

## Contract

- **Resolution**: 1280×720 or larger, up to 3840×2160. Any aspect ratio, but
  AppStream prefers `~16:9` for the main screenshot.
- **Format**: PNG without an embedded ICC profile (`magick out.png` already
  satisfies this).
- **Name**: `NN-description.png`, in the order they appear. The first becomes
  `<screenshot type="default">` in the metainfo.
- **Content**: the real app, no watermarks, no crude mock-ups.

## Current state

`01-viewer.png` is a **placeholder** generated with ImageMagick (logo +
gradient + text). Replace it before the repository goes public:

```bash
# Capture a specific BigIris window on GNOME:
gnome-screenshot --window --file=docs/screenshots/01-viewer.png

# Or on KDE with Spectacle:
spectacle --activewindow --background --output=docs/screenshots/01-viewer.png
```

Suggested sequence for publication:

1. `01-viewer.png`: the viewer with a photo loaded, header bar visible
2. `02-prisma-convert.png`: the Convert dialog (Prisma) with a preview
3. `03-prisma-resize.png`: the Resize dialog with its modes
4. `04-remove-bg.png`: before and after remove-bg (local AI)
5. `05-file-manager.png`: the BigIris ▸ submenu in Dolphin's right-click menu

Each PNG becomes a `<screenshot>` in the metainfo, in order.

## Validation

```bash
appstreamcli validate --explain data/com.biglinux.Iris.metainfo.xml
```

It fails if the declared `width`/`height` do not match the real file; relax
it with `--no-net` while the repository is private.
