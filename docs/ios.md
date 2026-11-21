# iOS

The iOS app is a second binary with its own crate, not a port of the desktop
server. It links `stemd-core` directly, so the separation pipeline, the model
graph and the stem arithmetic are the same code the desktop runs; what differs
is the surface around them.

```text
crates/stemd-ios     static library: HTTP API, job store, FFI
crates/stemd-core    the pipeline, shared
crates/stemd-audio   decoding, shared
crates/stemd-models  preset catalogue and digests, shared
crates/stemd-server  the desktop server, unchanged
```

Nothing in `stemd-server` is compiled into the app, and nothing in this crate
changes the desktop. The one shared change is that the model catalogue moved
from `stemd-server/src/models` into `stemd-models`, because two copies of the
artefact names and digests would eventually disagree about `model_id`.

## Why the API differs

The app has a file, not samples, so its own spelling of `POST /v1/jobs` takes
the encoded file and `stemd-audio` decodes it. That keeps one decoder in the
path and avoids a full-size copy through Swift.

The desktop spelling is also accepted — `format=f32le|s16le` with
`sample_rate`/`channels`, and a raw interleaved body — because the CDJ sidecar
inside `cdj3k-emu` speaks that and cannot be changed without a guest-side
release. See [Desktop compatibility](#desktop-compatibility).

The other differences follow from the app being the library:

- every finished job keeps all three stems (`drums` is written from
  `mix − harmonics − vocals`), because a retrieved folder has no mix to rebuild
  it from;
- there is no separate cache store: a job's directory is its cache. A
  submission whose content key matches an existing job is answered with that
  job rather than separated twice; the app decides when to delete entries.
- `flac`, `wav`, `pcm16` and `pcm32` outputs are offered. MP3 is a lossy format
  that is wrong for stems, and its encoder is LGPL, so the app does not build
  it.

## Desktop compatibility

The `stemd_client` sidecar in `cdj3k-emu` makes five calls and gates every one
of them on a specific shape:

- `GET /v1/health` must report `sample_rate: 44100`, `channels: 2`, and
  `harmonics`/`vocals` in `stems`; it uses `model_id` to scope the deck's cache.
- `POST /v1/jobs?sample_rate=…&channels=…&format=f32le&output_format=flac` with
  a streamed raw PCM body.
- `GET /v1/jobs/{id}` must have top-level `stage` and `completed`/`total`, with
  terminal stages `done` and `failed`; the result carries `sample_rate` and
  `frames`.
- `GET /v1/jobs/{id}/stems/harmonics` and `.../vocals` with no token.
- `DELETE /v1/jobs/{id}` with no token.

The iOS server now provides all of it:

| Need | How |
| --- | --- |
| Discovery | Bonjour advertises `_stemd._tcp` with the system responder. This is allowed without the restricted multicast entitlement; it needs `NSBonjourServices` and the local-network permission. |
| Health | The desktop fields are included alongside the app's own (`model`, `model_id`, `backend`, `device`, `sample_rate`, `channels`, `stems`). |
| Raw PCM | Accepted when `format` is `f32le`/`s16le` (or `pcm16`/`pcm32`); `sample_rate` and `channels` come from the query. `output_format` selects the container. |
| Job document | `stage`, `completed`, `total`, `fraction`, and after completion `sample_rate`, `channels`, `frames`, `format`. |
| No token | The server starts without requiring one, because the sidecar cannot send it. **Require token** in Settings turns it on, and the choice is remembered. |
| Stems | All three are written; the sidecar fetches the two it plays and rebuilds drums itself. |

`output_sample_rate` and `dsp_mode` are accepted and currently ignored: the
model runs at 44.1 kHz and the stems ship at that rate, so the deck resamples
them. That works and costs load time, not correctness. Serving 96 kHz directly
would need the desktop's output-resampling path.

> [!WARNING]
>
> The app starts open so a deck can use it: every endpoint is reachable by any
> device on the network. Turn **Require token** on in Settings when the network is
> not one you trust; the choice survives a relaunch.

## HTTP API

All responses are JSON except the stems.

| Request | Result |
| --- | --- |
| `GET /v1/health` | app fields plus the desktop compatibility fields |
| `POST /v1/jobs?title=&artist=&album=&filename=&format=` | `202` with the job; `200` with an existing one on a content hit; the body is the encoded file |
| `POST /v1/jobs?sample_rate=&channels=&format=f32le&output_format=flac` | `202` with the job; `200` on a content hit; the body is raw PCM (desktop spelling) |
| `GET /v1/jobs/lookup?source_hash=&format=` | `200` with the job already made from that source; `404` when there is none |
| `GET /v1/jobs` | every job, newest first |
| `GET /v1/jobs/{id}` | one job: state, progress, stems, and the desktop `stage`/counters |
| `DELETE /v1/jobs/{id}` | stop a running job between model segments |
| `DELETE /v1/jobs/{id}/cache` | remove one finished job and its stems |
| `DELETE /v1/jobs` | delete every separation and its stems; model files stay |
| `GET /v1/jobs/{id}/stems/{name}` | `audio/flac`, `audio/wav` or raw PCM |
| `POST /v1/models/download` | fetch the preset's weights; progress under `models` in health |
| `GET /v1/server/requests` | the last 200 requests the Rust listener answered |

`filename` is used only as the decoder's extension hint. In the file spelling,
`format` names the **output** container; in the raw spelling it names the
**input** encoding, and `output_format` names the output. The upload limit is
512 MB and the default length limit is 15 minutes, both because phone memory
grows with track length.

A submission is reused when an existing job has the same **content key**: the
uploaded bytes, the output container, the model, and — for raw PCM — the input
format, sample rate and channel count. `title`, `artist` and `album` are
annotations and are never part of it. The same recording in a different
container, or uploaded as an encoded file instead of raw PCM, is a different
key. Jobs written before the key existed cannot be matched.

A client that already has the file can ask **before** uploading. `GET
/v1/jobs/lookup?source_hash=&format=` returns the job whose source has that
identity and the same output format, or `404`; the deck's sidecar uses it and
skips the PCM upload on a hit. `source_hash` is 16 lowercase hex characters:
a 64-bit FNV-1a variant over the file size (as a little-endian `u64`), then the
first 64 KiB and the last 64 KiB, so it is cheap on a phone and identical
across clients. (The offset basis is the deck's own, not the textbook one;
the point is that every client agrees.)
`POST /v1/jobs` accepts the same `source_hash=` parameter and records it in
`detail.json`; a job created without one is still matched by its content key,
but cannot answer a lookup.

A job:

```json
{
  "id": "a8e29ff15c16-0",
  "created": 0,
  "title": "Track",
  "state": "running",
  "progress": { "stage": "separating", "completed": 12, "total": 34, "fraction": 0.41 },
  "stage": "separating",
  "completed": 12,
  "total": 34,
  "fraction": 0.41,
  "stems": []
}
```

When it finishes, `state` is `done` with `seconds` and `realtime`, `stage` is
`done`, and the result adds `sample_rate`, `channels`, `frames` and `format`.
`stems` lists `harmonics`, `vocals` and `drums`, each with a `url`, byte count,
gain and content type. The gain is also in `detail.json` beside the files: a
stem that peaks past full scale is scaled to fit and the factor is recorded, so
a player can restore the level.

A failed job reports `{"state":"failed","error":"..."}`. A cancelled one is
`{"state":"cancelled"}`, which is not a failure.

The `GET /v1/server/requests` log is also what the in-app Settings screen shows:
each entry has `method`, `path`, `status`, `millis`, `remote` and `loopback`,
so the app's own polling can be separated from a deck on the LAN.

## FFI

```c
char *stemd_ios_start(const char *config_json); // {"port":N,"token":"..."} or {"error":"..."}
void  stemd_ios_stop(void);
void  stemd_ios_free_string(char *s);
void  stemd_ios_init_logging(void);              // optional; start does it too
void  stemd_ios_set_token_required(int required);
void  stemd_ios_set_save_pcm(int save);
```

When `require_token` is on, every request except `GET /v1/health` needs the
token, as `Authorization: Bearer <token>` or `?token=<token>`. The query
spelling is what the stem URLs carry, so a player can be handed one without
knowing the header convention. The app passes `require_token: false` because the
CDJ sidecar cannot send one; an embedder that omits the field gets the server's
default, which is to require it. Omit `token` in the config and one is generated
and returned either way.

The config is JSON:

```json
{
  "data_dir": "/var/mobile/.../Library/Application Support/stemd",
  "models_dir": "/var/mobile/.../Library/Application Support/stemd/models",
  "preset": "fast",
  "bind": "0.0.0.0:0",
  "max_track_secs": 900,
  "pcm_dir": "/var/mobile/.../Documents/stemd-pcm",
  "save_pcm": false
}
```

`save_pcm` writes the pre-separation mix (after any resampling to the model's
rate) to `pcm_dir/<job>-<title>-original.wav`. That is the reference the stems
sum back to, so it is what a playback defect is compared against; it is off by
default because it doubles the storage each track uses. The runtime setter
applies from the next job. Undo each stem's `gain` before comparing: the API
documents `sample * 1/gain`, and a client that skips it plays hot stems too
quietly.

`models_dir` defaults to `data_dir/models`. `bind` defaults to every interface
on an ephemeral port, because the stems are meant to be retrieved by another
device; use `127.0.0.1:0` for a loopback-only build (the UI can still use it).
An ephemeral port is reported back because 8420 may be taken.

Swift:

```swift
let result = stemd_ios_start(configJSON)!
let reply = String(cString: result)
stemd_ios_free_string(result)
```

The server and the separation worker live until `stemd_ios_stop`, or until iOS
suspends the app. The UI should hold the idle timer off while a job runs and
while it is serving a finished one.

## Models

The app expects `<models_dir>/<artefact>.safetensors`:

| preset | artefacts |
| --- | --- |
| `fast` | `htdemucs` |
| `balanced` | `htdemucs_ft` |
| `quality` | BS PolarFormer and `htdemucs_ft` |

Loading happens on the first job, not at start, so the app can open and show its
library immediately. A missing or wrong file fails that job with the tensor or
path that disagreed; it does not crash the process.

The downloader is `/v1/models/download`. It fetches the preset's pinned
artefacts into `models_dir`, verifying each as it is written, and reports
`file`, `done` and `total` under `models` in `/v1/health`. Files already present
are skipped, so retrying after a failed download does not start from zero for
the parts that landed; a `.part` file from an interrupted file is fetched again.

`stemd-models` owns the URLs and digests. The desktop and the app therefore
download the same bytes and can never disagree about what an artefact is.

## Building

Needs Xcode, not just the Command Line Tools: MLX compiles Metal shaders with
`xcrun metal`, which CLT does not ship. If `xcode-select -p` names Command Line
Tools, either select Xcode or set `DEVELOPER_DIR` for the commands below.

The iOS Metal fix lives in the vendored `mlx-sys` build script: it gives CMake
an `iOS`-aware copy of MLX only when the target is iOS, so the macOS build still
uses the stock tree. Stock MLX v0.31.2 keys its Metal backend on
`CMAKE_SYSTEM_NAME MATCHES "Darwin"`, which an iOS cross-build is not, and
silently produces a CPU-only library.

```bash
rustup target add aarch64-apple-ios
IPHONEOS_DEPLOYMENT_TARGET=17.0 \
  DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  cargo build --release -p stemd-ios --target aarch64-apple-ios
```

`IPHONEOS_DEPLOYMENT_TARGET` is not optional on the command line. MLX's own
build script sets it for CMake, but `ring` and LAME are built by `cc`, which
reads the environment directly; without it they use the SDK's default floor
(iOS 26.4 with Xcode 26) and every object warns when the app is linked for 17.
Xcode exports the variable to build phases, so the pre-build script in
`ios/project.yml` does not need it.

The static library lands at
`target/aarch64-apple-ios/release/libstemd_ios.a`. The Metal kernels are not in
it: copy the `mlx.metallib` to the root of the app bundle, beside the
executable (MLX looks there first):

```bash
cp target/aarch64-apple-ios/release/build/mlx-sys-*/out/build/lib/mlx.metallib \
   "$APP_BUNDLE/mlx.metallib"
```

Link `libstemd_ios.a` plus `libc++` and the frameworks MLX records privately and
a static archive cannot carry: `Metal`, `Foundation`, `Accelerate` and
`QuartzCore`. `Security` is needed too, because the model download uses
`rustls-platform-verifier`, which validates against the system trust store.

The app needs `NSLocalNetworkUsageDescription`; without it iOS blocks the
listener's visibility to other devices and prompts on first use. Add
`NSBonjourServices` only if the app advertises itself; this crate does not.

**Simulator builds are CPU-only.** MLX's Metal backend segfaults inside the
simulator's Metal service, so the vendored build script turns Metal off for
`aarch64-apple-ios-sim`. The app, its HTTP API and the model all work there;
separation is far slower, which is acceptable for the UI and integration tests.
Device builds keep the Metal backend.

## Known limits

- **Jobs persist until cleared.** A finished job's stems and
  `detail.json` live under `data_dir/jobs/<id>/`; the list is rebuilt from them
  at launch, so separations survive a restart. Nothing removes them
  automatically; each row's **Remove from cache** (or
  `DELETE /v1/jobs/{id}/cache`) deletes one entry, and the toolbar **trash**
  (or `DELETE /v1/jobs`) deletes every separation and frees the space. Model
  files are elsewhere and stay.
