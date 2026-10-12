---
name: nocturnum console
description: A dark, dense admin console for a self-hosted vector database, where engine state reads at a glance and every number comes straight from the engine.
colors:
  ground: "#0b0d12"
  surface: "#0f1218"
  raised: "#141821"
  raised-hover: "#1a1f2a"
  hover: "#171c26"
  line: "#1c212c"
  line-2: "#262c39"
  line-3: "#323a4b"
  text: "#e5e7eb"
  text-2: "#a3a9b5"
  text-3: "#7a8191"
  on-accent: "#ffffff"
  accent: "#dc244c"
  accent-hover: "#e83a60"
  accent-ink: "#ff6b8a"
  accent-soft: "rgba(220, 36, 76, 0.12)"
  accent-line: "rgba(220, 36, 76, 0.45)"
  ok: "#3fb37f"
  ok-soft: "rgba(63, 179, 127, 0.12)"
  warn: "#e2a33b"
  warn-soft: "rgba(226, 163, 59, 0.11)"
  bad: "#f0616d"
  bad-soft: "rgba(240, 97, 109, 0.12)"
  danger-solid: "#b5303c"
  danger-solid-hover: "#c93a47"
  track: "#1f2431"
  meter: "#7b8496"
  series-strong: "#8fa3c7"
  series-soft: "#5d6e8c"
  json-key: "#9cb4e0"
  json-string: "#a6d18f"
  json-number: "#e0b47a"
  json-literal: "#e58aa6"
  json-punct: "#5d6577"
  json-gutter: "#3c4354"
typography:
  metric:
    fontFamily: "Geist Variable, ui-sans-serif, system-ui, sans-serif"
    fontSize: "30px"
    fontWeight: 500
    lineHeight: "34px"
    letterSpacing: "-0.03em"
    fontFeature: "'tnum'"
  headline:
    fontFamily: "Geist Variable, ui-sans-serif, system-ui, sans-serif"
    fontSize: "20px"
    fontWeight: 600
    lineHeight: "28px"
    letterSpacing: "-0.015em"
  stat:
    fontFamily: "Geist Variable, ui-sans-serif, system-ui, sans-serif"
    fontSize: "15px"
    fontWeight: 500
    letterSpacing: "-0.01em"
    fontFeature: "'tnum'"
  title:
    fontFamily: "Geist Variable, ui-sans-serif, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 600
    letterSpacing: "-0.005em"
  body:
    fontFamily: "Geist Variable, ui-sans-serif, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1.45
  label:
    fontFamily: "Geist Variable, ui-sans-serif, system-ui, sans-serif"
    fontSize: "12px"
    fontWeight: 500
  mono:
    fontFamily: "Geist Mono Variable, ui-monospace, SF Mono, Menlo, monospace"
    fontSize: "12px"
    fontWeight: 400
    fontFeature: "'tnum'"
  code:
    fontFamily: "Geist Mono Variable, ui-monospace, SF Mono, Menlo, monospace"
    fontSize: "12.5px"
    fontWeight: 400
    lineHeight: "20px"
  mono-small:
    fontFamily: "Geist Mono Variable, ui-monospace, SF Mono, Menlo, monospace"
    fontSize: "11px"
    fontWeight: 400
rounded:
  xs: "2px"
  sm: "4px"
  md: "6px"
  lg: "8px"
  pill: "10px"
spacing:
  xs: "4px"
  sm: "8px"
  md: "12px"
  panel: "14px"
  lg: "16px"
  xl: "20px"
  2xl: "24px"
