//! psconnector-mcp: an MCP server that lets AI assistants (Claude Code, Claude Desktop, Codex, ...) see and
//! control a PS5.
//!
//! It runs as a stdio MCP server and forwards commands to the command port of a running psconnector-view
//! (127.0.0.1:8650).
//!
//! ```text
//! claude mcp add psconnector -- /path/to/psconnector-mcp                # screen, OSD, macro status only
//! claude mcp add psconnector -- /path/to/psconnector-mcp --allow-input  # plus buttons, sticks, running macros, rest mode
//! ```
//! Options: `--port 8650` (psconnector-view command port), `--host 192.168.x.x` (for wake-up; defaults to the
//! ps5rmtctl config)

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;

const INSTRUCTIONS: &str = "Tools to see a PS5 Remote Play screen and control it with a virtual controller.
- Screen coordinates are always in 1280x720 space (screenshot's region uses them), whatever the picture quality.
- Button names: up down left right l1 r1 l2 r2 cross circle square triangle options share ps l3 r3 touchpad.
- Sticks: which=left|right, x and y in -1.0..1.0 (up is negative). With duration, the stick is held that long and then centered.
- Input is sent in order; psconnector-view encrypts and sends everything from one place. release_all releases everything at once.
- Macros (the list in the psconnector-view bar) can be handled with macro_list / macro_run (with optional time limit, rest mode afterwards, start time) / macro_stop / macro_status.
- psconnector-view must be running (it owns the PS5 connection).";

/// Connection to the psconnector-view command port (connects on first use, reconnects after errors)
struct Port {
    port: u16,
    conn: Mutex<Option<(BufReader<OwnedReadHalf>, OwnedWriteHalf)>>,
}

impl Port {
    async fn with<T>(
        &self,
        f: impl AsyncFnOnce(&mut BufReader<OwnedReadHalf>, &mut OwnedWriteHalf) -> Result<T>,
    ) -> Result<T> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            let sock = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(("127.0.0.1", self.port)))
                .await
                .map_err(|_| anyhow!("timed out connecting to psconnector-view"))?
                .map_err(|_| anyhow!("psconnector-view is not running (127.0.0.1:{})", self.port))?;
            let (r, w) = sock.into_split();
            *guard = Some((BufReader::new(r), w));
        }
        let (r, w) = guard.as_mut().unwrap();
        let out = f(r, w).await;
        if out.is_err() {
            *guard = None; // reconnect on the next call
        }
        out
    }

    /// Sends one command and returns what follows 'ok'
    async fn call(&self, msg: Value) -> Result<String> {
        self.with(async |r, w| {
            w.write_all(format!("{msg}\n").as_bytes()).await?;
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(10), r.read_line(&mut line))
                .await
                .map_err(|_| anyhow!("timed out waiting for psconnector-view"))??;
            let line = line.trim();
            match line.strip_prefix("ok") {
                Some(rest) => Ok(rest.trim().to_string()),
                None => bail!("{}", if line.is_empty() { "psconnector-view did not answer" } else { line }),
            }
        })
        .await
    }

    async fn frame(&self) -> Result<(usize, usize, Vec<u8>)> {
        self.with(async |r, w| {
            w.write_all(b"{\"frame\":true}\n").await?;
            let mut line = String::new();
            r.read_line(&mut line).await?;
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.first() != Some(&"ok") || parts.len() != 4 {
                bail!("{}", line.trim());
            }
            let (w_, h_, n): (usize, usize, usize) = (parts[1].parse()?, parts[2].parse()?, parts[3].parse()?);
            let mut buf = vec![0u8; n];
            r.read_exact(&mut buf).await?;
            Ok((w_, h_, buf))
        })
        .await
    }
}

#[derive(Clone)]
struct Psconnector {
    port: Arc<Port>,
    allow_input: bool,
    host: Option<String>,
    tool_router: ToolRouter<Self>,
}

fn ok_text(s: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(s.into())]))
}

fn fail(e: anyhow::Error) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())]))
}

macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return fail(e.into()),
        }
    };
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ScreenshotArgs {
    /// Region to crop [x, y, width, height] in 1280x720 coordinates. Whole screen if omitted.
    region: Option<[u32; 4]>,
    /// Width of the returned image in pixels. Default 960. Smaller is faster and lighter.
    width: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TapArgs {
    /// Button name (cross, circle, up, r2, ...)
    button: String,
    /// How long to hold, in seconds. Default 0.08
    hold: Option<f64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TapsArgs {
    /// Button names to tap in order
    buttons: Vec<String>,
    /// Gap between buttons, in seconds. Default 0.25
    gap: Option<f64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ButtonArgs {
    /// Button name
    button: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct StickArgs {
    /// left or right
    which: String,
    /// -1.0 (left) to 1.0 (right)
    x: f64,
    /// -1.0 (up) to 1.0 (down)
    y: f64,
    /// Hold for this many seconds, then center. If omitted the stick stays tilted.
    duration: Option<f64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TextArgs {
    /// Text to show (an empty string clears it)
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct QualityArgs {
    /// "low" (360p, for macros), "720p" or "1080p" (for playing). Omit to only report the current one.
    quality: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RunArgs {
    /// Macro name (equal to, or part of, a name in the list)
    name: String,
    /// Run time limit in minutes (0 = no limit). Defaults to the macro list setting.
    limit_min: Option<u32>,
    /// What to do when it ends by itself or at the limit: "none" or "standby" (PS5 rest mode).
    after: Option<String>,
    /// Start later instead of now: local "HH:MM" (today, or tomorrow if past) or "+MINUTES".
    at: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct StatusArgs {
    /// Number of macro output lines to include. Default 10
    lines: Option<u32>,
}

/// Crops and downscales an RGB image (box average)
fn crop_scale(w: usize, h: usize, rgb: &[u8], region: Option<[u32; 4]>, out_w: usize) -> (usize, usize, Vec<u8>) {
    let [x0, y0, rw, rh] = region.map(|r| r.map(|v| v as usize)).unwrap_or([0, 0, w, h]);
    let (x0, y0) = (x0.min(w - 1), y0.min(h - 1));
    let (rw, rh) = (rw.clamp(1, w - x0), rh.clamp(1, h - y0));
    let out_w = out_w.clamp(1, rw);
    let out_h = (rh * out_w / rw).max(1);
    let mut out = vec![0u8; out_w * out_h * 3];
    for oy in 0..out_h {
        let (sy0, sy1) = (y0 + oy * rh / out_h, (y0 + (oy + 1) * rh / out_h).max(y0 + oy * rh / out_h + 1));
        for ox in 0..out_w {
            let (sx0, sx1) = (x0 + ox * rw / out_w, (x0 + (ox + 1) * rw / out_w).max(x0 + ox * rw / out_w + 1));
            let mut acc = [0u32; 3];
            let mut n = 0u32;
            for sy in sy0..sy1.min(h) {
                for sx in sx0..sx1.min(w) {
                    let i = (sy * w + sx) * 3;
                    for c in 0..3 {
                        acc[c] += rgb[i + c] as u32;
                    }
                    n += 1;
                }
            }
            let o = (oy * out_w + ox) * 3;
            for c in 0..3 {
                out[o + c] = (acc[c] / n.max(1)) as u8;
            }
        }
    }
    (out_w, out_h, out)
}

fn png_bytes(w: usize, h: usize, rgb: &[u8]) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.write_header()?.write_image_data(rgb)?;
    }
    Ok(buf)
}

impl Psconnector {
    fn need_input(&self) -> Result<()> {
        if !self.allow_input {
            bail!(
                "input tools are disabled; register psconnector-mcp with --allow-input to use buttons, sticks and macro_run"
            );
        }
        Ok(())
    }

    async fn call(&self, msg: Value) -> Result<String> {
        self.port.call(msg).await
    }
}

#[tool_router]
impl Psconnector {
    #[tool(description = "Gets the current PS5 screen as a PNG image. Use region to zoom into part of it.")]
    async fn screenshot(&self, Parameters(a): Parameters<ScreenshotArgs>) -> Result<CallToolResult, McpError> {
        let (w, h, rgb) = tri!(self.port.frame().await);
        // regions are given in 1280x720 space; the stream may be 360p or 1080p
        let region = a.region.map(|[x, y, rw, rh]| {
            let sx = |v: u32| (u64::from(v) * w as u64 / 1280) as u32;
            let sy = |v: u32| (u64::from(v) * h as u64 / 720) as u32;
            [sx(x), sy(y), sx(rw), sy(rh)]
        });
        let (ow, oh, out) = crop_scale(w, h, &rgb, region, a.width.unwrap_or(960) as usize);
        let png = tri!(png_bytes(ow, oh, &out));
        Ok(CallToolResult::success(vec![
            ContentBlock::image(B64.encode(png), "image/png"),
            ContentBlock::text(format!("source {w}x{h}, sent {ow}x{oh}")),
        ]))
    }

    #[tool(description = "Presses and releases a button once.")]
    async fn tap(&self, Parameters(a): Parameters<TapArgs>) -> Result<CallToolResult, McpError> {
        tri!(self.need_input());
        tri!(self.call(json!({"button": a.button, "action": "tap", "hold": a.hold.unwrap_or(0.08)})).await);
        ok_text(format!("tapped {}", a.button))
    }

    #[tool(description = "Taps several buttons in order (e.g. down down cross in a menu).")]
    async fn taps(&self, Parameters(a): Parameters<TapsArgs>) -> Result<CallToolResult, McpError> {
        tri!(self.need_input());
        let gap = Duration::from_secs_f64(a.gap.unwrap_or(0.25));
        for b in &a.buttons {
            tri!(self.call(json!({"button": b, "action": "tap", "hold": 0.08})).await);
            tokio::time::sleep(gap).await;
        }
        ok_text(format!("tapped {}", a.buttons.join(" ")))
    }

    #[tool(description = "Holds a button down (use release to let go).")]
    async fn press(&self, Parameters(a): Parameters<ButtonArgs>) -> Result<CallToolResult, McpError> {
        tri!(self.need_input());
        tri!(self.call(json!({"button": a.button, "action": "press"})).await);
        ok_text(format!("pressed {}", a.button))
    }

    #[tool(description = "Releases a held button.")]
    async fn release(&self, Parameters(a): Parameters<ButtonArgs>) -> Result<CallToolResult, McpError> {
        tri!(self.need_input());
        tri!(self.call(json!({"button": a.button, "action": "release"})).await);
        ok_text(format!("released {}", a.button))
    }

    #[tool(description = "Tilts a stick. With duration, centers it again after that many seconds.")]
    async fn stick(&self, Parameters(a): Parameters<StickArgs>) -> Result<CallToolResult, McpError> {
        tri!(self.need_input());
        tri!(self.call(json!({"stick": a.which, "x": a.x, "y": a.y})).await);
        if let Some(d) = a.duration {
            tokio::time::sleep(Duration::from_secs_f64(d.clamp(0.0, 30.0))).await;
            tri!(self.call(json!({"stick": a.which, "x": 0.0, "y": 0.0})).await);
        }
        ok_text(format!("{} stick ({}, {})", a.which, a.x, a.y))
    }

    #[tool(description = "Releases all buttons and centers both sticks.")]
    async fn release_all(&self) -> Result<CallToolResult, McpError> {
        tri!(self.call(json!({"release_all": true})).await);
        ok_text("released everything")
    }

    #[tool(
        description = "Shows text in the top-left of the psconnector-view window (to tell the user what is happening)."
    )]
    async fn osd(&self, Parameters(a): Parameters<TextArgs>) -> Result<CallToolResult, McpError> {
        tri!(self.call(json!({"osd": a.text})).await);
        ok_text("shown")
    }

    #[tool(description = "Lists the macros registered in psconnector-view.")]
    async fn macro_list(&self) -> Result<CallToolResult, McpError> {
        let s = tri!(self.call(json!({"macros": true, "reload": true})).await);
        ok_text(s)
    }

    #[tool(
        description = "Runs a macro now, or schedules it with `at` (fails if one is already running). Optional run time limit and after-action (rest mode)."
    )]
    async fn macro_run(&self, Parameters(a): Parameters<RunArgs>) -> Result<CallToolResult, McpError> {
        tri!(self.need_input());
        let mut msg = json!({"macro_run": a.name});
        if let Some(m) = a.limit_min {
            msg["limit_min"] = m.into();
        }
        if let Some(after) = a.after {
            msg["after"] = after.into();
        }
        if let Some(at) = a.at {
            msg["at"] = at.into();
        }
        let reply = tri!(self.call(msg).await);
        ok_text(format!("{}: {reply}", a.name))
    }

    #[tool(
        description = "Stops the running macro (SIGINT, killed after 3 s) and cancels a scheduled one. psconnector-view then releases all buttons. The after-action is skipped."
    )]
    async fn macro_stop(&self) -> Result<CallToolResult, McpError> {
        tri!(self.call(json!({"macro_stop": true})).await);
        ok_text("stop signal sent")
    }

    #[tool(description = "Macro run status and the last few lines of its output.")]
    async fn macro_status(&self, Parameters(a): Parameters<StatusArgs>) -> Result<CallToolResult, McpError> {
        let s = tri!(self.call(json!({"macro_status": true, "lines": a.lines.unwrap_or(10)})).await);
        ok_text(s)
    }

    #[tool(
        description = "Reports or changes the picture quality of psconnector-view. Changing it reconnects (no picture for a few seconds)."
    )]
    async fn quality(&self, Parameters(a): Parameters<QualityArgs>) -> Result<CallToolResult, McpError> {
        let s = tri!(self.call(json!({"quality": a.quality.unwrap_or_default()})).await);
        ok_text(s)
    }

    #[tool(description = "Puts the PS5 into rest mode (this also ends the Remote Play connection).")]
    async fn standby(&self) -> Result<CallToolResult, McpError> {
        tri!(self.need_input());
        tri!(self.call(json!({"standby": true})).await);
        ok_text("rest mode requested")
    }

    #[tool(description = "Checks the PS5 power state (on / rest mode). Works without psconnector-view.")]
    async fn console_status(&self) -> Result<CallToolResult, McpError> {
        let host = tri!(self.host.clone().ok_or_else(|| anyhow!("PS5 address unknown (pass --host)")));
        let st = tri!(psconnector::ddp::status(&host).await);
        let viewer_on = self.call(json!({"status": true})).await.is_ok();
        ok_text(format!(
            "PS5 {} (code {}), running app: {}, psconnector-view {}",
            if st.is_on() { "on" } else { "in rest mode" },
            st.code,
            st.fields.get("running-app-name").map(String::as_str).unwrap_or("-"),
            if viewer_on { "connected" } else { "not running" }
        ))
    }

    #[tool(
        description = "Wakes a PS5 from rest mode. It takes 20-30 s to start; start psconnector-view afterwards to see the screen."
    )]
    async fn wake(&self) -> Result<CallToolResult, McpError> {
        tri!(self.need_input());
        let host = tri!(self.host.clone().ok_or_else(|| anyhow!("PS5 address unknown (pass --host)")));
        let st = tri!(psconnector::ddp::status(&host).await);
        if st.is_on() {
            return ok_text("already on");
        }
        let host_id = tri!(st.host_id().map(str::to_string).ok_or_else(|| anyhow!("no host-id")));
        let reg = tri!(psconnector::profile::load(&psconnector::profile::default_path(), None, &host_id));
        tri!(psconnector::ddp::wakeup(&host, &reg.regist_key).await);
        ok_text("wake-up sent")
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Psconnector {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(INSTRUCTIONS)
    }
}

fn default_host() -> Option<String> {
    let home = std::env::home_dir()?;
    let text = std::fs::read_to_string(home.join(".ps5rmtctl").join("config.json")).ok()?;
    serde_json::from_str::<Value>(&text).ok()?["host"].as_str().map(str::to_string)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut port, mut host, mut allow_input) = (8650u16, None, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => {
                port = args.next().and_then(|p| p.parse().ok()).ok_or_else(|| anyhow!("missing value for --port"))?
            }
            "--host" => host = args.next(),
            "--allow-input" => allow_input = true,
            other => bail!("unknown option: {other}"),
        }
    }
    let server = Psconnector {
        port: Arc::new(Port { port, conn: Mutex::new(None) }),
        allow_input,
        host: host.or_else(default_host),
        tool_router: Psconnector::tool_router(),
    };
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
