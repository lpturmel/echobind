# Echobind

Minimalistic audio streaming CLI

Project mainly for education purposes, I needed to have my audio of a Windows machine streamed to my Mac and wanted something with low footprint.

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

Echobind now uses UDP for setup, heartbeat, and audio. The client sends a
UDP hello to receive the audio config, then sends periodic UDP pings; either
side treats 3 seconds without a UDP response as a disconnect.
