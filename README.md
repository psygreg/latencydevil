# latencydevil
GPU-aware launcher wrapper to set low latency modes for gaming.

## `latencydevil` launcher wrapper

A small binary that wraps a launch command, injects GPU-aware environment
variables (discrete GPUs preferred) and then `exec`s the command, so the
wrapper never lingers in the process tree (Steam keeps tracking the game as
its direct child).

| Detected GPU(s)           | Variables injected                                                                                              |
| ------------------------- | --------------------------------------------------------------------------------------------------------------- |
| Any GPU                   | `PROTON_USE_WAYLAND=1` `LOW_LATENCY_LAYER=1` `SDL_VIDEO_DRIVER=wayland`                                          |
| NVIDIA GPU present        | additionally `LOW_LATENCY_LAYER_REFLEX=1` and `DXVK_CONFIG="dxgi.hideAmdGpu = True"`                             |

An existing `DXVK_CONFIG` is preserved: the NVIDIA directive is appended
newline-separated (DXVK parses one directive per line).

### Build & install

No dependencies; builds with anything ≥ Rust 1.75 (Ubuntu 24.04's stock
`rustc` is enough):

```sh
cargo build --release
sudo cp target/release/latencydevil /usr/local/bin/   # or anywhere on your PATH
sudo cargo install --path .                            # alternative: ~/.cargo/bin
```

### Usage

Steam launch options:

```
latencydevil -- %command%
```

Terminal (works with any command, flatpak included):

```sh
latencydevil -- flatpak run at.anegostudios.VintageStory
latencydevil -- wine /path/to/game.exe
```

### Flatpak handling

Flatpak sandboxes drop environment variables they don't allow-list, so plain
`setenv` would never reach the app. When the wrapped command starts with
`flatpak run`, the wrapper detects it and forwards the variables as
`--env=KEY=VALUE` args right after `run`:

```
flatpak run --env=PROTON_USE_WAYLAND=1 --env=... app.id
```

### Debugging

```sh
latencydevil --print            # variables that would be set, then exit
latencydevil --debug -- env     # GPU detection, applied vars and exec line
```

GPU detection walks `/sys/bus/pci/devices` and keeps PCI display-class
(`0x03xxxx`) devices. Discreteness (used only to prefer one GPU in the
detection report) is heuristic: NVIDIA always counts as discrete; other
vendors count as discrete when they report the 3D-controller (`0x030200`) or
display-controller (`0x038000`) subclass — iGPUs universally report
VGA-compatible (`0x030000`). If no GPU can be detected (unusual VM setups), the
universal variable set is applied as a fallback.

Unix signals pass straight through to the game since the wrapper `exec`s into
it — nothing to forward.

### Exit codes

- The wrapped command's exit status (the wrapper replaces itself, so signals
  killing the game propagate naturally).
- `64` for bad usage, `127`/`126` when the wrapped command cannot be exec'd.

## License
Apache-2.0 (see `LICENSE`).
