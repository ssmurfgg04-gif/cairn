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
/// ffmpeg -y -i SRC -vf scale=-2:HEIGHT -c:v libx264 -preset medium
///        -crf CRF -pix_fmt yuv420p -c:a aac -b:a 128k
///        -movflags +faststart DST
/// ```
///
/// * `-vf scale=-2:H` — width auto (even, as H.264 requires), no upscale
///   is handled by the caller choosing min(height, source height)...
///   practically we cap: scale only downscales via `scale=-2:'min(ih,H)'`.
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
        let vf = format!(
            "scale=-2:'min(ih,{})'",
            profile.max_height.max(2) // even, downscale-only
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
}