components:
  button:
    backgroundColor: "{colors.raised}"
    textColor: "{colors.text}"
    rounded: "{rounded.md}"
    padding: "0 12px"
    height: "30px"
  button-hover:
    backgroundColor: "{colors.raised-hover}"
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.on-accent}"
    rounded: "{rounded.md}"
    padding: "0 12px"
    height: "30px"
  button-primary-hover:
    backgroundColor: "{colors.accent-hover}"
  button-ghost:
    textColor: "{colors.text-2}"
    rounded: "{rounded.md}"
    padding: "0 12px"
    height: "30px"
  button-danger-solid:
    backgroundColor: "{colors.danger-solid}"
    textColor: "{colors.on-accent}"
    rounded: "{rounded.md}"
    height: "30px"
  button-danger-solid-hover:
    backgroundColor: "{colors.danger-solid-hover}"
  button-sm:
    rounded: "{rounded.sm}"
    padding: "0 8px"
    height: "24px"
  input:
    backgroundColor: "{colors.ground}"
    textColor: "{colors.text}"
    rounded: "{rounded.md}"
    padding: "0 10px"
    height: "30px"
  panel:
    backgroundColor: "{colors.surface}"
    rounded: "{rounded.lg}"
    padding: "14px 16px 16px"
  panel-header:
    typography: "{typography.title}"
    padding: "0 14px 0 16px"
    height: "42px"
  table-header:
    textColor: "{colors.text-3}"
    typography: "{typography.label}"
    padding: "0 12px"
    height: "32px"
  table-row:
    textColor: "{colors.text}"
    padding: "0 12px"
    height: "40px"
  points-row:
    height: "28px"
  points-row-selected:
    backgroundColor: "{colors.accent-soft}"
    textColor: "{colors.text}"
  pill-ok:
    backgroundColor: "{colors.ok-soft}"
    textColor: "{colors.ok}"
    rounded: "{rounded.pill}"
    padding: "0 8px"
    height: "20px"
  pill-warn:
    backgroundColor: "{colors.warn-soft}"
    textColor: "{colors.warn}"
    rounded: "{rounded.pill}"
    padding: "0 8px"
    height: "20px"
  pill-bad:
    backgroundColor: "{colors.bad-soft}"
    textColor: "{colors.bad}"
    rounded: "{rounded.pill}"
    padding: "0 8px"
    height: "20px"
  flag-warn:
    backgroundColor: "{colors.warn-soft}"
    textColor: "{colors.warn}"
    rounded: "{rounded.sm}"
    padding: "0 7px"
    height: "20px"
  tag:
    backgroundColor: "{colors.raised}"
    textColor: "{colors.text-2}"
    typography: "{typography.mono-small}"
    rounded: "{rounded.sm}"
    padding: "0 6px"
    height: "18px"
  segmented:
    backgroundColor: "{colors.ground}"
    rounded: "{rounded.sm}"
    padding: "1px"
  segmented-option:
    textColor: "{colors.text-3}"
    padding: "0 8px"
    height: "20px"
  segmented-option-on:
    backgroundColor: "{colors.raised}"
    textColor: "{colors.text}"
  tab:
    textColor: "{colors.text-3}"
    padding: "0 10px"
    height: "38px"
  tab-current:
    textColor: "{colors.text}"
  nav-item:
    textColor: "{colors.text-2}"
    rounded: "{rounded.sm}"
    padding: "0 10px"
    height: "30px"
  nav-item-current:
    backgroundColor: "{colors.raised}"
    textColor: "{colors.text}"
  json-pane:
    textColor: "{colors.text-2}"
    typography: "{typography.code}"
    padding: "9px 0"
  toast:
    backgroundColor: "{colors.raised}"
    textColor: "{colors.text}"
    rounded: "{rounded.md}"
    padding: "9px 8px 9px 12px"
  dialog:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text}"
    rounded: "{rounded.lg}"
    padding: "18px 20px 16px"
    width: "460px"
---

# Design System: nocturnum console

## Overview

**Creative North Star: "The Quiet Instrument Panel"**

The console is the category standard played straight, a dark admin frame in the familiar shape of vector database dashboards with a sidebar of collections, a top bar with search and node health, and a main column of hairline panels, and it earns its place through finish and density. Everything is tuned so the operator can read engine state at a glance: tones step up from a near-black ground in small lifts, numbers sit in tabular columns, and the only saturated colours on screen are a single crimson for the thing you chose or the thing you should press, and the three engine signals for healthy, attention and failure.

