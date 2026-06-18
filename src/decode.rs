// HEVC decode via an `ffmpeg` subprocess: Annex-B in on stdin, PPM frames out on
// stdout. PPM is self-describing, so resolution/rotation changes need no extra
// signalling.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Notify;

use crate::protocol::{Frame, FrameSlot};

/// Spawn `ffmpeg` decoding raw HEVC (Annex-B on stdin) to PPM frames on stdout.
/// stderr is piped so the session can watch it for decode errors.
pub fn spawn_ffmpeg() -> std::io::Result<(Child, ChildStdin, ChildStdout, ChildStderr)> {
    let mut child = Command::new("ffmpeg")
        // Do *not* add `-fflags nobuffer`: it makes ffmpeg skip the opening IDR +
        // parameter sets, so every P-frame fails with "Could not find ref".
        .args(["-flags", "low_delay"])
        .args(["-f", "hevc", "-i", "pipe:0"])
        .args([
            "-an",
            "-f",
            "image2pipe",
            "-vcodec",
            "ppm",
            "-pix_fmt",
            "rgb24",
        ])
        .arg("pipe:1")
        .args(["-loglevel", "error"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let stdin = child.stdin.take().expect("ffmpeg stdin piped");
    let stdout = child.stdout.take().expect("ffmpeg stdout piped");
    let stderr = child.stderr.take().expect("ffmpeg stderr piped");
    Ok((child, stdin, stdout, stderr))
}

/// Read PPM frames from ffmpeg's stdout, publishing each as RGBA into `slot` and
/// waking the UI via `repaint`. Each frame pulses `beat`, the liveness heartbeat
/// watched by the session's stall watchdog. Returns when the stream ends.
pub async fn read_frames(
    stdout: ChildStdout,
    slot: FrameSlot,
    beat: Arc<Notify>,
    repaint: impl Fn(),
) {
    let mut reader = BufReader::new(stdout);
    let mut last_dims: Option<(usize, usize)> = None;
    // A static screen still streams at display rate (HEVC emits near-empty
    // P-frames), so skip publishing identical frames to let the UI go idle.
    let mut last_rgb: Vec<u8> = Vec::new();
    loop {
        match read_ppm(&mut reader).await {
            Ok(Some((width, height, rgb))) => {
                let dims = (width, height);
                if last_dims != Some(dims) {
                    tracing::info!("decoded frame size: {}x{}", dims.0, dims.1);
                    last_dims = Some(dims);
                }

                // Pulse even for duplicate frames: a frozen-but-streaming
                // screen is still a healthy stream.
                beat.notify_one();

                if rgb == last_rgb {
                    continue;
                }

                slot.publish(rgb_to_frame(width, height, &rgb));
                last_rgb = rgb;
                repaint();
            }
            Ok(None) => {
                tracing::info!("ffmpeg stdout closed");
                break;
            }
            Err(e) => {
                tracing::warn!("ppm read error: {e}");
                break;
            }
        }
    }
}

/// Expand a top-down RGB raster to an opaque RGBA [`Frame`].
fn rgb_to_frame(width: usize, height: usize, rgb: &[u8]) -> Frame {
    let mut rgba = vec![0u8; width * height * 4];
    for (src, dst) in rgb.chunks_exact(3).zip(rgba.chunks_exact_mut(4)) {
        dst[0] = src[0];
        dst[1] = src[1];
        dst[2] = src[2];
        dst[3] = 255;
    }
    Frame {
        width,
        height,
        rgba,
    }
}

/// Read a single binary PPM (P6) image as a raw top-down RGB raster. Returns
/// `Ok(None)` at clean EOF.
async fn read_ppm<R: AsyncReadExt + Unpin>(
    r: &mut R,
) -> std::io::Result<Option<(usize, usize, Vec<u8>)>> {
    let mut magic = [0u8; 2];
    match r.read_exact(&mut magic).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    if &magic != b"P6" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected PPM 'P6' magic, got {magic:?}"),
        ));
    }

    let width: usize = read_header_uint(r).await?;
    let height: usize = read_header_uint(r).await?;
    let maxval: usize = read_header_uint(r).await?;
    if maxval == 0 || maxval > 255 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported PPM maxval {maxval}"),
        ));
    }

    // `read_header_uint` already consumed the whitespace byte after maxval, so
    // the raster starts here — do NOT consume another or every frame desyncs.
    let mut rgb = vec![0u8; width * height * 3];
    r.read_exact(&mut rgb).await?;

    Ok(Some((width, height, rgb)))
}

/// Read a whitespace-delimited ASCII unsigned integer from a PPM header,
/// skipping leading whitespace and `#` comment lines.
async fn read_header_uint<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<usize> {
    let mut b = [0u8; 1];
    loop {
        r.read_exact(&mut b).await?;
        match b[0] {
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            b'#' => {
                while b[0] != b'\n' {
                    r.read_exact(&mut b).await?;
                }
            }
            _ => break,
        }
    }
    let mut value: usize = 0;
    while b[0].is_ascii_digit() {
        value = value * 10 + (b[0] - b'0') as usize;
        r.read_exact(&mut b).await?;
    }
    Ok(value)
}
