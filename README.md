# plugins-example-tiktools

Example and first-party plugins for [TikTools](https://github.com/nglmercer/TikTools-app),
extracted from the TikTools repository with their history. Each directory under
`plugins/` is an independent plugin package; TikTools itself builds, runs and
ships without any of them, and installs them like any other plugin (registry,
`.plugin` file, or development staging).

| Directory | Plugin id | Runtime | Notes |
| --- | --- | --- | --- |
| `plugins/sonicboom` | `sonicboom.server` | process + WebView UI | SonicBoom TTS: Rust backend (`backend/`), Vue UI (`ui/`), shared TS policy (`shared/`) |
| `plugins/audio-process` | `audio.play.process` | process | Minimal Rust process plugin |
| `plugins/event-gateway` | `tiktools.event-gateway` | process | Loopback HTTP/WebSocket gateway; serves the TikTools OBS widgets |
| `plugins/textintel` | `textintel` | process | Pre-filter event processor (local text intelligence) |
| `plugins/hotkeys` | `hotkeys` | napi-vm | Global hotkeys through the `rdev-node` native addon |
| `plugins/minecraft-server` | `minecraft.server` | declarative | Minecraft server actions |
| `plugins/peak-game` | `peak.game` | declarative | PEAK game integration |
| `plugins/repo-game` | `repo.game` | declarative | R.E.P.O. game integration |

`examples/` holds sample rule profiles and templates for these plugins, plus
`sonicboom-server-declarative`, the earlier manifest-only SonicBoom package kept
for reference (same id as `plugins/sonicboom`, so it is not staged).

Plugin ids, action ids, manifests, versions and settings are unchanged from the
TikTools tree they came from, so existing installations and saved settings keep
working.

## Build and test

```bash
bun install
bun run typecheck            # SonicBoom UI + hotkeys guest
bun run test                 # TypeScript unit tests
bun run build:sonicboom-ui   # plugins/sonicboom/ui/dist
cargo test --workspace --locked
bun scripts/check.ts         # manifest ids / versions / runtimes
```

Rust plugins depend only on the public `tiktools-plugin-sdk` / `tiktools-plugin-api`
crates, pinned to a TikTools revision in the root `Cargo.toml`. The plugin UI
uses the public `@tiktools/plugin-ui` broker client; it is not on npm yet, so a
pinned copy lives in `vendor/tiktools-plugin-ui` (refresh with
`bun run sync-sdk`). The Event Gateway vendors the subset of the Universal
Widget SDK it compiles (`plugins/event-gateway/vendor/universal`, with the same
SHA-256 pins TikTools uses).

## Developing with TikTools

Clone the repositories side by side:

```text
workspace/
├── TikTools-app/
├── RemakeBackend/              # optional
└── plugins-example-tiktools/
```

`bun run start` in TikTools then builds every package here and stages it under
TikTools' `.dev-plugins` (loaded through `TIKTOOLS_DEV_PLUGINS_DIR`). Point at
another checkout with `TIKTOOLS_PLUGINS_REPO=/path/to/plugins-example-tiktools`,
or disable with `TIKTOOLS_PLUGINS_REPO=off`. Distributable archives:

```bash
# from the TikTools checkout
bun scripts/task.ts plugins --all            # or --plugin event-gateway
```

The Event Gateway serves TikTools' OBS widgets (`src/widgets` in TikTools);
staging and packaging build them in TikTools and copy them into the package's
`dist/widgets/`.

### Developing against a local TikTools checkout

To build the Rust plugins against unmerged SDK changes, override the git
dependency without editing `Cargo.toml`:

```bash
cargo build --workspace \
  --config 'patch."https://github.com/nglmercer/TikTools-app".tiktools-plugin-sdk.path="../TikTools-app/crates/tiktools-plugin-sdk"' \
  --config 'patch."https://github.com/nglmercer/TikTools-app".tiktools-plugin-api.path="../TikTools-app/crates/tiktools-plugin-api"'
```

The SonicBoom host/plugin interop suite (`plugins/sonicboom/ui/tests/interop.spec.ts`)
bundles the real TikTools WebView host shim; it runs when `TIKTOOLS_APP_DIR`
(or a sibling `TikTools-app` checkout) is present and is skipped otherwise.

## SonicBoom health strategy

`plugins/sonicboom/plugin.json` keeps `"strategy": "sonicboom"` so it still
works on already released TikTools versions. TikTools now implements this as
the generic `service-info` strategy (the old name is an alias) and reads the
protected probe from `authProbePath`, which the manifest declares explicitly.

## License

MIT
