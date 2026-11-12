# stemd for iOS

The app is a thin SwiftUI shell over the Rust library in `../crates/stemd-ios`.
Separation, decoding and the HTTP server are Rust; Swift starts the server and
talks to it over loopback, which is also the interface another device uses to
fetch stems.

## Generate the project

The `.xcodeproj` is generated from `project.yml`, so it is not checked in:

```bash
brew install xcodegen    # once
cd ios
xcodegen generate
open StemdiOS.xcodeproj
```

Select a development team in the target's Signing settings. The app targets
iOS 17, which is MLX's Metal floor.

The first build runs `cargo build --release -p stemd-ios --target
aarch64-apple-ios`, which compiles MLX and its Metal kernels; expect several
minutes and a large `target/` afterwards. Xcode's own toolchain is used, so
`xcrun metal` resolves without the `DEVELOPER_DIR` workaround the command line
needs when `xcode-select` points at Command Line Tools.

A post-build phase copies `mlx.metallib` into the app bundle. Without it the
app links and launches, then fails the first time MLX touches the GPU.

## First run

1. The app starts the server and shows **Model files → Download model files**.
   The download is 168 MB for `fast` (672 MB for `balanced`); progress is in the
   same row.
2. **+** opens the file picker. Any format `stemd-audio` can decode is accepted:
   WAV, FLAC, MP3, M4A/AAC, AIFF, Ogg.
3. When a job finishes, each stem has a share button. The share link is the
   phone's LAN address plus the token, and the other device must be on the same
   network. The app shows that address in the Server section.

iOS suspends the app — and with it the server and any running separation — as
soon as it leaves the foreground or the display locks. There is no background
mode for a listener; the app stops the display sleeping while it is in front
(**Keep screen awake**, on by default), and republishes its Bonjour record when
it comes back. An always-on server belongs on the desktop build, or on an iPad
kept in Guided Access with **Auto-Lock → Never**.

## Limitations

- The stem server is reachable from the whole local network by design, and it
  starts without requiring a token so a deck can use it. **Require token** in
  Debug restricts it to clients that send one (the app itself and its share
  links); use it on a network you do not trust. Use `127.0.0.1:0` in the Rust
  config if the server should not be reachable at all.
- `fast` is the default and the only preset that fits comfortably in a phone's
  memory. `quality` needs two artefacts and is slower than realtime on a phone.
- The model download cannot be cancelled yet; a failed one is retried from the
  files that made it.
