//! Inline kitty-graphics viewer for inbound share streams.
//!
//! Each `ViewerHandle` owns an `ffmpeg` child that pipes BMP frames on stdout,
//! a decode task that turns those bytes into `DynamicImage` values, and a slot
//! holding the most-recent frame. After the TUI finishes its ratatui draw, the
//! event loop reads the slot and paints it via `viuer` over the Stream pane.
//!
//! BMP is used because it's trivially length-prefixed (file size at offset 2),
//! so framing in a streaming byte buffer doesn't need a format-aware parser.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use image::DynamicImage;

use crate::debug_log;
use crate::protocol::ShareKind;

pub(crate) struct ViewerHandle {
    pub kind: ShareKind,
    pub frame: Arc<Mutex<Option<DynamicImage>>>,
    child_pid: Option<u32>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl ViewerHandle {
    pub fn spawn(url: String, kind: ShareKind) -> Option<Self> {
        // 480p @ 15fps BMP — small enough to decode in real time, big enough to read.
        // Default to `warning` so pipeline failures land in the log instead of /dev/null.
        let tpl = std::env::var("DUSK_VIEW_INLINE").unwrap_or_else(|_| {
            "ffmpeg -loglevel warning -fflags nobuffer -flags low_delay \
             -i tcp://{addr}?timeout=10000000 -vf scale=640:-2,fps=15 -f image2pipe -vcodec bmp -"
                .into()
        });
        let cmd = tpl.replace("{addr}", &url);

        debug_log::log(format!("viewer[{kind:?}] spawn url={url} cmd=`{cmd}`"));

        // ffmpeg stderr → /tmp/dusk-view.log so the user can `tail -f` it when
        // reproducing "blank stream" failures. Falls back to null on any error.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/dusk-view.log")
            .ok()
            .map(Stdio::from)
            .unwrap_or_else(Stdio::null);

        let mut child: Child = match Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log)
            .process_group(0)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                debug_log::log(format!("viewer[{kind:?}] spawn failed: {e}"));
                return None;
            }
        };

        let pid = child.id();
        let Some(stdout) = child.stdout.take() else {
            debug_log::log(format!("viewer[{kind:?}] pid={pid} stdout pipe missing"));
            return None;
        };
        debug_log::log(format!("viewer[{kind:?}] pid={pid} spawned"));

        let frame: Arc<Mutex<Option<DynamicImage>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let frame_w = frame.clone();
        let stop_w = stop.clone();
        thread::spawn(move || decode_loop(stdout, frame_w, stop_w, kind, pid));

        // Reap if it exits early so we don't leave zombies.
        thread::spawn(move || {
            match child.wait() {
                Ok(status) => debug_log::log(format!(
                    "viewer[{kind:?}] pid={pid} exited status={status}"
                )),
                Err(e) => debug_log::log(format!(
                    "viewer[{kind:?}] pid={pid} wait error: {e}"
                )),
            }
        });

        Some(Self { kind, frame, child_pid: Some(pid), stop })
    }
}

impl Drop for ViewerHandle {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(pid) = self.child_pid.take() {
            debug_log::log(format!("viewer[{:?}] pid={pid} dropped, killing", self.kind));
            let _ = Command::new("kill").arg(format!("-{pid}")).status();
        }
    }
}

fn decode_loop(
    mut stdout: impl Read,
    frame: Arc<Mutex<Option<DynamicImage>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    kind: ShareKind,
    pid: u32,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut tmp = [0u8; 16 * 1024];
    let mut total_bytes: u64 = 0;
    let mut frames: u64 = 0;
    let mut decode_errs: u64 = 0;
    let mut last_log_frames: u64 = 0;
    let mut first_byte_logged = false;
    debug_log::log(format!("decode[{kind:?}] pid={pid} loop start"));
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        match stdout.read(&mut tmp) {
            Ok(0) => {
                debug_log::log(format!(
                    "decode[{kind:?}] pid={pid} EOF after {total_bytes}B, {frames} frames, {decode_errs} decode-errs"
                ));
                break;
            }
            Ok(n) => {
                if !first_byte_logged {
                    debug_log::log(format!("decode[{kind:?}] pid={pid} first read n={n}"));
                    first_byte_logged = true;
                }
                total_bytes += n as u64;
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(e) => {
                debug_log::log(format!(
                    "decode[{kind:?}] pid={pid} read err: {e} after {total_bytes}B, {frames} frames"
                ));
                break;
            }
        }
        while let Some(bmp) = next_bmp(&mut buf) {
            match image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp) {
                Ok(img) => {
                    frames += 1;
                    if let Ok(mut slot) = frame.lock() {
                        *slot = Some(img);
                    }
                    if frames == 1 || frames - last_log_frames >= 30 {
                        debug_log::log(format!(
                            "decode[{kind:?}] pid={pid} frames={frames} bytes={total_bytes}"
                        ));
                        last_log_frames = frames;
                    }
                }
                Err(e) => {
                    decode_errs += 1;
                    if decode_errs <= 3 || decode_errs % 30 == 0 {
                        debug_log::log(format!(
                            "decode[{kind:?}] pid={pid} bmp decode err: {e} (count={decode_errs})"
                        ));
                    }
                }
            }
        }
    }
}

// BMP framer: a complete BMP file is 14-byte BITMAPFILEHEADER (starts "BM")
// followed by file_size-14 more bytes; file_size is u32 little-endian at offset 2.
fn next_bmp(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    // Sync to "BM" — if junk leads the buffer, drop it.
    if buf.len() < 6 { return None; }
    if &buf[..2] != b"BM" {
        if let Some(p) = buf.windows(2).position(|w| w == b"BM") {
            buf.drain(..p);
            if buf.len() < 6 { return None; }
        } else {
            buf.clear();
            return None;
        }
    }
    let size = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]) as usize;
    if size < 14 || size > 16 * 1024 * 1024 { // sanity bound
        buf.drain(..2);
        return None;
    }
    if buf.len() < size { return None; }
    Some(buf.drain(..size).collect())
}
