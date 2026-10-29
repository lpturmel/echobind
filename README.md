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
rate-sized four-to-twelve-slot asynchronous NVENC pipeline, and GPU scaling.
Capture textures and video-processor views are allocated before pacing and
stall detection begin. Acquired desktop surfaces are copied into the
application-owned BGRA ring and converted to NV12 with the D3D11 video
processor. Automatic video enhancements are disabled for desktop frames.
NVENC resource mapping supplies the documented D3D11 completion
synchronization, avoiding redundant GPU query waits and per-frame context
flushes. NVENC submission stays on a non-blocking producer thread while a
dedicated output thread waits for events and copies completed bitstreams in
submission order.

Only two frames may remain in the copy/convert/submit stages, while the deeper
ring retains frames already accepted by NVENC. Prepared and converted frames
older than 50 ms (two frame intervals below 40 FPS) are discarded, queued
desktop samples are coalesced before conversion, and conversion is limited to
one frame of work-ahead. A loaded GPU can hold the frame being mapped for
longer than two 120 FPS intervals, so a frame-relative limit would discard the
next frame on every cycle. GPU overload therefore cannot flood the graphics queue with stale
full-screen copies or scaler work. A transient `NV_ENC_ERR_ENCODER_BUSY`
response is retried only within the frame's latency budget instead of tearing
down the stream; a genuinely stalled session is rebuilt with a bounded retry
count. Capture waits for an active viewer before creating its duplication
session and recreates it after reconnect, so a static desktop always supplies a
bootstrap surface. Every acquired desktop update carries a session-local
generation through post-NVENC completion; if the final update is discarded
before the desktop becomes static, the host refreshes the current surface with
non-fatal bounded backoff. A pending decoder keyframe request or failed video
send uses the same refresh path when an unchanged desktop has no new bitmap to
encode. Cursor-only acquisition can provide the initial desktop surface, while
pointer traffic itself is coalesced off the highest-priority capture thread.
Device-removal status from D3D11 or NVENC triggers a fresh device/session retry.
The Windows host requests realtime GPU scheduling priority and falls back to
high when it isn't elevated. With hardware-accelerated GPU scheduling and an
uncapped game saturating the GPU, high priority left every NVENC frame waiting
about 190 ms and capped the stream near 10 FPS; realtime kept 120 FPS with a
4 ms encode. Set `ECHOBIND_GPU_REALTIME=0` to stay at high priority.
macOS uses ScreenCaptureKit and a four-frame
asynchronous VideoToolbox pipeline for hardware encode.
`VTCompressionSessionCompleteFrames` is used only for final drain rather than
serializing every frame. The client uses VideoToolbox NV12 decode and
IOSurface-to-Metal presentation without a CPU pixel copy. OpenH264 is retained
as a reported fallback.

The low-latency path has no B-frame reordering, uses one H.264 slice per picture
and a one-frame NVENC VBV (recovery IDRs may use twice the P-frame budget so
they do not blur the picture), generates IDRs only for startup or recovery, requests
UI repaints directly from the decoder callback, and bounds every host and
decode queue. On Windows at most one forced recovery IDR is in flight: a
viewer keyframe request, send failure, or discarded P-frame raised before that
IDR reaches the sender is resolved by it instead of forcing another. The UI reports
capture, encode, send, reassembly, decode,
presentation, RTT, jitter, drop, loss, and host send-failure measurements.
Encoded datagrams are paced in byte-bounded batches on both hardware backends
to avoid burst loss at high frame rates. The viewer receives
the host's pipeline snapshot once per second and keeps both host and client
health visible while the remote video is fullscreen. If those snapshots stop
for three seconds, the viewer marks the retained server values as stale so an
exited or stalled host video pipeline cannot continue to look healthy. On
Windows it also reports
the DXGI update rate, accumulated desktop frames, timeouts, pacing,
preprocessing and encoder-slot skips, plus copy, conversion, mapping,
submission, completion, and bitstream timing. Opus system
audio is streamed independently and the viewer can select the default or a
named output device.

Standard mode uses 1400-byte UDP datagrams to avoid IP fragmentation. The host
can explicitly enable **Jumbo MTU 9000**; it uses 8192-byte UDP datagrams only
when the desktop client advertises support. Enable it only when every LAN hop,
including both network adapters and switches, is configured for a 9000-byte
MTU. The bitrate control describes encoded video before recovery and network
overhead; use 80 Mbps or less on a physical 100-Mbps path. A 100-Mbps encoded
stream requires roughly 108–110 Mbps on the wire at 90–120 FPS.

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
