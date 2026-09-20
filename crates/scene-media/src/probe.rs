//! `ffprobe -print_format json` → typed `MediaInfo`.
//!
//! The JSON parsing is a pure function — tests feed it fixtures, the
//! subprocess wrapper is thin.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use scene_ir::Rational;
use serde::Deserialize;

use crate::error::{MediaError, Tool};

/// What a media file contains, reduced to what the engine needs.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    /// Container duration in seconds (best available).
    pub duration_s: f64,
    pub format_name: String,
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub codec: String,
    /// Average frame rate; `None` for still images / unknown.
    pub frame_rate: Option<Rational>,
    pub pixel_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioInfo {
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
}

// --- ffprobe JSON shape ----------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: Option<ProbeFormat>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    /// e.g. "30/1", "30000/1001", "0/0" for stills.
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    pix_fmt: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeFormat {
    format_name: Option<String>,
    duration: Option<String>,
}

/// Parse an ffprobe rate string. `"0/0"` (stills) and garbage → `None`.
fn parse_rate(s: &str) -> Option<Rational> {
    let (n, d) = s.split_once('/')?;
    let numerator: u32 = n.trim().parse().ok()?;
    let denominator: u32 = d.trim().parse().ok()?;
    if numerator == 0 || denominator == 0 {
        return None;
    }
    Some(Rational {
        numerator,
        denominator,
    })
}

fn parse_f64(s: &str) -> Option<f64> {
    s.trim().parse().ok().filter(|v: &f64| v.is_finite())
}

/// Parse ffprobe JSON into `MediaInfo`. Pure — feed it a fixture.
pub fn parse_probe_json(json: &str) -> Result<MediaInfo, MediaError> {
    let out: ProbeOutput =
        serde_json::from_str(json).map_err(|e| MediaError::ProbeParse(e.to_string()))?;

    let video_stream = out
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"));
    let audio_stream = out
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"));

    let video = video_stream.map(|s| VideoInfo {
        width: s.width.unwrap_or(0),
        height: s.height.unwrap_or(0),
        codec: s.codec_name.clone().unwrap_or_default(),
        frame_rate: s
            .avg_frame_rate
            .as_deref()
            .and_then(parse_rate)
            .or_else(|| s.r_frame_rate.as_deref().and_then(parse_rate)),
        pixel_format: s.pix_fmt.clone(),
    });

    let audio = audio_stream.map(|s| AudioInfo {
        codec: s.codec_name.clone().unwrap_or_default(),
        sample_rate: s.sample_rate.as_deref().and_then(|r| r.trim().parse().ok()),
        channels: s.channels,
    });

    // Duration: prefer the container's, fall back to any stream's.
    let duration_s = out
        .format
        .as_ref()
        .and_then(|f| f.duration.as_deref())
        .and_then(parse_f64)
        .or_else(|| {
            out.streams
                .iter()
                .filter_map(|s| s.duration.as_deref())
                .filter_map(parse_f64)
                .fold(None, |acc: Option<f64>, v| {
                    Some(acc.map_or(v, |a: f64| a.max(v)))
                })
        })
        .unwrap_or(0.0);

    Ok(MediaInfo {
        duration_s,
        format_name: out.format.and_then(|f| f.format_name).unwrap_or_default(),
        video,
        audio,
    })
}

