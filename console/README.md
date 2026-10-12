# nocturnum console

The web console nocturnum serves on its API port, a SvelteKit app built to static files and embedded in the binary.

## Building

```bash
npm ci
npm run build      # writes build/, which the server embeds
npm run check      # type-checks every component
```

A release build of the server carries `build/` inside the binary, so build the console first.
A debug server reads `build/` from disk as it is asked for, so a fresh build shows up on reload.

## Developing

```bash
npm run dev
```

Vite serves the app with hot reload and sends `/v1`, `/metrics`, `/livez` and `/readyz` to a nocturnum server, `http://127.0.0.1:6333` unless `NOCTURNUM_URL` names another one.

## How it fits together

- `src/lib/api.ts` is the REST client, typed from `docs/api/v1.md`, and keeps the key in the browser's local storage, sending it as `Authorization: Bearer`, never in a URL.
- `src/lib/state.svelte.ts` holds what every page shares: the server, the collection list, the key and the top search.
- `src/lib/search.ts` turns the top search into a filter: an id, `field:value`, `field>n` or plain words.
- `src/lib/components/PointsBrowser.svelte` is the points table and JSON pane that move together with `j` and `k`.
- `src/routes/` has one folder per page, with a collection's tabs under `collections/[name]/`.

The look follows the direction in `.impeccable/surfaces/console.md`, with tokens in `src/app.css`.

Every screen shows the engine's own numbers, and anything made up for a demo has to say so.
