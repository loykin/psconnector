//! The command port (127.0.0.1:8650, one JSON object per line) used by macros and the MCP server.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use psconnector::Button;
use psconnector_protocol::{ButtonAction, Command, InputBench, OcrRegion, PictureChange, touch_point};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::macros::record::{self, Event};
use crate::ocr;
use crate::quality::Quality;
use crate::session::Shared;
use crate::video::{picture, presets};

pub(crate) enum Reply {
    Text(String),
    Bytes(Vec<u8>),
}

pub(crate) async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let Ok((sock, _)) = listener.accept().await else { continue };
        let shared = shared.clone();
        tokio::spawn(async move {
            let (r, mut w) = sock.into_split();
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let out = match handle(&line, &shared).await {
                    Ok(Reply::Text(s)) => format!("{s}\n").into_bytes(),
                    Ok(Reply::Bytes(b)) => b,
                    Err(e) => format!("err {e}\n").into_bytes(),
                };
                if w.write_all(&out).await.is_err() {
                    break;
                }
            }
        });
    }
}

fn text(s: impl Into<String>) -> Result<Reply> {
    Ok(Reply::Text(s.into()))
}

fn image(width: usize, height: usize, rgb: &[u8]) -> Result<Reply> {
    let mut out = psconnector_protocol::image_header(width, height, rgb.len()).into_bytes();
    out.extend_from_slice(rgb);
    Ok(Reply::Bytes(out))
}

