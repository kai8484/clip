# clip

Lightweight Windows instant-replay recorder.

Captures in RAM on GPU (DXGI Desktop Duplication -> D3D11 NV12 -> Media Foundation H.264/HEVC) and saves the last seconds on hotkey press.

## Build

```bash
cargo build --release --workspace
```

Binary produced:
- `clip.exe` — tray recorder and settings UI

Data, logs (`data/clip.log`), and saved clips live next to the executable.

## License

[GPL-3.0-or-later](LICENSE)