Density is high and deliberate, with 13px body type, 28px point rows and a full collection overview that fits 1440 by 900 without scrolling. Machine values (ids, field keys, dimensions, versions, segment names, JSON) are set in Geist Mono, and human labels stay in Geist, so the two kinds of text never blur. There are no gradients, no glass and no glow anywhere, depth comes from hairline borders on lifted tones, and the only shadows belong to things that float above the frame.

**Key Characteristics:**
- Near-black ground with panels lifted one small step, separated by 1px hairlines.
- One crimson accent, spent on the primary action and on selection or current state.
- Green, amber and red reserved for engine health, always paired with a word or a dot.
- Geist for interface text, Geist Mono for machine values, tabular figures on every number.
- Compact, information-dense tables with a synced JSON pane as the signature interaction.

## Colors

A cool near-black neutral ramp carrying one crimson accent, three engine signals and a quiet slate family for data.

### Primary
- **Signal Crimson** (accent): the primary button, the selected point row's 2px left marker and soft fill, the current tab's underline, the current sidebar item's 2px marker, checked checkboxes, the chosen ef point on the sweep chart, focus outlines and text selection.
- **Crimson Hover** (accent-hover): the primary button's hover state only.
- **Crimson Ink** (accent-ink): text and caret colour on accent-tinted surfaces, such as the active filter chip and input carets, where the base crimson would be too dark to read.
- **Crimson Wash and Crimson Edge** (accent-soft, accent-line): the 12% selection fill and the 45% border used by selected rows, the focus halo on fields and the active filter chip.

### Secondary
- **Healthy Green** (ok, ok-soft): ready segments, healthy node, ok status pills, collection dots in the sidebar, success toasts.
- **Attention Amber** (warn, warn-soft): anything that needs looking at, a segment over the vacuum threshold, warning flags, a meter past its limit.
- **Failure Red** (bad, bad-soft): errors, failed jobs, destructive text buttons and menu items, invalid field borders.
- **Committed Red** (danger-solid, danger-solid-hover): the filled confirm button inside a destructive dialog, darker than Failure Red so white text stays legible.

### Tertiary
- **Slate Series** (series-strong, series-soft, meter, track): charts and meters, the ef-sweep points, the memory stack bar, budget bars and their empty track.
- **JSON Palette** (json-key, json-string, json-number, json-literal, json-punct, json-gutter): payload syntax colouring, with soft blue keys, sage strings, sand numbers, rose booleans and null (null also italic), dim slate punctuation and a near-invisible line-number gutter. Boolean cells in the points table reuse json-literal so a value looks the same in the table and in the pane.

### Neutral
- **Ground** (ground): the page, input wells and the segmented control well.
- **Surface** (surface): panels, the sidebar, dialogs and the top search field.
- **Raised** (raised, raised-hover): default buttons and their hover fill, toasts, menus, tags, the current nav item and the active segmented option.
- **Hover** (hover): row and nav hover, ghost button hover.
- **Hairlines** (line, line-2, line-3): panel and row dividers, control borders, and hover borders in rising strength.
- **Text ramp** (text, text-2, text-3): primary values, secondary labels and meta, quiet headers and hints.

### Named Rules
**The One Accent Rule.** Crimson marks only the primary action and what is selected, current or focused, and it never decorates, never draws a data series and never signals status.

**The Engine Signal Rule.** Green means healthy, amber means attention and red means failure, each always carried by a word, a dot or an icon alongside the colour, on a soft 11 to 12% wash of the same hue.

**The Slate Data Rule.** Charts and meters draw in the slate family, and a status hue enters a meter only when the measured value itself is the warning.

## Typography

**Display Font:** Geist Variable (with ui-sans-serif, system-ui)
**Body Font:** Geist Variable (with ui-sans-serif, system-ui)
**Label/Mono Font:** Geist Mono Variable (with ui-monospace, SF Mono, Menlo)

