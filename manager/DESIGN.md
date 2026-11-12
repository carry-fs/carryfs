---
name: CarryFS Manager
description: Native device-first storage inspection and maintenance.
colors:
  primary: "oklch(60% 0.17 256)"
  focus: "oklch(58% 0.15 264)"
  background: "oklch(95.8% 0.004 265)"
  sidebar: "oklch(93.8% 0.006 265)"
  ink: "oklch(22.5% 0.006 265)"
  secondary-ink: "oklch(54.019% 0.006 265)"
  divider: "oklch(22.5% 0.006 265 / 10%)"
typography:
  heading:
    fontFamily: "system-ui"
    fontSize: "24px"
    fontWeight: 600
  application-title:
    fontFamily: "system-ui"
    fontSize: "18px"
    fontWeight: 600
  body:
    fontFamily: "system-ui"
    fontSize: "13px"
    fontWeight: 400
  label:
    fontFamily: "system-ui"
    fontSize: "12px"
    fontWeight: 500
  control:
    fontFamily: "system-ui"
    fontSize: "13px"
    fontWeight: 500
    lineHeight: "20px"
rounded:
  control: "6px"
  file-row: "5px"
spacing:
  detail: "4px"
  control: "8px"
  group: "16px"
  sidebar-inset: "20px"
  content-inset: "24px"
components:
  button-neutral:
    backgroundColor: "{colors.sidebar}"
    textColor: "{colors.ink}"
    typography: "{typography.control}"
    rounded: "{rounded.control}"
    height: "24px"
    padding: "0 12px"
  button-ghost:
    backgroundColor: "transparent"
    textColor: "{colors.ink}"
    typography: "{typography.control}"
    rounded: "{rounded.control}"
    height: "24px"
    padding: "0 12px"
  button-contrast:
    backgroundColor: "{colors.ink}"
    textColor: "{colors.background}"
    typography: "{typography.control}"
    rounded: "{rounded.control}"
    height: "24px"
    padding: "0 12px"
  target-input:
    backgroundColor: "{colors.background}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    height: "28px"
  source-row:
    typography: "{typography.control}"
    rounded: "{rounded.control}"
    height: "44px"
    padding: "0 8px"
---

# Design System: CarryFS Manager

## Overview

**Creative North Star: "Native storage utility"**

Native desktop utility in Operate mode. Keep Rune Kit controls, restrained light surfaces, dark text, and compact labels. The user-supplied screenshots define the device-first hierarchy; native utility conventions define the controls.

Source identity and observed state lead each task. Selection stays quiet, technical details open on demand, and operation results remain attached to the source. The implementation uses neutral selected controls and a blue capacity indicator and focus ring.

**Key Characteristics:**

- Device sidebar with stable volume tabs.
- Compact native controls and readable source identity.
- Progressive disclosure for system storage and technical details.
- Independent structure and file-data results.

## Colors

The light palette comes from Rune's light theme and `Kit::from_palette`, preserving the source OKLCH values. Tokens above are normative; derived colors follow the pinned Rune Kit palette, not an independent theme.

### Primary

Blue marks used capacity. A separate blue focus token identifies keyboard focus. Selected tabs, sources, and app navigation use Rune Kit Neutral controls; primary actions generally use Neutral or Contrast.

### Neutral

The cool background carries the content. The darker sidebar also supplies neutral selected controls. Dark ink carries values and headings; secondary ink carries captions and explanatory text. Low-alpha ink supplies one-point dividers.

**The Observed State Rule.** Use color and text to describe real selection, capacity, and operation state. Missing observations stay unknown.

## Typography

Use Rune's default native UI font selection, represented as `system-ui` in the portable tokens. Keep platform font metrics and tracking. There is no separate display face.

Source and page headings use the heading role. The sidebar application name uses application-title. Body copy and values use body; captions and explanatory text use label. Rune Kit buttons use control. Long values wrap where possible; sidebar identities truncate within their bounded row.

**The Native Control Rule.** Keep Rune Kit control geometry, focus treatment, and glyphs when extending this application.