pub(crate) async fn handle(line: &str, sh: &Arc<Shared>) -> Result<Reply> {
    let command = Command::parse_line(line)?;
    let c = &sh.client();
    let frame = || c.frame().ok_or_else(|| anyhow!("no frame yet"));
    match command {
        Command::Button { button, action } => match action {
            ButtonAction::Press => c.press(button),
            ButtonAction::Release => c.release(button),
            ButtonAction::Tap { hold } => c.tap(button, Duration::from_secs_f64(hold)).await,
        },
        Command::Stick { right, x, y } => c.stick(right, x, y),
        Command::Touch { id, down, x, y } => c.touch(id, down, x, y),
        Command::Swipe { from: (x0, y0), to: (x1, y1), ms } => {
            let steps = (ms / 16.0).ceil() as u32;
            let id = 0x40 | (sh.touch_ids.fetch_add(1, Ordering::Relaxed) & 0x3F) as u8;
            let at = |k: u32| {
                let f = f64::from(k) / f64::from(steps);
                touch_point(x0 + (x1 - x0) * f, y0 + (y1 - y0) * f)
            };
            for k in 0..=steps {
                let (x, y) = at(k);
                c.touch(id, true, x, y);
                tokio::time::sleep(Duration::from_secs_f64(ms / 1000.0 / f64::from(steps))).await;
            }
            let (x, y) = at(steps);
            c.touch(id, false, x, y);
        }
        Command::ReleaseAll => sh.release_all(),
        Command::Frame => {
            let f = frame()?;
            return image(f.width, f.height, f.rgb());
        }
        Command::Thumb { region } => {
            let f = frame()?;
            let region = region.unwrap_or(record::FULL);
            let rgb = tokio::task::spawn_blocking(move || record::downscale(&f, region, record::THUMB)).await?;
            let (w, h) = record::THUMB;
            return image(w, h, &rgb);
        }
        Command::Ocr { region, scale } => {
            let f = frame()?;
            let region = match region {
                OcrRegion::Pixels(r) => r,
                OcrRegion::Whole => [0, 0, f.width as u32, f.height as u32],
                OcrRegion::Fractions([x0, y0, x1, y1]) => {
                    let px = |v: f64, size: usize| (v.clamp(0.0, 1.0) * size as f64) as u32;
                    let (x0, y0, x1, y1) = (px(x0, f.width), px(y0, f.height), px(x1, f.width), px(y1, f.height));
                    [x0, y0, x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1)]
                }
            };
            let lines = tokio::task::spawn_blocking(move || {
                let (w, h, rgb) = ocr::crop_scale(&f, region, scale);
                ocr::read_text(&psconnector_protocol::png(w, h, &rgb, false)?).map_err(|e| anyhow!(e))
            })
            .await??;
            return text(format!("ok {}", serde_json::to_string(&lines)?));
        }
        Command::Snap(path) => {
            let f = frame()?;
            tokio::task::spawn_blocking(move || save_png(&path, f.width, f.height, f.rgb(), false)).await??;
        }
        Command::ScreenSnap(path) => {
            // Captures the composed viewport, including the GPU picture shader. `snap` and
            // `picture_snap` intentionally remain decoder-sized images for macro tooling.
            let ctx = sh.ctx.get().ok_or_else(|| anyhow!("window is not ready"))?.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            ctx.request_screenshot(move |image| {
                let _ = tx.send(image);
            });
            let image = tokio::time::timeout(Duration::from_secs(3), rx)
                .await
                .context("window screenshot timed out")?
                .context("window screenshot was cancelled")?;
            tokio::task::spawn_blocking(move || {
                let rgba: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
                save_png(&path, image.width(), image.height(), &rgba, true)
            })
            .await??;
        }
        Command::PictureSnap(path) => {
            // Unlike `snap`, this captures the actual display adjustment. It is primarily for comparing and
            // tuning presets without pretending that the unadjusted macro/OCR frame is what the window shows.
            let f = frame()?;
            let adjustment = *sh.picture.lock().unwrap();
            tokio::task::spawn_blocking(move || {
                let mut rgb = Vec::new();
                picture::Lut::new(&adjustment).apply(f.rgb(), &mut rgb);
                save_png(&path, f.width, f.height, &rgb, false)
            })
            .await??;
        }
        Command::Play(path) => {
            if sh.playing.swap(true, Ordering::SeqCst) {
                bail!("already playing");
            }
            sh.stop_play.store(false, Ordering::SeqCst);
            let loaded = std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|t| serde_json::from_str::<Vec<Value>>(&t).map_err(anyhow::Error::from));
            match loaded {
                Ok(events) => {
                    tokio::spawn(play(events, sh.clone()));
                }
                Err(e) => {
                    sh.playing.store(false, Ordering::SeqCst);
                    bail!("cannot read the recording: {e}");
                }
            }
        }
        Command::Stop => sh.stop_play.store(true, Ordering::SeqCst),
        Command::InputBench(b) => {
            // Generates a small, repeatable command load so input queue latency can be measured without a
            // person moving the pad. Stick values default below the console dead zone and button events are
            // releases only.
            if sh.playing.swap(true, Ordering::SeqCst) {
                bail!("already playing or benchmarking");
            }
            sh.stop_play.store(false, Ordering::SeqCst);
            tokio::spawn(input_bench(sh.clone(), b));
            return text(format!(
                "ok input benchmark started ({:.1} s, {} ms interval, burst {})",
                b.seconds, b.interval_ms, b.burst
            ));
        }
        Command::Status => return text(if sh.playing.load(Ordering::SeqCst) { "ok playing" } else { "ok idle" }),
        Command::Osd(s) => *sh.osd.lock().unwrap() = s,
        Command::Standby => c.standby(),
        Command::Macros { reload } => {
            let mut r = sh.runner.lock().unwrap();
            if reload {
                r.reload();
            }
            return text(format!("ok {}", r.list_json()));
        }
        Command::MacroRun { name, options, at } => {
            let msg = sh.runner.lock().unwrap().run_named(&name, &options, at.as_deref()).map_err(|e| anyhow!(e))?;
            return text(format!("ok {msg}"));
        }
        Command::Quality(q) => {
            if let Some(q) = q {
                let q = Quality::parse(&q).ok_or_else(|| anyhow!("quality must be low, 720p or 1080p"))?;
                sh.set_quality(q).map_err(|e| anyhow!(e))?;
            }
            let f = c.frame();
            return text(format!(
                "ok {}",
                serde_json::json!({
                    "quality": sh.quality.lock().unwrap().name(),
                    "reconnecting": sh.reconnecting.lock().unwrap().clone(),
                    "width": f.as_ref().map(|f| f.width),
                    "height": f.as_ref().map(|f| f.height),
                })
            ));
        }
        Command::VideoCodec(Some(codec)) => {
            sh.set_video_codec(codec).map_err(|e| anyhow!(e))?;
            return text(format!("ok reconnecting {}", codec.name()));
        }
        Command::VideoCodec(None) => return text(format!("ok {}", sh.video_codec.lock().unwrap().name())),
        Command::VideoRange(range) => {
            if let Some(range) = range {
                sh.set_video_range(range);
            }
            return text(format!("ok {}", sh.video_range.lock().unwrap().name()));
        }
        Command::PictureSave(name) => {
            let name = name.trim();
            if name.is_empty() || name.eq_ignore_ascii_case("Original") {
                bail!("picture_save needs a name other than Original");
            }
            presets::save(name, *sh.picture.lock().unwrap(), *sh.video_range.lock().unwrap());
            return text(format!("ok saved {name}"));
        }
        Command::PictureDelete(name) => {
            let name = name.trim();
            if !presets::delete(name) {
                bail!("no saved picture preset named '{name}'");
            }
            return text(format!("ok deleted {name}"));
        }
        Command::Picture(change) => {
            let next = match change {
                PictureChange::Values(v) => Some((sh.picture.lock().unwrap().merged(&Value::Object(v)), None)),
                PictureChange::Preset(name) => {
                    let preset = presets::all().into_iter().find(|p| p.name.eq_ignore_ascii_case(&name));
                    let p = preset.ok_or_else(|| anyhow!("no picture preset named '{name}'"))?;
                    Some((p.picture, p.range))
                }
                PictureChange::Report => None,
            };
            if let Some((p, range)) = next {
                if let Some(range) = range {
                    sh.set_video_range(range);
                }
                sh.set_picture(p);
            }
            return text(format!("ok {}", sh.picture.lock().unwrap().to_json()));
        }
        Command::ShouldStop => {
            let yes = sh.runner.lock().unwrap().should_stop();
            return text(if yes { "ok yes" } else { "ok no" });
        }
        Command::MacroStop => sh.runner.lock().unwrap().stop(),
        Command::MacroStatus { lines } => {
            return text(format!("ok {}", sh.runner.lock().unwrap().status_json(lines)));
        }
    }
    text("ok")
}

