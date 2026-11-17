//! The psconnector-view command port (127.0.0.1:8650): what goes over it, shared by the window (which serves
//! it), psconnector-mcp and the recording replay (which use it). `python/psconnector.py` speaks the same
//! protocol from Python.
//!
//! Each command is one JSON object on one line ([`Command`]). Each reply is one line: `ok`, `ok TEXT` or
//! `err REASON`. Only `frame` and `thumb` add bytes: their line is `ok WIDTH HEIGHT BYTES` and the RGB bytes
//! follow ([`image_header`]).

mod command;
pub mod settings;

pub use command::touch_point;
pub use command::{BadCommand, ButtonAction, Command, InputBench, OcrRegion, PictureChange, RunOptions, TAP_HOLD};

/// The port psconnector-view listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 8650;

/// Environment variable that gives a macro process the port of the window that started it.
pub const PORT_ENV: &str = "PSCONNECTOR_PORT";

/// The port from [`PORT_ENV`], or [`DEFAULT_PORT`].
pub fn port_from_env() -> u16 {
    std::env::var(PORT_ENV).ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}

/// What follows `ok` on a reply line (trimmed), or the whole line as the error.
pub fn parse_reply(line: &str) -> Result<&str, String> {
    let line = line.trim();
    match line.strip_prefix("ok") {
        Some(rest) => Ok(rest.trim()),
        None if line.is_empty() => Err("psconnector-view did not answer".into()),
        None => Err(line.to_string()),
    }
}

/// The reply line in front of an image's RGB bytes.
pub fn image_header(width: usize, height: usize, bytes: usize) -> String {
    format!("ok {width} {height} {bytes}\n")
}

/// Width, height and byte count from an [`image_header`] line.
pub fn parse_image_header(line: &str) -> Result<(usize, usize, usize), String> {
    let rest = parse_reply(line)?;
    let v: Vec<usize> = rest.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    match v[..] {
        [w, h, n] if rest.split_whitespace().count() == 3 => Ok((w, h, n)),
        _ => Err(format!("bad image reply: {}", line.trim())),
    }
}

/// PNG of 8-bit RGB (or RGBA with `alpha`) pixels, compressed fast.
pub fn png(width: usize, height: usize, pixels: &[u8], alpha: bool) -> Result<Vec<u8>, png::EncodingError> {
    let mut buf = Vec::new();
    write_png(&mut buf, width, height, pixels, alpha)?;
    Ok(buf)
}

/// Writes a [`png`] to `out`.
pub fn write_png(
    out: impl std::io::Write,
    width: usize,
    height: usize,
    pixels: &[u8],
    alpha: bool,
) -> Result<(), png::EncodingError> {
    let mut enc = png::Encoder::new(out, width as u32, height as u32);
    enc.set_color(if alpha { png::ColorType::Rgba } else { png::ColorType::Rgb });
    enc.set_compression(png::Compression::Fast);
    enc.write_header()?.write_image_data(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies() {
        assert_eq!(parse_reply("ok\n"), Ok(""));
        assert_eq!(parse_reply("ok yes\n"), Ok("yes"));
        assert_eq!(parse_reply("err no frame yet\n"), Err("err no frame yet".into()));
        assert_eq!(parse_reply(""), Err("psconnector-view did not answer".into()));
        assert_eq!(parse_image_header(&image_header(80, 45, 10800)), Ok((80, 45, 10800)));
        assert!(parse_image_header("ok 80 45\n").is_err());
        assert!(parse_image_header("err no frame yet\n").is_err());
    }
}
