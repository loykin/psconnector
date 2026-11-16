//! The command port (127.0.0.1:8650, one JSON object per line) used by macros and the MCP server.

#[allow(unused_imports)]
use super::*;

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

pub(crate) fn button(name: &str) -> Result<Button> {
    Button::parse(name).ok_or_else(|| anyhow!("unknown button: {name}"))
}

pub(crate) async fn handle(line: &str, sh: &Arc<Shared>) -> Result<Reply> {
    let ok = || Ok(Reply::Text("ok".into()));
    let m: Value = serde_json::from_str(line).context("not JSON")?;
    let c = &sh.client();
    if let Some(name) = m["button"].as_str() {
        let b = button(name)?;
        match m["action"].as_str().unwrap_or("tap") {
            "press" => c.press(b),
            "release" => c.release(b),
            "tap" => c.tap(b, Duration::from_secs_f64(m["hold"].as_f64().unwrap_or(0.08))).await,
            other => bail!("unknown action: {other}"),
        }
    } else if let Some(which) = m["stick"].as_str() {
        let (x, y) = (m["x"].as_f64().unwrap_or(0.0) as f32, m["y"].as_f64().unwrap_or(0.0) as f32);
        c.stick(which == "right", x, y);
    } else if m.get("release_all").is_some() {
        sh.release_all();
    } else if m.get("frame").is_some() {
        let f = c.frame().ok_or_else(|| anyhow!("no frame yet"))?;
        let mut out = format!("ok {} {} {}\n", f.width, f.height, f.rgb().len()).into_bytes();
        out.extend_from_slice(f.rgb());
        return Ok(Reply::Bytes(out));
    } else if let Some(region) = m.get("thumb") {
        // {"thumb": [x0, y0, x1, y1]} (fractions; [] or true = whole frame): the screen at checkpoint size
        let f = c.frame().ok_or_else(|| anyhow!("no frame yet"))?;
        let v: Vec<f64> = region.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
        let region = match v.as_slice() {
            [x0, y0, x1, y1] => [*x0, *y0, *x1, *y1],
            [] => record::FULL,
            _ => bail!("thumb region must be [x0, y0, x1, y1] fractions"),
        };
        let rgb = tokio::task::spawn_blocking(move || record::downscale(&f, region, record::THUMB)).await?;
        let (w, h) = record::THUMB;
        let mut out = format!("ok {w} {h} {}\n", rgb.len()).into_bytes();
        out.extend_from_slice(&rgb);
        return Ok(Reply::Bytes(out));
    } else if let Some(region) = m.get("ocr") {
        // {"ocr": [x, y, w, h], "scale": 2.0} → ok ["line", ...]
        let f = c.frame().ok_or_else(|| anyhow!("no frame yet"))?;
        let r: Vec<u32> = region
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_f64()).map(|v| v.max(0.0) as u32).collect())
            .unwrap_or_default();
        let region = if m["rel"].as_bool() == Some(true) {
            // "rel": corners [x0, y0, x1, y1] as fractions of the frame, so the region does not depend on quality
            let v: Vec<f64> =
                region.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
            let [x0, y0, x1, y1] = v.as_slice() else { bail!("ocr with rel needs [x0, y0, x1, y1] fractions") };
            let px = |v: f64, size: usize| (v.clamp(0.0, 1.0) * size as f64) as u32;
            let (x0, y0, x1, y1) = (px(*x0, f.width), px(*y0, f.height), px(*x1, f.width), px(*y1, f.height));
            [x0, y0, x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1)]
        } else {
            match r.as_slice() {
                [x, y, w, h] => [*x, *y, *w, *h],
                [] => [0, 0, f.width as u32, f.height as u32],
                _ => bail!("ocr region must be [x, y, w, h]"),
            }
        };
        let scale = m["scale"].as_f64().unwrap_or(1.0) as f32;
        let lines = tokio::task::spawn_blocking(move || {
            let (w, h, rgb) = ocr::crop_scale(&f, region, scale);
            ocr::read_text(&png_bytes(w, h, &rgb)?).map_err(|e| anyhow!(e))
        })
        .await??;
        return Ok(Reply::Text(format!("ok {}", serde_json::to_string(&lines)?)));
    } else if let Some(path) = m["snap"].as_str() {
        let f = c.frame().ok_or_else(|| anyhow!("no frame yet"))?;
        let path = path.to_string();
        tokio::task::spawn_blocking(move || save_png(&path, &f)).await??;
    } else if let Some(path) = m["screen_snap"].as_str() {
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
        let path = path.to_string();
        tokio::task::spawn_blocking(move || save_color_image(&path, &image)).await??;
    } else if let Some(path) = m["picture_snap"].as_str() {
        // Unlike `snap`, this captures the actual display adjustment. It is primarily for comparing and
        // tuning presets without pretending that the unadjusted macro/OCR frame is what the window shows.
        let f = c.frame().ok_or_else(|| anyhow!("no frame yet"))?;
        let adjustment = *sh.picture.lock().unwrap();
        let path = path.to_string();
        tokio::task::spawn_blocking(move || {
            let mut rgb = Vec::new();
            picture::Lut::new(&adjustment).apply(f.rgb(), &mut rgb);
            let adjusted = Frame::new(f.width, f.height, psconnector::PixelFormat::Rgb8, rgb, f.index);
            save_png(&path, &adjusted)
        })
        .await??;
    } else if let Some(path) = m["play"].as_str() {
        if sh.playing.swap(true, Ordering::SeqCst) {
            bail!("already playing");
        }
        sh.stop_play.store(false, Ordering::SeqCst);
        let loaded = std::fs::read_to_string(path)
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
    } else if m.get("stop").is_some() {
        sh.stop_play.store(true, Ordering::SeqCst);
    } else if let Some(options) = m.get("input_bench") {
        // Generates a small, repeatable command load so input queue latency can be measured without a person
        // moving the pad. Stick values default below the console dead zone and button events are releases only.
        if sh.playing.swap(true, Ordering::SeqCst) {
            bail!("already playing or benchmarking");
        }
        sh.stop_play.store(false, Ordering::SeqCst);
        let seconds = options["seconds"].as_f64().unwrap_or(12.0).clamp(2.0, 30.0);
        let interval = options["interval_ms"].as_u64().unwrap_or(10).clamp(1, 100);
        let burst = options["burst"].as_u64().unwrap_or(4).clamp(1, 20) as usize;
        let amplitude = options["amplitude"].as_f64().unwrap_or(0.01).clamp(0.0, 1.0) as f32;
        tokio::spawn(input_bench(sh.clone(), seconds, interval, burst, amplitude));
        return Ok(Reply::Text(format!(
            "ok input benchmark started ({seconds:.1} s, {interval} ms interval, burst {burst})"
        )));
    } else if m.get("status").is_some() {
        return Ok(Reply::Text(if sh.playing.load(Ordering::SeqCst) { "ok playing" } else { "ok idle" }.into()));
    } else if let Some(text) = m.get("osd") {
        *sh.osd.lock().unwrap() = text.as_str().map(str::to_string).unwrap_or_else(|| text.to_string());
    } else if m.get("standby").is_some() {
        c.standby();
    } else if m.get("macros").is_some() {
        let r = sh.runner.lock().unwrap();
        if m["reload"].as_bool() == Some(true) {
            drop(r);
            sh.runner.lock().unwrap().reload();
            return Ok(Reply::Text(format!("ok {}", sh.runner.lock().unwrap().list_json())));
        }
        return Ok(Reply::Text(format!("ok {}", r.list_json())));
    } else if let Some(name) = m["macro_run"].as_str() {
        // optional "limit_min", "after": "none|standby", "at": "HH:MM|+MIN"
        let msg = sh.runner.lock().unwrap().run_named(name, &m, m["at"].as_str()).map_err(|e| anyhow!(e))?;
        return Ok(Reply::Text(format!("ok {msg}")));
    } else if let Some(q) = m["quality"].as_str() {
        // {"quality": "low|720p|1080p"} reconnects at that quality; {"quality": ""} just reports it
        if !q.is_empty() {
            let q = Quality::parse(q).ok_or_else(|| anyhow!("quality must be low, 720p or 1080p"))?;
            sh.set_quality(q).map_err(|e| anyhow!(e))?;
        }
        let f = c.frame();
        return Ok(Reply::Text(format!(
            "ok {}",
            serde_json::json!({
                "quality": sh.quality.lock().unwrap().name(),
                "reconnecting": sh.reconnecting.lock().unwrap().clone(),
                "width": f.as_ref().map(|f| f.width),
                "height": f.as_ref().map(|f| f.height),
            })
        )));
    } else if let Some(name) = m["video_codec"].as_str() {
        if !name.is_empty() {
            let codec = parse_video_codec(name).ok_or_else(|| anyhow!("video_codec must be h264, h265 or h265-hdr"))?;
            sh.set_video_codec(codec).map_err(|e| anyhow!(e))?;
            return Ok(Reply::Text(format!("ok reconnecting {}", video_codec_name(codec))));
        }
        return Ok(Reply::Text(format!("ok {}", video_codec_name(*sh.video_codec.lock().unwrap()))));
    } else if let Some(name) = m["video_range"].as_str() {
        if !name.is_empty() {
            let range = parse_video_range(name).ok_or_else(|| anyhow!("video_range must be auto, limited or full"))?;
            sh.set_video_range(range);
        }
        return Ok(Reply::Text(format!("ok {}", video_range_name(*sh.video_range.lock().unwrap()))));
    } else if let Some(name) = m["picture_save"].as_str() {
        let name = name.trim();
        if name.is_empty() || name.eq_ignore_ascii_case("Original") {
            bail!("picture_save needs a name other than Original");
        }
        Viewer::save_preset(name, *sh.picture.lock().unwrap(), *sh.video_range.lock().unwrap());
        return Ok(Reply::Text(format!("ok saved {name}")));
    } else if let Some(name) = m["picture_delete"].as_str() {
        let name = name.trim();
        if !Viewer::delete_preset(name) {
            bail!("no saved picture preset named '{name}'");
        }
        return Ok(Reply::Text(format!("ok deleted {name}")));
    } else if let Some(v) = m.get("picture") {
        // {"picture": {"shadows": 0.5, ...}} changes those values; {"picture": "preset name"} loads a preset;
        // {"picture": ""} only reports. Reply: ok {current values}
        let cur = *sh.picture.lock().unwrap();
        let next = match v {
            Value::Object(_) => Some((cur.merged(v), None)),
            Value::String(name) if !name.is_empty() => {
                let preset = Viewer::picture_presets()
                    .into_iter()
                    .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
                    .map(|(_, p, range)| (p, range));
                let (picture, range) = preset.ok_or_else(|| anyhow!("no picture preset named '{name}'"))?;
                Some((picture, range))
            }
            _ => None,
        };
        if let Some((p, range)) = next {
            if let Some(range) = range {
                sh.set_video_range(range);
            }
            sh.set_picture(p);
        }
        return Ok(Reply::Text(format!("ok {}", sh.picture.lock().unwrap().to_json())));
    } else if m.get("should_stop").is_some() {
        let yes = sh.runner.lock().unwrap().should_stop();
        return Ok(Reply::Text(if yes { "ok yes" } else { "ok no" }.into()));
    } else if m.get("macro_stop").is_some() {
        sh.runner.lock().unwrap().stop();
    } else if m.get("macro_status").is_some() {
        let lines = m["lines"].as_u64().unwrap_or(10) as usize;
        return Ok(Reply::Text(format!("ok {}", sh.runner.lock().unwrap().status_json(lines))));
    } else {
        bail!("unknown command");
    }
    ok()
}

