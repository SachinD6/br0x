# 0001 Use WebKitGTK 6.0 for Linux v1

Status: accepted.

## Context

br0x needs startup under 300 ms, low RAM for 9 tabs, and full compatibility with YouTube, Gmail, and Figma. The team ships Linux first through Flatpak and AUR and keeps core logic portable for later Windows and Mac shells.

## Decision

Build Linux v1 on WebKitGTK 6.0 with GTK4 and Libadwaita. Keep full JavaScript, JIT, WASM, and WebGL2 enabled. Keep all platform calls behind the lifecycle and blocker seams so a later shell can swap the view layer.

## Alternatives

- Chromium or CEF: rejected. It raises baseline RAM, slows cold start, and complicates Flatpak distribution.
- Servo: rejected. It promises low memory but lacks production compatibility for Gmail and Figma today. Revisit in 12 months.

## Consequences

- Native GTK4 behavior, fast start, shared system WebKit through Flatpak.
- Memory control comes from freeze and park, not process flags.
- UI code must never import WebKit types outside the lifecycle adapter.