## Layout

The sidebar is fixed at 248 points, with sidebar-inset padding and group gaps. The main column grows with content-inset padding and 20-point gaps between navigation, divider, content viewport, and footer. The initial window is 1120 by 760 points; the minimum is 940 by 640. These are desktop bounds, not web breakpoints. Portable `px` tokens represent Rune logical points.

Overview, Files, and Activity remain stable selected-source tabs. Software & updates is app-level navigation in the sidebar. Overview puts name, path or identity, access, format, capacity, and independent integrity states before maintenance actions.

Default sources are external storage and CarryFS volumes. Show system devices reveals a collapsible physical/virtual storage hierarchy with 12-point indentation per level. Opened images remain in a separate list. Technical volume details open inline. Keep names and capacity ahead of raw block identifiers.

Use column scroll viewports with Rune Kit ScrollBar for devices, images, content, file lists, and reports. Main and file viewports grow within the window; the image list is capped at 112 points. The footer stays outside the main scroll area.

## Elevation & Depth

The layout uses tonal separation and one-point dividers. Neutral buttons keep Rune Kit's shallow contact shadow and ring. Contrast buttons keep its restrained sheen and contact shadow. Ghost buttons have no resting shadow. Inputs use the Kit focus or invalid outline. Do not turn the native button shadows into large container elevation.

## Shapes

Controls use the control radius. File rows use file-row. Storage capacity is a slim horizontal bar. Disclosure triangles and folder/file glyphs come from Rune Kit. Avoid decorative containers around every group; headings, spacing, and dividers carry the hierarchy.

## Components

### Buttons

Use Neutral for ordinary actions and selected navigation; Ghost for unselected navigation, refresh, and disclosures; Contrast for Open read-only. Keep the Kit hover/press overlays, two-point keyboard focus ring with 2.5-point offset, and disabled opacity. The ordinary button is 24 points high. Source rows expand to 44 points for a name and identity caption.

### Inputs / Fields

The exact-target formatting confirmation uses Rune Kit TextInput with an accessible target label. It requires the exact device identifier before enabling the Terminal handoff. Keep the 28-point input, control radius, and one-point inset focus treatment.

### Navigation

Overview / Files / Activity expose tablist and tab roles with the selected state. Devices expose tree/treeitem roles, expansion, and selected state. Images expose listbox/option roles with selected state. Software & updates exposes Current view when selected. Keep keyboard focus groups and accessible labels on icon controls.

### Storage and integrity

Capacity shows observed total, used, available, and reserved space. Structure and file-data checks start at Not checked yet and retain independent results by UUID and generation. Present Passed, incomplete, unsupported, busy, and failed states distinctly. Activity attaches results and failures to the source.

Long tasks show a real stage and elapsed time. Cancellation of read-only subprocess operations leaves the source intact; recovery may leave explicitly incomplete destination files. Explain actions that require an unmounted source.

Formatting is a separate inline exact-target flow, limited to a verified external physical partition and the installed formatter. The system interactive formatter runs in Terminal. Software installation opens the existing interactive installer, preserving adapter and mounted-volume constraints. In-place format migration stays unavailable until implemented and verified.

## Do's and Don'ts

### Do:

- Do lead with source names and capacity, then disclose block identifiers and technical details.
- Do preserve keyboard access, selected-state semantics, accessible labels, and bounded scrolling.
- Do keep software versions and updates separate from on-disk format compatibility.
- Do show a real running stage and elapsed time; retain check and scrub results by UUID and generation.

### Don't:

- Don't use text characters as folder, disk, or disclosure icons.
- Don't show internal system storage or unrelated installer images in the default source list.
- Don't invent percentage progress, successful checks, adapter versions, or format migration support.
- Don't hide destructive targets, force unmount, repair sources implicitly, or silently install software.

Evidence: `manager/src/main.rs`, pinned Rune/Rune Kit revision `656af1d`, and the captures in `.impeccable/review/` at both desktop bounds. The source screenshots and native conventions remain the visual authority. No generated comp is required.