/// Plays a recording: `[{"t":secs,"b":button,"a":"press|release"} or {"t":secs,"s":"left|right","x":..,"y":..}]`
pub(crate) async fn play(events: Vec<Value>, sh: Arc<Shared>) {
    let c = sh.client();
    let t0 = tokio::time::Instant::now();
    let mut held: HashSet<Button> = HashSet::new();
    for ev in events {
        if sh.stop_play.load(Ordering::SeqCst) {
            break;
        }
        let t = ev["t"].as_f64().unwrap_or(0.0);
        tokio::time::sleep_until(t0 + Duration::from_secs_f64(t.max(0.0))).await;
        if let Some(name) = ev["b"].as_str() {
            let Ok(b) = button(name) else { continue };
            if ev["a"].as_str() == Some("press") {
                c.press(b);
                held.insert(b);
            } else {
                c.release(b);
                held.remove(&b);
            }
        } else if let Some(which) = ev["s"].as_str() {
            c.stick(which == "right", ev["x"].as_f64().unwrap_or(0.0) as f32, ev["y"].as_f64().unwrap_or(0.0) as f32);
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
pub(crate) async fn input_bench(sh: Arc<Shared>, seconds: f64, interval_ms: u64, burst: usize, amplitude: f32) {
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

pub(crate) fn png_bytes(w: usize, h: usize, rgb: &[u8]) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_compression(png::Compression::Fast);
        enc.write_header()?.write_image_data(rgb)?;
    }
    Ok(buf)
}

pub(crate) fn save_png(path: &str, f: &Frame) -> Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, f.width as u32, f.height as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_compression(png::Compression::Fast);
    enc.write_header()?.write_image_data(f.rgb())?;
    Ok(())
}

pub(crate) fn save_color_image(path: &str, image: &egui::ColorImage) -> Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, image.width() as u32, image.height() as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_compression(png::Compression::Fast);
    let rgba: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
    enc.write_header()?.write_image_data(&rgba)?;
    Ok(())
}
