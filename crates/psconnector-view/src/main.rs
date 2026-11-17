//! psconnector-view: PS5 screen window + control command port + macro runner.
//!
//! ```text
//! cargo run --release -p psconnector-view -- [host] [--port 8650] [--macros FILE]
//!     [--quality low|720p|1080p] [--bitrate KBPS] [--no-pad] [--no-audio] [--mute] [--run MACRO [--for MIN] [--after none|standby] [--at HH:MM|+MIN]]
//! - A game controller connected to this computer (DualSense, ...) drives the PS5 directly (off with `--no-pad`).
//! - The console's sound plays on the default output device (🔊 in the bar mutes it; `--mute`, `--no-audio`).
//! ```
//! - Command port (127.0.0.1:8650, one JSON object per line): the same commands as the Python `ps5view.py`
//!   plus `frame` and `release_all`, so existing macros run unchanged. From Python, use `python/psconnector.py`.
//! - Top bar: pick a macro and ▶ Run / ■ Stop. Each folder with a `macro.json` in the `macros` folder next to
//!   the program is a macro (`--macros PATH` reads other folders, or an older list file, instead).
//!   Macros run as child processes and get the port via the `PSCONNECTOR_PORT` environment variable.
//!   Stop sends SIGINT first (KeyboardInterrupt in Python) and kills the process if it is not gone in 3 s.
//!   However a macro ends, all buttons are released and the sticks are centered.
//!   Each run can have a time limit and an after-action (rest mode), and can be scheduled for a start time.
//!   At the limit, a macro that polls `should_stop` gets 2 minutes to finish; others are stopped right away.
//!
//! The protocol is defined in the `psconnector-protocol` crate (`Command`); the list below is a summary.
//!
//! Commands: `{"button":"cross","action":"tap|press|release","hold":0.08}` `{"stick":"left","x":0,"y":-1}`
//! `{"touch":0,"down":true,"x":960,"y":540}` (a finger on the touchpad, 1920×1080; `down:false` lifts it)
//! `{"swipe":[x0,y0,x1,y1],"ms":300}` (a finger moved across the touchpad; returns when done)
//! `{"release_all":true}` `{"snap":"path.png"}` `{"screen_snap":"path.png"}` `{"frame":true}`
//! `{"play":"recording.json"}` `{"stop":true}` `{"thumb":[x0,y0,x1,y1]}` (the screen at checkpoint size: reply
//! `ok WIDTH HEIGHT BYTES` and the RGB bytes, like `frame`)
//! `{"status":true}` `{"osd":"text"}` `{"standby":true}` `{"ocr":[x,y,w,h],"scale":2}` (reply `ok ["line", ...]`)
//! Macros: `{"macros":true}` `{"macro_run":"name","limit_min":60,"after":"standby","at":"23:00"}` (all but the name
//! optional) `{"macro_stop":true}` (also cancels a schedule) `{"macro_status":true,"lines":10}` (reply `ok {JSON}`)
//! `{"should_stop":true}` (reply `ok yes` once the time limit is up, else `ok no`)
//! `{"quality":"low|720p|1080p"}` reconnect at that picture quality (`""` only reports it; reply `ok {JSON}`)
//! OCR with `"rel":true` takes corners `[x0,y0,x1,y1]` as fractions of the frame.
//! Replies are one line: `ok` / `ok playing` / `ok idle` / `err REASON`.
//! Only `frame` is followed by raw RGB bytes after its `ok WIDTH HEIGHT BYTES` line. Press Q in the window to quit.

mod app;
mod cli;
mod commands;
mod input;
mod macros;
mod ocr;
mod quality;
mod session;
mod sound;
mod ui;
mod video;

fn main() -> anyhow::Result<()> {
    match cli::parse(std::env::args().skip(1))? {
        cli::Mode::LoginWindow => ui::login_window::run(),
        cli::Mode::Replay { path, repeat } => macros::replay::run(&path, repeat),
        cli::Mode::Window(args) => {
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
            app::run(args)
        }
    }
}