**Character:** A crisp, neutral grotesk paired with its own mono, so interface prose and machine values share proportions and sit on one baseline without either shouting. Both families are self-hosted through Fontsource and ship inside the binary.

### Hierarchy
- **Metric** (500, 30px, 34px): the single headline figure of a panel, such as recall on the search quality panel.
- **Headline** (600, 20px, 28px, -0.015em): the page or collection name in the page head, one per view.
- **Stat** (500, 15px, -0.01em): secondary figures in panels (memory held, disk used, latency), with a 12px text-3 unit or denominator beside them; dialog titles and the wordmark share this size at 600.
- **Title** (600, 13px, -0.005em): panel headings, always followed by a quiet count or aside.
- **Body** (400, 13px, 1.45): everything else, table cells, buttons, nav and prose, with prose capped near 52ch in empty states.
- **Label** (500, 12px): table headers, field labels and sidebar section heads in text-3 or text-2, always sentence case.
- **Mono** (400, 12px): ids, field keys, dims, segment names and other machine values inline.
- **Code** (400, 12.5px, 20px): the JSON pane and payload textareas.
- **Mono small** (400, 11px): tags, kbd hints, version numbers and tab counts; this is the smallest size in the ramp.

### Named Rules
**The Tabular Figures Rule.** Tabular figures are set on the body, so every number that can change or be compared lines up in its column and counters and meters never jitter.

**The Machine Voice Rule.** If the engine wrote it (an id, a key, a dtype, a path, a version) it is set in Geist Mono, and if a person reads it as a label it is set in Geist.

**The Sentence Case Rule.** Labels and section heads are quiet 12px sentence case, and the system has no uppercase tracked labels.

## Layout

The app is a two-column grid, a 232px sidebar beside a flexible main column, both filling the dynamic viewport height, with a 48px top bar and a page head (18px top padding, 24px sides, hairline bottom) above a scrolling body padded 20px 24px 24px. Inside the body, panels stack with a 16px gap, the collection overview splits into a flexible left column and a 360 to 468px right column with a 20px gap, and the points browser pairs a flexible table with a sticky 320 to 380px JSON pane at a 16px gap.

Spacing runs on a 4px base with the working steps 4, 8, 12, 14, 16, 20 and 24px; panel bodies pad 14px 16px 16px and panel headers hold a 42px minimum height. Table rows are 40px by default and 28px in the points table, with 12px cell padding and 16px on the outer edges so text aligns with panel headings.

Responsive behaviour is one layout collapsing in steps. At 1240px the overview drops to one column, at 1100px the JSON pane moves under the points table, at 1000px the query and telemetry grids stack, at 900px the sidebar becomes a 264px drawer behind a menu button and the breadcrumbs hide, at 760px the page padding tightens to 16px, at 700px the overview pairs stack and engine health moves above configuration, and at 560px the top bar sheds its kbd hint and secondary health text. Panels that live in varying widths use container queries instead: the configuration panel goes from four columns to two at 639px and to one at 380px, and the segments table drops its least important columns at 640px and 520px.

## Elevation & Depth

The frame is flat and tonal: depth comes from four lifts of the neutral ramp (ground, surface, raised, hover) and 1px hairlines, and nothing that sits in the page casts a shadow. Shadows exist only for layers that float above the frame, the overflow menu, the confirm dialog, toasts and the open mobile drawer, each a single soft dark drop with no colour. Scrims are flat translucent near-black with no blur.

### Shadow Vocabulary
- **Menu** (`box-shadow: 0 12px 32px rgba(0, 0, 0, 0.45)`): dropdown menus anchored to a button.
- **Dialog** (`box-shadow: 0 12px 28px rgba(0, 0, 0, 0.45)`): the confirm dialog, over a `rgba(5, 6, 9, 0.66)` backdrop.
- **Toast** (`box-shadow: 0 8px 24px rgba(0, 0, 0, 0.4)`): bottom-right notices.
- **Drawer** (`box-shadow: 12px 0 32px rgba(0, 0, 0, 0.45)`): the open sidebar drawer under 900px, over a `rgba(5, 6, 9, 0.6)` scrim.
- **Field focus halo** (`box-shadow: 0 0 0 3px` accent-soft): the focus ring on inputs, textareas, selects and the top search, the only halo the system uses.

