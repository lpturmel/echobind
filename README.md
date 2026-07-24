# Echobind

Low-footprint real-time audio and video streaming.

The repository is a Cargo workspace:

- `echobind-core` contains shared configuration, the versioned wire protocol,
  and bounded video fragmentation/reassembly.
- `echobind-cli` contains the current audio CLI and platform integrations. Its
  binary is still named `echobind`.
- `echobind-desktop` is the native `wgpu` desktop application for manually
  approved H.264 screen sharing.

Video capture, encoding, decoding, and presentation are not wired into the CLI
yet. Protocol version 2 reserves packet types for fragmented video frames and
keyframe requests.

## Desktop video MVP

Run the native desktop application:

```sh
cargo run --release -p echobind-desktop
```

On the sharing machine:

1. Select **Create server**.
2. Choose the bind IP and UDP port, then start the server.
3. Accept the viewer when its IP address appears.

On the viewing machine:

1. Select **Connect**.
2. Enter the server's IP address and UDP port.
3. Wait for the server to accept the connection.

The MVP streams the primary display at up to 720p using H.264. On macOS it uses
ScreenCaptureKit `IOSurface` frames directly with a required VideoToolbox
hardware encoder, avoiding CPU pixel readback and color conversion. The
encoder enables real-time, no-frame-reordering, low-latency rate control. If
that path is unavailable, the UI reports the OpenH264 software fallback.

Capture and display queues retain only the newest frame, and incomplete video
frames expire after 120 ms rather than accumulating latency. Windows currently
uses the OpenH264 software path. Desktop audio playback is not wired into this
MVP yet; the existing CLI continues to provide audio streaming.

# Build

```
cargo build --release
```

Add `./target/release/echobind` to your `PATH` variable.


## Server

```
echobind record --port 3013
```


## Client

```
echobind connect --ip X.X.X.X --dest-port 3013 --src-port 3013
```

## Default ports

UDP: 3013

Echobind uses UDP for setup, heartbeat, media, and clipboard synchronization.
The client sends a UDP hello to receive the session config, then sends periodic
UDP pings; either side treats 3 seconds without a UDP response as a disconnect.

Protocol v2 uses 1200-byte media datagrams. Audio frames carry sequence numbers
and presentation timestamps. Encoded video frames are split into independently
identified fragments and reassembled with bounded memory.
