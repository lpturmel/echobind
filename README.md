# Echobind

Low-footprint real-time audio and video streaming.

The repository is a Cargo workspace:

- `echobind-core` contains shared configuration, the versioned wire protocol,
  and bounded video fragmentation/reassembly.
- `echobind-cli` contains the current audio CLI and platform integrations. Its
  binary is still named `echobind`.

Video capture, encoding, decoding, and presentation are not wired into the CLI
yet. Protocol version 2 reserves packet types for fragmented video frames and
keyframe requests.

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
