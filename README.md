# PsConnector

A PS5 Remote Play client written in Rust. It connects to a PS5 over Remote Play from macOS, receives the screen, and sends button and stick input.
It is a connection layer that lets game macros and AI assistants operate a PS5.

> Unofficial tool, not affiliated with Sony Interactive Entertainment. PlayStation, PS5 and PS Remote Play are trademarks of Sony.

| Crate | Kind | Purpose |
|---|---|---|
| `psconnector` | library | Session, Takion, controller input, video decoding |
| `psconnector-view` | binary | Screen window, macro command port (8650), macro bar |
| `psconnector-mcp` | binary | MCP server for AI assistants (Claude Code, Codex, ...) |
| `python/psconnector.py` | Python module | Simple interface for writing macros (standard library only) |

## Status

| Feature | Status |
|---|---|
| Console status (DDP) | done |
| Wake from rest mode | done |
| Session setup (init/ctrl auth, control channel, heartbeat) | done |
| Takion stream (INIT → COOKIE → BIG → BANG, ECDH key exchange) | done |
| Button input (last 5 events sent together, plus one re-send of the same packet) | done |
| Stick input (state re-sent every 200 ms) | done (verified with analog recording playback in Code Vein) |
| Rest mode | done |
| Video (H.264 → RGB, 720p) | done (openh264, about 40 fps for the first 30 frames). Frames with missing units are dropped and a key frame is requested. No FEC recovery yet |
| Audio | not yet (received and discarded) |
| PSN login, PIN registration | not yet (reads the registration saved by pyremoteplay) |

## Design

**Only the stream task writes to the socket.** Button and stick commands are passed to it over a channel, and it encrypts and sends them in order. Input from many places at once never shares an encryption position (key_pos).

The `psconnector` library only exposes `Client`, `Button`, `Frame`, `Options`, `StreamConfig`, `Phase` and `Error`, plus the `ddp` (status and wake-up) and `profile` (registration) modules. Errors are a `psconnector::Error` enum, so you can handle cases such as `Error::Busy` (another session connected) or `Error::NotOn` (rest mode) separately.

Internal modules:

| Module | Purpose |
|---|---|
| `ddp` | Status and wake-up (UDP 9302) |
| `session` | Session setup and control channel (TCP 9295) |
| `rpcrypt` | Session-stage encryption (AES-CFB) |
| `stream` | Takion connection and the send task (UDP 9296) |
| `takion` | Building and parsing Takion packets |
| `gkcrypt` | Stream-stage encryption (ECDH, key stream, GMAC) |
| `controller` | Button events and stick state data |
| `video` | Video unit assembly, decoder thread (openh264) |
| `proto` | Takion protobuf messages |
| `profile` | Reading registration data (`~/.pyremoteplay/.profile.json`) |

## Library usage

```rust
use psconnector::{Button, Client};

let client = Client::connect("192.168.1.20", Default::default()).await?;
client.tap(Button::Cross, std::time::Duration::from_millis(80)).await;
if let Some(frame) = client.frame() {
    println!("{}x{}", frame.width, frame.height); // frame.rgb: 8-bit RGB
}
client.close().await;
```

Command-line tool for manual checks:

```bash
cargo run -p psconnector --example rp -- 192.168.1.20 status
```

```bash
cargo run -p psconnector --example rp -- 192.168.1.20 tap right right
```

It supports `status`, `wake`, `connect [secs]`, `tap <button>...`, `snap [file]` and `standby`. Build with `--release`, because video decoding is slow in debug builds.

## psconnector-view (screen window + command port)

Shows the PS5 screen in a window and opens a command port for macros (127.0.0.1:8650). It accepts the same commands as the earlier Python `ps5view.py`, so existing macros can switch connection layers without code changes.

```bash
cargo run --release -p psconnector-view -- 192.168.1.20
```

