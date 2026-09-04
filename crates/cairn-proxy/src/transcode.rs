//! Transcoders: the pluggable ffmpeg boundary. The pipeline never links
//! codecs — it shells out (or swaps in a test double), which keeps the
//! crate dependency-free and the audit surface tiny.

use std::path::Path;
use std::process::Command;

/// What one transcode run produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscodeOutput {
    pub bytes: u64,
}

/// A transcoder turns (source media, profile) into a proxy file at `dst`.
/// Implementations MUST be deterministic for the same (source, profile):
/// the index assumes proxy = f(source bytes, profile).
pub trait Transcoder {
    /// Human name (for logs and errors).
    fn name(&self) -> &'static str;
    /// Whether this transcoder is usable in the current environment.
    fn available(&self) -> bool;
    /// Transcode `src` → `dst` honoring the profile (height cap, quality).
    fn transcode(
        &self,
        src: &Path,
        dst: &Path,
        profile: &super::model::ProxyProfile,
    ) -> Result<TranscodeOutput, String>;
}

/// The real one: ffmpeg. Invoked with:
///
/// ```text
/// ffmpeg -y -i SRC -vf scale=-2:'trunc(min(ih,H)/2)*2',setsar=1
///        -c:v libx264 -preset medium -crf CRF -pix_fmt yuv420p
///        -c:a aac -b:a 128k -movflags +faststart DST
/// ```
///
/// * `-vf scale=-2:'trunc(min(ih,H)/2)*2'` — BOTH dimensions forced even
///   (H.264/yuv420p requirement; `-2` alone only fixes width — odd-HEIGHT
///   sources like 321x179 used to fail the encode outright, which the
///   first dogfood run caught), downscale-only via `min(ih,H)`.
/// * `setsar=1` — square pixels so a ±1px rounded dimension can never
///   stretch the proxy against the original's aspect.
/// * `-movflags +faststart` — the moov atom moves to the front so the
///   review player scrubs over HTTP ranges without full download.
#[derive(Clone, Copy, Debug, Default)]
pub struct FfmpegTranscoder;

impl FfmpegTranscoder {
    pub fn detect_ffmpeg() -> Option<std::path::PathBuf> {
        let candidates = [
            "ffmpeg",
            "ffmpeg.exe",
            "/usr/local/bin/ffmpeg",
            "/opt/homebrew/bin/ffmpeg",
        ];
        for c in candidates {
            if Command::new(c)
                .arg("-version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
            {
                return Some(std::path::PathBuf::from(c));
            }
        }
        None
    }
}

impl Transcoder for FfmpegTranscoder {
    fn name(&self) -> &'static str {
        "ffmpeg"
    }

    fn available(&self) -> bool {
        Self::detect_ffmpeg().is_some()
    }

    fn transcode(
        &self,
        src: &Path,
        dst: &Path,
        profile: &super::model::ProxyProfile,
    ) -> Result<TranscodeOutput, String> {
        let ffmpeg = Self::detect_ffmpeg().ok_or_else(|| "ffmpeg not found".to_string())?;
        // even dims on BOTH axes (see module doc), downscale-only
        let vf = format!(
            "scale=-2:'trunc(min(ih,{})/2)*2',setsar=1",
            profile.max_height.max(2)
        );
        let out = Command::new(&ffmpeg)
            .arg("-y")
            .arg("-i")
            .arg(src)
            .arg("-vf")
            .arg(&vf)
            .arg("-c:v")
            .arg("libx264")
            .arg("-preset")
            .arg("medium")
            .arg("-crf")
            .arg(profile.crf.to_string())
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg("-c:a")
            .arg("aac")
            .arg("-b:a")
            .arg("128k")
            .arg("-movflags")
            .arg("+faststart")
            .arg(dst)
            .output()
            .map_err(|e| format!("spawn ffmpeg: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "ffmpeg exit {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
                    .chars()
                    .take(400)
                    .collect::<String>()
            ));
        }
        let bytes = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
        Ok(TranscodeOutput { bytes })
    }
}

/// The test/CI double: byte-copy the source. Deterministic, dependency
/// free, and exercises the whole pipeline (digest → index → staleness →
/// portal streaming) without a codec. NEVER use for real media — a copy
/// of ARRIRAW is not a proxy.
#[derive(Clone, Copy, Debug, Default)]
pub struct CopyTranscoder;

impl Transcoder for CopyTranscoder {
    fn name(&self) -> &'static str {
        "copy"
    }

    fn available(&self) -> bool {
        true
    }

    fn transcode(
        &self,
        src: &Path,
        dst: &Path,
        _profile: &super::model::ProxyProfile,
    ) -> Result<TranscodeOutput, String> {
        std::fs::copy(src, dst)
            .map(|bytes| TranscodeOutput { bytes })
            .map_err(|e| format!("copy: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_transcoder_is_deterministic_and_reports_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.mov");
        std::fs::write(&src, b"0123456789").unwrap();
        let dst = dir.path().join("a-proxy.mp4");
        let t = CopyTranscoder;
        assert!(t.available());
        let out = t
            .transcode(&src, &dst, &crate::model::ProxyProfile::default())
            .unwrap();
        assert_eq!(out.bytes, 10);
        assert_eq!(std::fs::read(&dst).unwrap(), b"0123456789");
    }

    #[test]
    fn copy_transcoder_fails_on_missing_source() {
        let dir = tempfile::tempdir().unwrap();
        let r = CopyTranscoder.transcode(
            &dir.path().join("nope.mov"),
            &dir.path().join("x.mp4"),
            &crate::model::ProxyProfile::default(),
        );
        assert!(r.is_err());
    }

    /// The 1px/odd-dimension proxy regression, against real ffmpeg: an
    /// odd source (321x179) used to fail the encode outright (x264 needs
    /// even width AND height; `-2` only fixed width). Skipped when ffmpeg
    /// is absent — CI installs it, dev boxes usually have it.
    #[test]
    fn ffmpeg_produces_even_dimensions_from_odd_sources() {
        let t = FfmpegTranscoder;
        if !t.available() {
            eprintln!("skipping: no ffmpeg");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // odd-odd source: the worst case for even-dimension scaling
        let src = dir.path().join("odd.mov");
        let gen = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=321x179:rate=24",
                "-t",
                "1",
                "-c",
                "ffv1",
            ])
            .arg(&src)
            .output()
            .expect("spawn ffmpeg for source");
        assert!(gen.status.success(), "source generation failed");
        let dst = dir.path().join("odd-proxy.mp4");
        let profile = crate::model::ProxyProfile {
            max_height: 1080,
            ..Default::default()
        };
        t.transcode(&src, &dst, &profile)
            .expect("odd source must transcode");
        // probe the output: both dimensions even, aspect within 1%
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "csv=p=0",
            ])
            .arg(&dst)
            .output()
            .expect("spawn ffprobe");
        let dims = String::from_utf8_lossy(&probe.stdout);
        let dims = dims.trim();
        let (w, h): (u64, u64) = dims
            .split_once(',')
            .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
            .expect("probe dims");
        assert_eq!(w % 2, 0, "width must be even, got {dims}");
        assert_eq!(h % 2, 0, "height must be even, got {dims}");
        let src_ar = 321.0 / 179.0;
        let out_ar = w as f64 / h as f64;
        assert!(
            (src_ar - out_ar).abs() / src_ar < 0.02,
            "aspect drift too large: {src_ar} vs {out_ar}"
        );
    }
}
