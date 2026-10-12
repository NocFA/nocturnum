# Running the server

The `nocturnum` binary serves the REST API in [`docs/api/v1.md`](../api/v1.md) on one port, 6333 by default, on loopback unless told otherwise.

```bash
cargo run --release -p nocturnum-server -- --data ./data
curl -s localhost:6333/readyz
```

It binds first and loads collections after, so `/livez` answers during a long recovery, while `/readyz` and every `/v1` request answer `503` until loading finishes.
On SIGINT or SIGTERM it stops taking connections, lets requests in flight finish and closes every collection before it exits.

## Configuration

Settings come from four places, each overriding the one before:

1. built-in defaults,
2. a TOML file given with `--config` or `NOCTURNUM_CONFIG`,
3. environment variables,
4. command-line flags.

An unknown key in the file or a value that makes no sense stops the server before it binds, with an error naming the line and the key but never the value, and the effective config is logged at startup with keys shown as `(set)`, or printed with `--print-config`.

```toml
[server]
host = "127.0.0.1"          # an IP address, never a hostname
port = 6333
max_body_bytes = 33554432   # 32 MiB, from 1 KiB to 1 GiB
allowed_hosts = []          # Host names accepted while no keys are set

[storage]
path = "./data"
snapshots_path = "/mnt/backup/nocturnum"   # optional, "snapshots" inside path by default

[auth]
admin_key = "..."           # at least 16 visible ASCII characters, no spaces
read_only_key = "..."       # needs admin_key, and must differ from it
allow_anonymous = false     # allows a non-loopback host with no keys

[log]
level = "info"              # a tracing filter, such as "nocturnum_api=debug,info"
```

| Setting | Environment | Flag |
|---|---|---|
| config file | `NOCTURNUM_CONFIG` | `--config` |
| `server.host` | `NOCTURNUM_HOST` | `--host` |
| `server.port` | `NOCTURNUM_PORT` | `--port` |
| `storage.path` | `NOCTURNUM_DATA` | `--data` |
| `storage.snapshots_path` | `NOCTURNUM_SNAPSHOTS` | `--snapshots` |
| `auth.allow_anonymous` | `NOCTURNUM_ALLOW_ANONYMOUS` | `--allow-anonymous`, or `--allow-anonymous=false` to override the file |
| `auth.admin_key` | `NOCTURNUM_ADMIN_KEY` | none |
| `auth.read_only_key` | `NOCTURNUM_READ_ONLY_KEY` | none |
| `log.level` | `NOCTURNUM_LOG` | none |

Keys have no flags, since flags show up in the process list.

## Listening beyond loopback

The server refuses to start on any address but loopback with no admin key, unless `auth.allow_anonymous = true` says that is intended.

To reach it over a tailnet as `http://my-server:6333/`, either set keys and listen on the tailnet address:

```toml
[server]
host = "100.x.y.z"

[auth]
admin_key = "..."
```

or, on a tailnet that only the owner's devices join, go without keys and list the name the server is reached by, since without keys any `Host` but an IP address, `localhost` and the listed names is refused:

```toml
[server]
host = "100.x.y.z"
allowed_hosts = ["my-server", "my-server.your-tailnet.ts.net"]

[auth]
allow_anonymous = true
```

## The data directory

```text
data/
  LOCK                         held while a server runs, so a second one cannot share the directory
  schedules.json               snapshot schedules, replaced whole and synced on every change
  collections/<name>/          one shard per collection, see docs/storage/durability.md
  snapshots/                   or storage.snapshots_path
    LOCK                       held while a server runs, so two servers never prune each other's
    <name>/                    <name>-<milliseconds>.snapshot and its .sha256
```

Snapshots can go on another disk with `storage.snapshots_path`, which keeps a backup when the data disk dies, and it must lie outside `collections/`.
Restoring copies from there into the data directory, so the two can be on different filesystems.
`schedules.json` that cannot be read stops the server at startup, since starting without it would silently stop the backups.

Creating, deleting and restoring a collection each work in a dot-named directory beside the collections and rename it into place, so a collection appears whole or not at all.
Startup removes what a crash left part way, and finishes a restore that had already moved the old copy aside, since the restored copy was complete and checked by then, or puts the old copy back if the new one is missing, syncing the directory before anything is removed.
The next restore of the same collection settles a failed earlier one the same way before it starts, so the only old copy is never deleted.
`startup_settles_what_a_crash_left` in `crates/nocturnum-api/tests/http.rs` and the registry's unit tests cover these, while a power cut in the middle of them rests on directory fsyncs, like the rest of storage.
