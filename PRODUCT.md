# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

Delegated: the engine is Rust, one static binary that serves its own web console. The console is a SvelteKit app built with the static adapter and embedded into the binary at compile time, so a self-hoster runs one file and gets the API and the UI on the same port with nothing else to install. SvelteKit was picked because it ships small bundles for a data-heavy tool and its build tooling is well known.

## Users

Self-hosters first, running nocturnum in a homelab as the vector store behind their own projects and agents, usually from a browser on another machine over a tailnet.

Other self-hosters are welcome from the first public 0.x release, while the API and on-disk format can still change: developers and hobbyists who want a vector database they can run at home or on one small server, without a cloud account or a licence split between an open core and paid features.

The console's jobs: look at what is stored (collections, points, payloads, vectors), run and tune queries, check that search quality is right, see how the engine is doing (segments, indexing, memory, disk), take and restore snapshots, and manage keys and tenants.

## Product Purpose

nocturnum is a vector database written in Rust that does what Qdrant does, storing vectors with JSON payloads, searching them by similarity with filters and staying fast as the data grows, as an independent implementation written after studying Qdrant's Apache-2.0 source.

It exists so the people who self-host get the whole thing, everything Qdrant keeps for its cloud included (backups on a schedule, monitoring, access control, the management UI), in one open project they can run and change.

Success for v1 is moving real vector workloads onto it and trusting it with them.

## Positioning

Fully open, self-host first: no hosted tier, no enterprise build, no feature that only exists in someone's cloud. The console is part of the binary rather than a separate download.

## Operating Context

- Runs on homelab hardware, such as a small Mac, a Linux box or a VM with 8 GB of RAM, so memory and disk use matter more than peak throughput on big servers.
- Reached over a tailnet by hostname, viewed mostly from a desktop browser, sometimes a laptop or phone.
- Fed by scripts and agents through the API, the console is where a human checks and steers.

## Capabilities and Constraints

- Research on how Qdrant works lives in `docs/research/` and `docs/how-qdrant-works.md`.
- v1 scope is recorded in `docs/decisions/0001-v1-scope.md`, decided on 2026-10-10.
- nocturnum has its own API, with familiar concepts but no Qdrant wire compatibility, so Qdrant clients do not work unchanged, REST first and gRPC over the same operations after.
- v1 is single-node, with shard ids already in the on-disk format so clustering can come later.
- Writes are acknowledged once durable (fsynced) by default, with `applied` and `accepted` as faster opt-in levels, each one crash-tested.
- REST and the console listen on 6333, with 6334 kept for gRPC once it lands, loopback only unless configured.
- Licence: Apache-2.0, the same as Qdrant and most of the Rust ecosystem, with the full text in `LICENSE`.
- Independent implementation: Qdrant's Apache-2.0 source was read to understand how it works, and nocturnum's code is written from scratch, never copied or translated from it.

## Brand Commitments

- The name is **nocturnum**, lowercase in prose and in the UI.
- The console follows the category standard, a dark admin console in the familiar shape of vector database dashboards, chosen on 2026-10-10 after comparing four rendered directions (`.impeccable/previews/`). Qdrant's dashboard is the craft reference: match its coverage and beat its finish.
- One writing rule applies to every string: related clauses joined with commas or conjunctions, never two short sentences side by side on one line, no em dashes.

## Evidence on Hand

None yet: no benchmarks, users or deployments exist, so the console and any docs must not claim speed, scale or adoption numbers. Demo data in designs is synthetic and labelled as such.

## Product Principles

1. One binary, one port, nothing else to run.
2. Everything a self-hoster needs is in the open project, nothing held back for a paid tier.
3. Show the engine's real state, segments, indexing progress, memory and recall, rather than hiding it behind a friendly summary.
4. Small hardware is the main target, so defaults favour low memory and disk-backed storage.
