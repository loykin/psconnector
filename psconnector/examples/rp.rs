//! Command-line tool for manual checks.
//!
//! ```text
//! cargo run -p psconnector --example rp -- <host> status
//! cargo run -p psconnector --example rp -- <host> connect [secs=10]     connect and hold without input
//! cargo run -p psconnector --example rp -- <host> tap <button>...       tap buttons in order (e.g. tap right right cross)
//! cargo run -p psconnector --example rp -- <host> snap [file=snap.png]  save one frame
//! cargo run -p psconnector --example rp -- <host> wake                  wake from rest mode
//! cargo run -p psconnector --example rp -- <host> standby
//! ```
//! Verbose logs: RUST_LOG=debug

use std::time::Duration;

use anyhow::{Result, anyhow};
use psconnector::{Button, Client, ddp};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let host = args.first().ok_or_else(|| anyhow!("usage: rp <host> status|connect|tap|snap|wake|standby"))?;
    match args.get(1).map(String::as_str).unwrap_or("status") {
        "status" => {
            let st = ddp::status(host).await?;
            println!(
                "status {} ({}), host-id {:?}",
                st.code,
                if st.is_on() { "on" } else { "rest mode" },
                st.host_id()
            );
            if let Some(app) = st.fields.get("running-app-name") {
                println!("running: {app}");
            }
        }
        "connect" => {
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
            let c = Client::connect(host, psconnector::Options { video: false, ..Default::default() }).await?;
            println!("connected, holding for {secs}s");
            for _ in 0..secs {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if !c.is_ready() {
                    println!("disconnected: {:?}", c.phase());
                    return Ok(());
                }
            }
            c.close().await;
            println!("closed cleanly");
        }
        "tap" => {
            let buttons: Vec<Button> = args[2..]
                .iter()
                .map(|s| Button::parse(s).ok_or_else(|| anyhow!("unknown button: {s}")))
                .collect::<Result<_>>()?;
            let c = Client::connect(host, psconnector::Options { video: false, ..Default::default() }).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            for b in buttons {
                println!("tap {}", b.name());
                c.tap(b, Duration::from_millis(80)).await;
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            c.close().await;
        }
        "snap" => {
            // save one frame as PNG
            let out = args.get(2).cloned().unwrap_or_else(|| "snap.png".into());
            let mut c = Client::connect(host, Default::default()).await?;
            let t0 = std::time::Instant::now();
            let mut n = 0;
            let frame = loop {
                let f = tokio::time::timeout(Duration::from_secs(10), c.next_frame()).await.ok().flatten();
                let Some(f) = f else { return Err(anyhow!("no frame within 10 s")) };
                n += 1;
                if n >= 30 {
                    break f;
                } // take a few frames so the picture settles after the key frame
            };
            println!(
                "{}x{}, {} frames in {:.1}s ({:.0} fps)",
                frame.width,
                frame.height,
                n,
                t0.elapsed().as_secs_f32(),
                n as f32 / t0.elapsed().as_secs_f32()
            );
            let file = std::io::BufWriter::new(std::fs::File::create(&out)?);
            let mut enc = png::Encoder::new(file, frame.width as u32, frame.height as u32);
            enc.set_color(png::ColorType::Rgb);
            enc.write_header()?.write_image_data(&frame.rgb)?;
            println!("saved: {out}");
            c.close().await;
        }
        "wake" => {
            let st = ddp::status(host).await?;
            let host_id = st.host_id().ok_or_else(|| anyhow!("no host-id"))?;
            let reg = psconnector::profile::load(&psconnector::profile::default_path(), None, host_id)?;
            ddp::wakeup(host, &reg.regist_key).await?;
            println!("wake-up sent; the console takes 20-30 s to start");
        }
        "standby" => {
            let c = Client::connect(host, psconnector::Options { video: false, ..Default::default() }).await?;
            c.standby();
            tokio::time::sleep(Duration::from_secs(2)).await;
            c.close().await;
        }
        other => return Err(anyhow!("unknown command: {other}")),
    }
    Ok(())
}