/// ffprobe is a local metadata read — seconds, never minutes. Anything
/// past this is a wedged process, not a slow one.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `ffprobe` on a file. Requires ffprobe on PATH (or `FFPROBE` env).
pub fn probe(path: &Path) -> Result<MediaInfo, MediaError> {
    let tool = std::env::var("FFPROBE").unwrap_or_else(|_| Tool::Ffprobe.name().to_string());
    let output = crate::proc::output_timeout(
        Command::new(&tool)
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
            ])
            .arg(path),
        "ffprobe",
        PROBE_TIMEOUT,
    )?;
    if !output.status.success() {
        return Err(MediaError::Failed {
            tool: "ffprobe",
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    parse_probe_json(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MP4_H264_AAC: &str = r#"{
        "streams": [
            {
                "codec_name": "h264", "codec_type": "video",
                "width": 1920, "height": 1080, "pix_fmt": "yuv420p",
                "r_frame_rate": "30/1", "avg_frame_rate": "30/1",
                "duration": "3.033333"
            },
            {
                "codec_name": "aac", "codec_type": "audio",
                "sample_rate": "48000", "channels": 2,
                "duration": "3.04"
            }
        ],
        "format": { "format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "3.045" }
    }"#;

    #[test]
    fn mp4_with_audio() {
        let info = parse_probe_json(MP4_H264_AAC).unwrap();
        assert_eq!(info.duration_s, 3.045);
        assert_eq!(info.format_name, "mov,mp4,m4a,3gp,3g2,mj2");
        let v = info.video.unwrap();
        assert_eq!((v.width, v.height), (1920, 1080));
        assert_eq!(v.codec, "h264");
        assert_eq!(
            v.frame_rate,
            Some(Rational {
                numerator: 30,
                denominator: 1
            })
        );
        let a = info.audio.unwrap();
        assert_eq!(a.codec, "aac");
        assert_eq!(a.sample_rate, Some(48000));
        assert_eq!(a.channels, Some(2));
    }

    #[test]
    fn still_image_zero_rate() {
        // PNGs report "0/0" — must become None, not a div-by-zero Rational
        let json = r#"{
            "streams": [{
                "codec_name": "png", "codec_type": "video",
                "width": 640, "height": 480,
                "avg_frame_rate": "0/0", "r_frame_rate": "25/1"
            }],
            "format": {"format_name": "image2", "duration": "0.04"}
        }"#;
        let info = parse_probe_json(json).unwrap();
        // avg is 0/0 → falls back to r_frame_rate 25/1
        assert_eq!(
            info.video.unwrap().frame_rate,
            Some(Rational {
                numerator: 25,
                denominator: 1
            })
        );
    }

    #[test]
    fn ntsc_rate_preserved() {
        let json = r#"{
            "streams": [{
                "codec_name": "h264", "codec_type": "video",
                "width": 1280, "height": 720,
                "avg_frame_rate": "30000/1001", "r_frame_rate": "30000/1001"
            }],
            "format": {"format_name": "mov", "duration": "10.01"}
        }"#;
        let info = parse_probe_json(json).unwrap();
        assert_eq!(
            info.video.unwrap().frame_rate,
            Some(Rational {
                numerator: 30000,
                denominator: 1001
            })
        );
    }

    #[test]
    fn audio_only() {
        let json = r#"{
            "streams": [{
                "codec_name": "mp3", "codec_type": "audio",
                "sample_rate": "44100", "channels": 2, "duration": "61.2"
            }],
            "format": {"format_name": "mp3", "duration": "61.2"}
        }"#;
        let info = parse_probe_json(json).unwrap();
        assert!(info.video.is_none());
        assert_eq!(info.audio.unwrap().sample_rate, Some(44100));
        assert_eq!(info.duration_s, 61.2);
    }

    #[test]
    fn missing_format_duration_falls_back_to_stream() {
        let json = r#"{
            "streams": [{
                "codec_name": "vp9", "codec_type": "video",
                "width": 320, "height": 240,
                "avg_frame_rate": "30/1", "duration": "7.5"
            }],
            "format": {"format_name": "matroska,webm"}
        }"#;
        let info = parse_probe_json(json).unwrap();
        assert_eq!(info.duration_s, 7.5);
    }

    #[test]
    fn empty_streams_is_not_an_error() {
        let info = parse_probe_json(r#"{"streams": [], "format": {"format_name": "x"}}"#).unwrap();
        assert!(info.video.is_none());
        assert!(info.audio.is_none());
        assert_eq!(info.duration_s, 0.0);
    }

    #[test]
    fn garbage_is_a_parse_error() {
        assert!(matches!(
            parse_probe_json("not json"),
            Err(MediaError::ProbeParse(_))
        ));
    }
}
