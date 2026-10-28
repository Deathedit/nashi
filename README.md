# nashi ナシ

Drag & drop an ISO into a web page, get back an emulator-ready file:

| Platform | Output | Tools | Plays in |
|---|---|---|---|
| PS2 | `.chd` | `chdman createdvd` | PCSX2 |
| Xbox 360 | `.zar` | `extract-xiso` → `zarchive` | Xenia Canary |

The platform is auto-detected (Xbox `MICROSOFT*XBOX*MEDIA` header, or ISO9660 with a
`SYSTEM.CNF` containing `BOOT2` for PS2). Picking a platform in the UI only lets
that platform through; an ISO that is detected as anything else is rejected.

## Run locally (nix)

```sh
nix-shell
cargo run --release
# open http://localhost:8080
```

## Run with docker compose

Uses the image CI publishes to `ghcr.io/deathedit/nashi`.

```sh
docker compose pull && docker compose up -d
# open http://localhost:8080
```

To build locally instead: `docker build -t ghcr.io/deathedit/nashi:latest .`

## Config (env vars)

| Var | Default | |
|---|---|---|
| `NASHI_ADDR` | `127.0.0.1:8080` | listen address |
| `NASHI_WORK_DIR` | `./work` | uploads + outputs (needs ~2× your biggest ISO free) |
| `NASHI_WEB_DIR` | `./web` | static frontend |
| `NASHI_MAX_JOBS` | `1` | conversions running at once |
| `NASHI_TTL_SECS` | `3600` | finished jobs are deleted after this long without a download |

There's no auth, so don't expose it to the internet.

## API

- `PUT /api/jobs`: raw ISO body. Headers: `X-Filename` (URI-encoded), `X-Platform: auto|ps2|x360`.
- `GET /api/jobs`, `GET /api/jobs/{id}`: job state.
- `GET /api/jobs/{id}/events`: server-sent events with live progress.
- `GET /api/jobs/{id}/download`: the `.chd` / `.zar` file.
- `DELETE /api/jobs/{id}`: cancel a job, or delete its output.