- **Foreground only.** iOS suspends the process when the app leaves the front,
  and the listener goes with it. There is no background mode for a
  general-purpose server: the audio and VoIP modes exist for audio and calls,
  and abusing them is a review rejection and a battery problem. The app offers
  **Keep screen awake** (on by default) so an auto-lock does not suspend it, and
  republishes Bonjour when it returns to the front. Returning to the front also
  probes the listener on loopback: if the process was only suspended it is still
  there, and if it is not, the app rebinds it. Three failed health polls while
  the app is in front replace the Server section with **The server is not
  running** and a **Restart the server** button. For an always-on server, run
  the desktop build on a machine that stays awake, or keep the iPad in Guided
  Access with **Auto-Lock → Never**.
- **Long tracks are separated in two-minute windows.** On the device the whole
  track is never one array: it is cut into 120-second windows with a two-second
  crossfade at each seam. This is a memory decision, not a quality one — the
  frame count and the stems are unchanged. An eight-minute track's whole-track
  arrays are what killed the app; a window is a size the device has always
  handled. The same reason caps MLX's allocator cache at 1 GiB and clears it
  after every window: its default is the GPU's recommended working set (over
  7 GB here), which is more than the app is allowed to hold.
- **Memory.** The pipeline holds the decoded track and the stem accumulators as
  f32. On an 8 GB iPad a two-minute window peaks around 2.5 GB of MLX memory
  and releases to zero between windows. `/v1/health` reports
  `available_memory`, `mlx_cached` and `mlx_limit`; each job also appends a
  flushed phase trace to `data_dir/jobs/debug-memory.log`, which is what to ask
  for when a separation takes the process down without a crash report.
- **No ANE.** MLX runs on the GPU. A Core ML conversion would be a different
  project and would have to reproduce every layer's checks.
- **MP3 is not built.** The app depends on `stemd-core` with default features
  off, so the LAME encoder is not in the binary and the LGPL obligation does not
  apply to an App Store build. The format table still names `mp3`; asking for it
  returns `400`, and the encoder would return an error if a caller reached it
  another way.
