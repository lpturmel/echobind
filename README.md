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
echobind record
```


## Client

```
echobind connect --ip X.X.X.X
```

## Default ports

TCP: 3012
UDP: 3013

Both can be overrided with cli parameters to both commands