| Command (one JSON object per line) | Reply |
|---|---|
| `{"button":"cross","action":"tap","hold":0.08}` (also `press`, `release`) | `ok` |
| `{"stick":"left","x":0.3,"y":-1}` | `ok` |
| `{"release_all":true}` release every button | `ok` |
| `{"snap":"path.png"}` save the current frame | `ok` |
| `{"frame":true}` current frame, raw | `ok WIDTH HEIGHT BYTES` followed by raw RGB bytes |
| `{"play":"recording.json"}` / `{"stop":true}` play and stop a recording | `ok` |
| `{"status":true}` | `ok playing` / `ok idle` |
| `{"osd":"text"}` show text in the top-left of the window | `ok` |
| `{"standby":true}` rest mode | `ok` |
| `{"macros":true}` / `{"macro_run":"name"}` / `{"macro_stop":true}` / `{"macro_status":true}` | `ok {JSON}` |

Whether you quit with Q in the window, Ctrl+C, or SIGTERM, the session is closed cleanly.

### Macro bar

Pick a macro in the bar at the top of the window and press **▶ Run / ■ Stop**. The list lives in `~/.psconnector/macros.json` (see `macros.example.json`).

```json
[{"name": "Code Vein farm", "cmd": "python3 -u farm.py 0", "cwd": "~/Project/qewsim/macro", "env": {"FARM_GOAL": "100000"}}]
```

- Macros run as child processes. The port is passed in the `PSCONNECTOR_PORT` environment variable.
- Stop sends SIGINT first (KeyboardInterrupt in Python) and kills the process if it has not exited after 3 seconds.
- However a macro ends, psconnector-view releases every button and centers both sticks, so nothing stays pressed.
- The bar shows the run time and the last line of the macro's output, with a rest-mode button on the right.
- `--run NAME` starts a macro right away.

## Python macro interface (`python/psconnector.py`)

A single file that only uses the standard library. No pyremoteplay, pyobjc or OpenCV needed.

```python
from psconnector import PS

ps = PS()                      # started from the psconnector-view bar, the port is passed automatically
ps.osd('starting')
ps.tap('cross')
ps.stick('left', 0, -1); ps.sleep(0.5); ps.stick('left', 0, 0)
w, h, rgb = ps.frame()         # ps.frame_array(bgr=True) if numpy is installed
ps.wait(lambda: ps.pixel(640, 360)[0] > 200, timeout=5)
```

See `python/example_menu_walk.py` for an example.

## AI integration (MCP server `psconnector-mcp`)

AI assistants with MCP support, such as Claude Code, Claude Desktop and Codex, can see the PS5 screen and control it. No API key is needed; they use your existing subscription. psconnector-view must be running.

```bash
cargo build --release -p psconnector-mcp
```

```bash
claude mcp add psconnector -- /path/to/psconnector/target/release/psconnector-mcp
```

Add `--allow-input` at the end to allow input. By default only viewing the screen, OSD and status work.

| Tool | Purpose | Needs input permission |
|---|---|---|
| `screenshot` | Current screen (zoom into a region, resize) | |
| `console_status` | PS5 on or in rest mode, whether psconnector-view is connected | |
| `osd` | Show text in the psconnector-view window | |
| `macro_list` / `macro_status` / `macro_stop` | Macro list, status and output, stop | |
| `release_all` | Release every button | |
| `tap` / `taps` / `press` / `release` / `stick` | Controller input | ✔ |
| `macro_run` | Run a macro | ✔ |
| `standby` / `wake` | Rest mode, wake-up | ✔ |

For example: "Look at the screen and tell me how to beat this boss", "Run the Code Vein farm", "Open the equipment menu".

## Build

```bash
cargo build --release
```

This produces `psconnector-view` and `psconnector-mcp` in `target/release/`. Release builds are recommended because of video decoding.

## Setup

- Register once with pyremoteplay (for example via ps5rmtctl) to log in to PSN and pair the PS5. That registration data is read as is.
- On macOS, the first run asks for local network access permission.

## References

The protocol behavior was checked against [chiaki](https://git.sr.ht/~thestr4ng3r/chiaki) and [pyremoteplay](https://github.com/ktnrg45/pyremoteplay). The crypto is compared byte for byte with pyremoteplay output in `psconnector/src/crypto_tests.rs`.

The session key tables in `psconnector/src/keys.rs` come from the official Remote Play app.

## License

MIT