### Named Rules
**The Floating-Only Shadow Rule.** A shadow means the layer floats above the frame and can be dismissed, so panels, cards and buttons never get one.

**The No Atmosphere Rule.** No gradients, no glass or backdrop blur and no glow, and focus is the only state that draws a ring.

## Shapes

Corners are small and tiered by size: 4px for small controls (small buttons, icon buttons, tags, flags, nav items, the segmented control), 6px for standard controls (buttons, inputs, menus, toasts) and 8px for containers (panels, dialogs). Status pills are fully rounded at 10px on a 20px height, dots are circles of 5 to 6px, and bars and selection markers use 2 to 3px ends. Every border is a single 1px hairline, and the only thicker strokes are the 2px accent markers for selection and current state and the 2px bottom edge on kbd hints.

## Components

### Buttons
Compact and quiet, so the one primary action on a page stands out without effort.
- **Shape:** gently rounded (6px), 30px tall, 12px side padding, a 7px gap before an optional 14px line icon in text-3.
- **Primary:** Signal Crimson fill and border with on-accent white text, one per page head (Create snapshot, Run optimizer).
- **Hover / Focus:** a 120ms colour transition, default buttons lift to raised-hover with a line-3 border, primary moves to Crimson Hover, and keyboard focus draws a 2px crimson outline at 2px offset.
- **Default:** raised fill, line-2 border, text colour.
- **Ghost:** no fill or border, text-2, hover fill on hover.
- **Danger:** Failure Red text on the default shape, washing to bad-soft on hover, and a solid Committed Red variant for the final confirm in a destructive dialog.
- **Small and square:** 24px tall with 4px corners and 12px text for in-panel actions, plus square 30px and 24px icon-only variants; disabled buttons drop to 45% opacity.

### Pills, Flags and Tags
- **Pill:** a 20px fully rounded status lozenge with a 5px dot and 12px medium text on a soft wash of its signal colour (ok, warn, bad, idle), used beside the collection name and in health readouts.
- **Flag:** a squarer 4px-cornered amber or red notice at 20px minimum height, used inline in tables for conditions like a segment over the vacuum threshold.
- **Tag:** an 18px mono chip in raised fill with a line-2 border for index types and similar machine labels.
- **Filter chip:** a 22px crimson-washed chip with Crimson Ink mono text, shown only while a filter is active on the points table.

### Segmented Control
A 1px line-2 well on ground with 4px corners and 1px padding, holding 20px options in text-3 at 12px, and the pressed option takes a raised fill and full text colour. Used for small view switches such as Payload and Vector on the point pane.

### Cards / Containers
- **Corner Style:** 8px.
- **Background:** surface on the ground.
- **Shadow Strategy:** none, per the Floating-Only Shadow Rule.
- **Border:** 1px line.
- **Internal Padding:** a 42px header row (16px left, 14px right) with a 13px semibold title and a 12px text-3 aside pushed right, a hairline under it, then a 14px 16px 16px body.

### Tables
Header cells are 32px, 12px medium text-3, left aligned with numbers right aligned, and body rows are 40px with hairline separators and no stripe. Clickable rows take the hover fill, the in-memory memtable row carries a faint 1.2% white wash to read as provisional, and narrow panels hide low-priority columns by container query.

### Inputs / Fields
- **Style:** 30px wells in ground with a 1px line-2 border and 6px corners, placeholder in text-3, a crimson-ink caret, and mono at 12px for id and filter inputs.
- **Focus:** the border moves to Crimson Edge with a 3px Crimson Wash halo, over 120ms.
- **Error / Disabled:** invalid fields take a red border at 55% strength, with the message on an error line below in Failure Red.
- **Checkbox:** a 14px square with 3px corners, filled crimson with a white tick when checked.

