# Echobind

Low-footprint real-time audio and video streaming.

The repository is a Cargo workspace:

- `echobind-core` contains shared configuration, the versioned wire protocol,
  and bounded video fragmentation/reassembly.
- `echobind-cli` contains the current audio CLI and platform integrations. Its
  binary is still named `echobind`.
- `echobind-desktop` is the native `wgpu` desktop application for manually
  approved H.264 screen sharing.

Video remains desktop-only; the CLI continues to provide its audio workflow.

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

The desktop app streams the primary display at native, 720p, or 1080p
resolution and up to 120 FPS. Windows uses DXGI Desktop Duplication, a
four-slot staged D3D11/NVENC pipeline, and GPU scaling. Each acquired desktop
surface is copied into an application-owned BGRA ring before DXGI releases it;
color conversion and NVENC submission then run on a separate worker. macOS uses
ScreenCaptureKit and VideoToolbox for hardware encode, then VideoToolbox NV12
decode and IOSurface-to-Metal presentation without a CPU pixel copy. OpenH264
is retained as a reported fallback.

The low-latency path has no B-frame reordering, uses a one-frame NVENC VBV,
generates IDRs only for startup or recovery, requests UI repaints directly from
the decoder callback, and discards video that spends more than 25 ms in a host
or decode queue. The UI reports capture, encode, send, reassembly, decode,
presentation, RTT, jitter, drop, and loss measurements. The viewer receives
the host's pipeline snapshot once per second and keeps both host and client
health visible while the remote video is fullscreen. On Windows it also reports
the DXGI source rate, accumulated desktop frames, timeouts, pacing and
encoder-slot skips, GPU conversion wait, and D3D/NVENC mutex wait. Opus system
audio is streamed independently and the viewer can select the default or a
named output device.

Standard mode uses 1400-byte UDP datagrams to avoid IP fragmentation. The host
can explicitly enable **Jumbo MTU 9000**; it uses 8192-byte UDP datagrams only
when the desktop client advertises support. Enable it only when every LAN hop,
including both network adapters and switches, is configured for a 9000-byte
MTU.

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

Protocol v2 uses 1400-byte media datagrams by default and negotiates optional
8192-byte video datagrams. Audio frames carry sequence numbers and presentation
timestamps. Encoded video frames are split into independently identified
fragments and reassembled with bounded memory and expiration deadlines.