/// Plays an old-style recording: a list of [`record::Event`]s (checkpoints are ignored).
async fn play(events: Vec<Value>, sh: Arc<Shared>) {
    let c = sh.client();
    let t0 = tokio::time::Instant::now();
    let mut held: HashSet<Button> = HashSet::new();
    for (t, ev) in events.iter().filter_map(record::read_event) {
        if sh.stop_play.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep_until(t0 + Duration::from_secs_f64(t.max(0.0))).await;
        match ev {
            Event::Input(Command::Button { button, action }) => {
                if action == ButtonAction::Press {
                    c.press(button);
                    held.insert(button);
                } else {
                    c.release(button);
                    held.remove(&button);
                }
            }
            Event::Input(Command::Stick { right, x, y }) => c.stick(right, x, y),
            Event::Input(Command::Touch { id, down, x, y }) => c.touch(id, down, x, y),
            _ => {}
        }
    }
    for b in held {
        c.release(b);
    }
    c.stick(false, 0.0, 0.0);
    c.stick(true, 0.0, 0.0);
    sh.playing.store(false, Ordering::SeqCst);
}

/// Generates repeatable, normally invisible input traffic for the passive latency counters in the stream.
async fn input_bench(sh: Arc<Shared>, b: InputBench) {
    let InputBench { seconds, interval_ms, burst, amplitude } = b;
    let c = sh.client();
    let interval = Duration::from_millis(interval_ms);
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(seconds);
    let mut tick = 0u64;
    while tokio::time::Instant::now() < deadline && !sh.stop_play.load(Ordering::SeqCst) {
        for n in 0..burst {
            let direction = if (tick + n as u64).is_multiple_of(2) { 1.0 } else { -1.0 };
            c.stick(true, amplitude * direction, 0.0);
        }
        // A release of an already-up button exercises the ordered button path without triggering gameplay.
        if tick.is_multiple_of(10) {
            c.release(Button::Cross);
        }
        tick += 1;
        tokio::time::sleep(interval).await;
    }
    sh.release_all();
    sh.playing.store(false, Ordering::SeqCst);
    log::info!("input benchmark finished after {tick} ticks; all controls released");
}

/// Saves 8-bit RGB (or RGBA) pixels as a PNG file.
pub(crate) fn save_png(path: &str, width: usize, height: usize, pixels: &[u8], alpha: bool) -> Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    psconnector_protocol::write_png(file, width, height, pixels, alpha)?;
    Ok(())
}