### Navigation
- **Sidebar:** surface panel with a 48px brand row (wordmark at 15px semibold, mono version at right), a Collections group of 30px rows showing a status dot, the name and a text-3 count, then an Engine group of icon links. Rows are text-2, brighten with the hover fill, and the current row takes a raised fill with a 2px crimson marker on the left edge.
- **Top bar:** 48px, breadcrumbs in text-3 with the current crumb in text, a 380px global search on surface with a kbd hint, and a bordered health chip with a dot at the right.
- **Tabs:** 38px text-3 tabs with mono-small counts, the current tab in text with a 2px crimson underline inset 10px from each side.
- **Mobile:** under 900px the sidebar slides in as a 264px drawer over a flat scrim, easing in 220ms.

### Points Table and JSON Pane (signature)
The points table runs 28px rows with mono ids, and the selected row takes Crimson Wash across every cell and a 2px inset crimson bar on its first cell. Keyboard focus adds a crimson-edge hairline above and below the row, and j, k or the arrow keys move the selection while the JSON pane beside it follows. The pane is a panel with a mono header (Point id), the Payload and Vector segmented control and small icon actions, then the JSON in 12.5px Geist Mono on a 20px line with a numbered gutter and the JSON Palette colours, and a hairline footer listing vector details. The table foot is a 32px hairline row with the range on the left and kbd hints with the pager on the right.

### ef-Sweep Chart
The search quality panel leads with recall as a 30px metric, then a slate line chart of recall across ef values: a dashed line-2 reference, a 1.2px slate path, hollow slate point rings on surface fill and mono value labels. The point for the collection's current ef is filled crimson as the selected value, every point is a link that brightens on hover, and three latency figures sit under it in a hairline-divided row.

### Query Modes
The query form leads with a Rank by segmented control (Vector, Text, Hybrid), and each mode shows only its own fields. Hybrid stacks a Vector search and a Text search group, each a 13px semibold heading over a hairline with its own weight and candidates, then the Fusion control, and its results gain a 12px text-3 candidate strip under the panel header, the pool and each search's found of limit in tabular figures, with an amber flag at the right when a short page came from a search that filled its candidate limit. In Text and Hybrid modes a collection without a text index shows a ground-well notice linking to the Overview tab and disables Search.

### Dialogs, Menus and Toasts
The confirm dialog is a 460px surface panel with 8px corners, a 15px semibold title and text-2 body, rising 6px into place over 200ms on the shared ease-out curve. Menus are raised 6px-cornered lists of 30px rows with 4px padding, and toasts sit bottom-right on raised fill with a coloured icon, flying in 8px over 180ms.

### Motion
State changes are 120ms colour and border transitions, overlays move on a single ease-out curve (`cubic-bezier(0.16, 1, 0.3, 1)`), skeletons pulse over 1.6s, and reduced-motion users get every transition and animation cut to near zero.

## Do's and Don'ts

### Do:
- **Do** keep crimson to the primary action plus selected, current and focused states, one primary button per page head.
- **Do** put every status colour next to a word, a dot or an icon, on its own soft wash.
- **Do** set ids, keys, dtypes, versions and JSON in Geist Mono and give every number tabular figures.
- **Do** build depth from the ground, surface, raised and hover steps with 1px hairlines, and keep panels at 8px corners with a 42px header.
- **Do** let panels adapt to their own width with container queries, and collapse the page grid at the shared breakpoints.
- **Do** use the 2px crimson edge marker for any new selectable list, matching the points table and sidebar.

### Don't:
- **Don't** use gradients, glass or backdrop blur, or glow on any surface.
- **Don't** put shadows on panels, buttons or anything that sits in the page; shadows belong to menus, dialogs, toasts and the drawer.
- **Don't** use crimson for data series, decoration or status, and don't use green, amber or red for anything other than health, attention and failure.
- **Don't** add uppercase tracked labels or set text below 11px.
- **Don't** introduce a second accent hue or a light theme without a design decision recorded first.
