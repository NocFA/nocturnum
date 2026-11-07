# Security

Please report a vulnerability privately through GitHub's private vulnerability reporting on [github.com/NocFA/nocturnum](https://github.com/NocFA/nocturnum/security/advisories/new), which keeps it out of view until a fix is out.

A useful report says which version or commit you ran, how the server was configured (keys set or not, which address it listened on) and the steps that show the problem.

nocturnum is a personal project in its 0.x releases, so fixes land on `main` and in the next release, with no older release lines kept up to date.

## What the server assumes

- It listens on loopback by default, and refuses any other address without an admin key unless `auth.allow_anonymous = true` says that is intended.
- It speaks plain HTTP, so keys cross the network as readable text, and anything beyond loopback belongs on an encrypted network such as a tailnet or behind a TLS proxy.
- It creates its data, WAL and snapshot files readable by its own user only.

[`docs/server/config.md`](docs/server/config.md) covers these settings in full.
