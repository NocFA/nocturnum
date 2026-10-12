---
version: 1
slug: "console"
primary_target: "console"
related_targets: []
---

# Surface: nocturnum console

Scope: the whole web console served by the nocturnum binary (collections, collection detail, points, query, search quality, snapshots, keys, later cluster and tenants). Visitor mode: **Operate**.

Audience and job: a self-hoster at a desktop browser over a tailnet, checking what is stored and how the engine is doing, running and tuning queries, taking snapshots and managing keys. Later, other self-hosters.

Constraints: PRODUCT.md applies (one binary, real engine state, small hardware, writing rule). Demo data is synthetic and labelled. No benchmark or adoption claims.

## Direction contract

THESIS: The category standard played straight, a dark admin console in the familiar shape of vector database dashboards, chosen after comparing four rendered directions. It refuses nothing about the familiar arrangement and wins on finish: it must match Qdrant's own dashboard on every job it does and beat it on density, clarity of engine state and craft.

OWN-WORLD: Near-black ground #0b0d12 with slightly lifted panels, text #e5e7eb with two quieter greys, one accent #dc244c used for the primary action and selection only, amber for warnings such as a segment over the vacuum threshold, green for healthy. A high-quality UI sans (Geist, Onest or Figtree, never Inter) with a real mono for JSON, ids and values, tabular numerals everywhere. Hairline 1px borders, 6px radii, no glow, no gradients, no glass, themed scrollbars, selection and focus rings.

STORY: The operator opens a collection and sees at once whether it is healthy, how big it is, what is indexed and what needs attention, then drills into points, payloads and queries without leaving the frame, and trusts the numbers because they come straight from the engine.

FIRST VIEWPORT: Left sidebar with collections (name, point count) and engine links (Cluster, Snapshots, Telemetry, Logs, API keys); top bar with global search over collections, point ids and payload, node health and the version. Main area: collection header (name, status pill, point count, vector spec, created date) with Run optimizer and Create snapshot as the primary actions, a tab strip (Overview, Points, Query, Snapshots). Overview splits into a left column of configuration, a segments table with state and vacuum flags, RAM and disk budget bars and search quality, and a right column with the points table and the selected point's syntax-coloured JSON payload. Everything fits 1440x900 without scrolling. Reference: .impeccable/previews/standard.html.

FORM: canon (the standing door, category standard), chosen after previews of the roll (Component Datasheet, position 4 of the grounded list), Impeccable's pick (Survey Plate) and the Tuning Specimen challenger. Craft reference: Qdrant's own dashboard (qdrant-web-ui), match its coverage, beat its finish. Signature interaction: the points table and JSON pane stay in sync with keyboard navigation (j/k or arrows move the selection, Enter opens the point), and filters typed in the top search apply live to the table.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance

## Build decisions (2026-10-12)

- Search quality is an ef sweep measured by the engine against exact search, each ef column linking to the Query tab with that ef filled in, which settles the Tuning Specimen question inside this world.
- The sidebar shows only what the engine backs (Collections, Snapshots, Telemetry, API keys), and Cluster and Logs return once clustering and a log endpoint exist.
- The header has no Run optimizer action because the optimizer runs on its own and has no trigger endpoint, and no created date because the engine records none, so segment count and disk size stand in.
- The finish review shipped the 1440x900 Overview, Points, home and Snapshots views, with Query, Telemetry and API keys never captured for it.

## Unresolved

- Ceiling left open by the finish review: budget bars and attention summaries on the collections home, and inline filter and sort controls on the Points tab beside the top search.
- Light theme: none planned, decide only if asked.
