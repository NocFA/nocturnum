# Contributing

nocturnum is a personal project, built first for one homelab and shared in the hope it helps other self-hosters, so issues get read when there is time and changes land at the pace of one person.

The main repository lives at [git.nocfa.net/NocFA/nocturnum](https://git.nocfa.net/NocFA/nocturnum), with a read-only mirror at [github.com/NocFA/nocturnum](https://github.com/NocFA/nocturnum) where issues are open.

## Before you write code

Open an issue first for anything bigger than a typo, so we can agree on the shape before you spend time on it, since pull requests on the GitHub mirror get carried over by hand rather than merged there.

Releases are 0.x, so the API and the on-disk format can still change between them, each change with a note on how to move your data.

## Building and testing

```bash
npm --prefix console ci && npm --prefix console run build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Anything the docs promise about durability needs a crash or fault-injection test behind it, and [`docs/storage/durability.md`](docs/storage/durability.md) lists the ones that exist.

## Writing code here

nocturnum's code is written from scratch, so please never paste or closely translate code from Qdrant or any other vector database, even where the licence would allow it.

By contributing you agree your work is licensed under the Apache License, Version 2.0, the same as the rest of the project.
