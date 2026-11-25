# ADR-002: Decoding backend

- **Status:** Implemented with a Bubblewrap worker; format expansion deferred
- **Date:** 2026-04-19
- **Revised:** 2026-10-11

## Context

The viewer and the CLI must deliver the same pixels, EXIF orientation and
limits against hostile images. Using different backends before a common layer
exists would raise the chance of divergence and would advertise formats the
product cannot open yet.

## Current decision

GUI and CLI use `bigimage_core::open_image`, based on the `image` crate, with
content-based detection, EXIF orientation and 1 GiB/256 MP limits. In the
viewer, decoding runs on a dedicated thread and successive requests are
coalesced, so the GTK loop is not blocked.

In the executable, the core receives a backend that delegates decoding,
dimensions, previews and metadata to a separate Bubblewrap worker. That worker
uses the same `image` implementation, reads the input through a descriptor and
delivers pixels over a validated protocol, with process limits and supervised
cancellation. The application refuses the image if the sandbox does not start.
The standalone library stays local by default; encoders and AI inference are
not isolated. Details and limits in [QUALITY.md](QUALITY.md).

The formats actually supported are only those exposed by
`bigimage_core::Format`. HEIC, JPEG XL, SVG and RAW have no empty feature flags
and do not appear in the desktop metadata.

## Evolution considered

Glycin remains an option to replace or extend the current loaders, provided
that:

1. there is a single adapter to the core's internal type;
2. the same orientation, dimension and limit tests cover both backends;
3. each format is advertised only after passing real tests;
4. the dependency is available in the supported distributions.

Until these criteria are met, the current backend is the official
implementation, not a temporary fallback.
