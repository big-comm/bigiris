# ADR-001: The core separates GUI and CLI

- **Status:** Accepted
- **Date:** 2026-04-19

## Context

The initial requirements demand (R3) that "the features used by the graphical interface also be accessible through the command line", to enable CI/CD that prevents regressions in stabilized features. Without this separation, every automated test would need a display (Xvfb, headless Wayland) and would introduce flakiness.

## Decision

All decode, encode, transform, pipeline and AI logic lives in `bigimage-core` and `bigimage-ai`, **crates that do NOT depend on `gtk4`, `adw` or any graphical toolkit**. The `bigiris` binary:

- **CLI subcommands** (`bigiris convert …`, `bigiris resize …`, etc.) call the core directly, without opening a window.
- **GUI** (viewer + modal dialogs) is a client of the core: it captures the user's parameters and calls the same API as the CLI.

Integration tests trigger the SAME operation through both surfaces and compare the outputs byte for byte. A divergence breaks CI.

## Consequences

**Positive**
- CI in an Arch container without a display validates 100% of the business rules.
- Terminal users and scripts have native access to everything.
- A core refactor cannot leak into the GUI (compile-time barrier).

**Negative**
- Double parameters: each option must be exposed in clap (CLI) and in the widget (GUI). Mitigated by partial generation: parameter structs serializable through serde, used by both.
- The temptation to put "just a small visual detail" into the core. Pull request reviews must refuse it.

## Rejected alternatives

- **Core depending on gtk4 for image types**: would couple the core to a specific toolkit and rule out headless CI.
- **CLI as a thin binding layer over the GUI**: would invert the order; the GUI would become the source of truth and tests would depend on a display.
