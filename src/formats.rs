//! Turning yt-dlp's format list into a short menu.
//!
//! Formats that would look the same in the menu are merged, keeping the
//! best one (yt-dlp lists formats worst-first).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Deserialize, Clone, Debug, Default)]
pub struct RawFormat {
    pub format_id: String,
    pub ext: Option<String>,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub dynamic_range: Option<String>,
    pub tbr: Option<f64>,
    pub abr: Option<f64>,
    pub language: Option<String>,
    pub protocol: Option<String>,
    pub filesize: Option<f64>,
    pub filesize_approx: Option<f64>,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Best,
    Video,
    Audio,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Choice {
    pub kind: Kind,
    /// Headline, e.g. "1080p60" or "Opus".
    pub quality: String,
    /// The rest, e.g. "MP4 · H.264 · HDR".
    pub detail: String,
    /// Approximate size in bytes, including the audio paired with a video.
    pub size: Option<u64>,
    /// Value for `yt-dlp -f`.
    pub format: String,
    /// Shown in the short list: one video per resolution, the one most
    /// devices can play.
    pub primary: bool,
}

pub const BEST: &str = "bv*+ba/b";

pub fn choices(formats: &[RawFormat], duration: Option<f64>) -> Vec<Choice> {
    let mut video: HashMap<(String, String), Pick> = HashMap::new();
    let mut audio: HashMap<(String, String), Pick> = HashMap::new();
    // Best audio per container, to pair with video-only formats.
    let mut audio_by_ext: HashMap<&str, Pick> = HashMap::new();

    for f in formats {
        let vcodec = f.vcodec.as_deref().unwrap_or("");
        let acodec = f.acodec.as_deref().unwrap_or("");
        let ext = f.ext.as_deref().unwrap_or("?");
        // Storyboards and other images carry neither stream.
        if (vcodec == "none" && acodec == "none") || ext == "mhtml" {
            continue;
        }
        // Direct downloads beat HLS/DASH fragments, and the original audio
        // beats YouTube's dynamic-range-compressed ("-drc") copy.
        let direct = f.protocol.as_deref().is_none_or(|p| p.starts_with("http") && !p.contains("m3u8"));
        let drc = f.format_id.ends_with("-drc");
        let preferred = u32::from(direct) * 2 + u32::from(!drc);
        if vcodec == "none" {
            if acodec.is_empty() {
                continue;
            }
            let codec = audio_codec(acodec, ext);
            let lang = f.language.as_deref().filter(|l| !l.is_empty() && *l != "und").unwrap_or("");
            let rank = (0, 0, preferred, kbps(f.abr.or(f.tbr)));
            keep_best(&mut audio, (codec, lang.to_string()), Pick { f, rank });
            let container = if ext == "m4a" { "mp4" } else { ext };
            keep_best(&mut audio_by_ext, container, Pick { f, rank });
            keep_best(&mut audio_by_ext, "*", Pick { f, rank });
            continue;
        }
        let Some(height) = f.height.filter(|h| *h > 0) else { continue };
        let fps = f.fps.map(|v| v.round() as u32).unwrap_or(0);
        let hdr = f.dynamic_range.as_deref().is_some_and(|d| d != "SDR");
        let quality = if fps > 30 { format!("{height}p{fps}") } else { format!("{height}p") };
        let mut detail = ext.to_uppercase();
        if let Some(codec) = video_codec(vcodec) {
            detail += &format!(" · {codec}");
        }
        if hdr {
            detail += " · HDR";
        }
        let rank = (height, fps, preferred, kbps(f.tbr));
        keep_best(&mut video, (quality, detail), Pick { f, rank });
    }

    let mut out = vec![Choice {
        kind: Kind::Best,
        quality: "Best".into(),
        detail: "Highest quality, chosen by yt-dlp".into(),
        size: None,
        format: BEST.into(),
        primary: true,
    }];
    let video: Vec<_> = sorted(video).collect();
    // Index of the most compatible format for each resolution.
    let mut primary: HashMap<&str, usize> = HashMap::new();
    for (i, ((quality, _), p)) in video.iter().enumerate() {
        if primary.get(quality.as_str()).is_none_or(|&j| compatibility(p.f) > compatibility(video[j].1.f)) {
            primary.insert(quality, i);
        }
    }
    let primary: Vec<usize> = primary.into_values().collect();
    out.extend(video.into_iter().enumerate().map(|(i, ((quality, detail), p))| {
        let is_primary = primary.contains(&i);
        let video_only = p.f.acodec.as_deref().is_none_or(|a| a == "none");
        let size = size_of(p.f, duration).map(|v| {
            let paired = if video_only {
                let container = p.f.ext.as_deref().unwrap_or("");
                audio_by_ext.get(container).or(audio_by_ext.get("*")).and_then(|a| size_of(a.f, duration))
            } else {
                None
            };
            v + paired.unwrap_or(0)
        });
        Choice { kind: Kind::Video, quality, detail, size, format: video_selector(p.f), primary: is_primary }
    }));
    out.extend(sorted(audio).map(|((codec, lang), p)| {
        let mut detail = p.f.ext.as_deref().unwrap_or("?").to_uppercase();
        if let Some(rate) = p.f.abr.or(p.f.tbr).filter(|r| *r > 0.0) {
            detail += &format!(" · {} kbps", rate.round());
        }
        if !lang.is_empty() {
            detail += &format!(" · {lang}");
        }
        Choice {
            kind: Kind::Audio,
            quality: codec,
            detail,
            size: size_of(p.f, duration),
            format: p.f.format_id.clone(),
            primary: true,
        }
    }));
    out
}

struct Pick<'a> {
    f: &'a RawFormat,
    /// Height, fps, preference flags, bitrate.
    rank: (u32, u32, u32, u64),
}

fn sorted<K: Ord>(map: HashMap<K, Pick<'_>>) -> impl Iterator<Item = (K, Pick<'_>)> {
    let mut v: Vec<(K, Pick)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.rank.cmp(&a.1.rank).then_with(|| a.0.cmp(&b.0)));
    v.into_iter()
}

fn keep_best<'a, K: std::hash::Hash + Eq>(map: &mut HashMap<K, Pick<'a>>, key: K, new: Pick<'a>) {
    if map.get(&key).is_none_or(|old| new.rank > old.rank) {
        map.insert(key, new);
    }
}

fn kbps(rate: Option<f64>) -> u64 {
    rate.filter(|r| r.is_finite() && *r > 0.0).map(|r| (r * 1000.0) as u64).unwrap_or(0)
}

/// How widely a video format plays: SDR before HDR, then H.264, AV1, VP9,
/// then MP4 over other containers.
fn compatibility(f: &RawFormat) -> (u8, u8, u8) {
    let sdr = f.dynamic_range.as_deref().is_none_or(|d| d == "SDR");
    let codec = match video_codec(f.vcodec.as_deref().unwrap_or("")).as_deref() {
        Some("H.264") => 3,
        Some("AV1") => 2,
        Some("VP9") => 1,
        _ => 0,
    };
    let mp4 = f.ext.as_deref() == Some("mp4");
    (u8::from(sdr), codec, u8::from(mp4))
}

/// Reported size, or an estimate from bitrate (kbit/s) and duration.
fn size_of(f: &RawFormat, duration: Option<f64>) -> Option<u64> {
    let estimate = || Some(f.tbr.or(f.abr)? * 1000.0 / 8.0 * duration?);
    f.filesize.or(f.filesize_approx).or_else(estimate).filter(|s| s.is_finite() && *s > 0.0).map(|s| s as u64)
}

/// Video-only formats get the best audio, preferring one that fits the same
/// container so the file stays e.g. a plain MP4 instead of becoming MKV.
fn video_selector(f: &RawFormat) -> String {
    let id = &f.format_id;
    if f.acodec.as_deref().is_some_and(|a| a != "none") {
        return id.clone();
    }
    match f.ext.as_deref() {
        Some("mp4") => format!("{id}+ba[ext=m4a]/{id}+ba/{id}"),
        Some("webm") => format!("{id}+ba[ext=webm]/{id}+ba/{id}"),
        _ => format!("{id}+ba/{id}"),
    }
}

/// Friendly codec name; None when the site doesn't say.
fn video_codec(vcodec: &str) -> Option<String> {
    let family = vcodec.split('.').next().unwrap_or("").to_ascii_lowercase();
    Some(match family.as_str() {
        "avc1" | "avc3" | "h264" => "H.264".into(),
        "hev1" | "hvc1" | "h265" | "hevc" => "HEVC".into(),
        "vp09" | "vp9" => "VP9".into(),
        "vp8" => "VP8".into(),
        "av01" => "AV1".into(),
        "" => return None,
        other => other.to_uppercase(),
    })
}

fn audio_codec(acodec: &str, ext: &str) -> String {
    let family = acodec.split('.').next().unwrap_or("").to_ascii_lowercase();
    match family.as_str() {
        "opus" => "Opus".into(),
        "mp4a" | "aac" => "AAC".into(),
        "mp3" => "MP3".into(),
        "vorbis" => "Vorbis".into(),
        "flac" => "FLAC".into(),
        "ac-3" | "ac3" => "AC3".into(),
        "ec-3" | "eac3" => "E-AC3".into(),
        "dtse" | "dts" => "DTS".into(),
        "" => ext.to_uppercase(),
        other => other.to_uppercase(),
    }
}

/// Only sane `-f` selectors are accepted from the browser.
pub fn valid_selector(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "+/[]=<>!^$*~?:._-,()".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(id: &str, ext: &str, vcodec: &str, height: u32, fps: f64, tbr: f64) -> RawFormat {
        RawFormat {
            format_id: id.into(),
            ext: Some(ext.into()),
            vcodec: Some(vcodec.into()),
            acodec: Some("none".into()),
            height: Some(height),
            fps: Some(fps),
            dynamic_range: Some("SDR".into()),
            tbr: Some(tbr),
            ..Default::default()
        }
    }

    fn a(id: &str, ext: &str, acodec: &str, abr: f64) -> RawFormat {
        RawFormat {
            format_id: id.into(),
            ext: Some(ext.into()),
            vcodec: Some("none".into()),
            acodec: Some(acodec.into()),
            abr: Some(abr),
            ..Default::default()
        }
    }

    fn summary(choices: &[Choice]) -> Vec<String> {
        choices.iter().map(|c| format!("{} | {} | {}", c.quality, c.detail, c.format)).collect()
    }

    #[test]
    fn merges_duplicates_keeping_the_best() {
        let list = vec![
            a("249", "webm", "opus", 50.0),
            a("251", "webm", "opus", 130.0),
            a("140", "m4a", "mp4a.40.2", 129.0),
            a("251-drc", "webm", "opus", 135.0),
            a("233", "mp4", "", 0.0),
            v("sb0", "mhtml", "none", 90, 0.0, 0.0),
            v("134", "mp4", "avc1.4d401e", 360, 30.0, 300.0),
            RawFormat { protocol: Some("m3u8_native".into()), ..v("230", "mp4", "avc1.4d401e", 360, 30.0, 700.0) },
            v("605", "mp4", "avc1.4d401e", 360, 30.0, 500.0),
            v("299", "mp4", "avc1.64002a", 1080, 60.0, 4000.0),
            v("137", "mp4", "avc1.640028", 1080, 30.0, 2500.0),
        ];
        assert_eq!(
            summary(&choices(&list, None)),
            vec![
                "Best | Highest quality, chosen by yt-dlp | bv*+ba/b",
                "1080p60 | MP4 · H.264 | 299+ba[ext=m4a]/299+ba/299",
                "1080p | MP4 · H.264 | 137+ba[ext=m4a]/137+ba/137",
                "360p | MP4 · H.264 | 605+ba[ext=m4a]/605+ba/605",
                "Opus | WEBM · 130 kbps | 251",
                "AAC | M4A · 129 kbps | 140",
            ]
        );
    }

    #[test]
    fn hdr_is_its_own_choice() {
        let mut hdr = v("701", "mp4", "av01.0.13M.10", 2160, 60.0, 20000.0);
        hdr.dynamic_range = Some("HDR10".into());
        let sdr = v("401", "mp4", "av01.0.12M.08", 2160, 60.0, 15000.0);
        let got = choices(&[sdr, hdr], None);
        assert_eq!(got[1].detail, "MP4 · AV1 · HDR");
        assert_eq!(got[2].detail, "MP4 · AV1");
    }

    #[test]
    fn sizes_include_paired_audio() {
        let mut video = v("137", "mp4", "avc1", 1080, 30.0, 2500.0);
        video.filesize = Some(1_000_000.0);
        let mut m4a = a("140", "m4a", "mp4a.40.2", 128.0);
        m4a.filesize = Some(100_000.0);
        let opus = a("251", "webm", "opus", 130.0);
        let got = choices(&[video, m4a, opus], Some(10.0));
        assert_eq!(got[1].size, Some(1_100_000));
        // 130 kbit/s for 10 s, estimated from the bitrate.
        assert_eq!(got[2].quality, "Opus");
        assert_eq!(got[2].size, Some(162_500));
        assert_eq!(got[0].size, None);
    }

    #[test]
    fn primary_is_the_most_compatible_per_resolution() {
        let mut hdr = v("701", "mp4", "av01.0.13M.10", 2160, 60.0, 20000.0);
        hdr.dynamic_range = Some("HDR10".into());
        let list = vec![
            hdr,
            v("401", "mp4", "av01.0.12M.08", 2160, 60.0, 15000.0),
            v("315", "webm", "vp9", 2160, 60.0, 18000.0),
            v("248", "webm", "vp9", 1080, 30.0, 3000.0),
            v("137", "mp4", "avc1.640028", 1080, 30.0, 2500.0),
        ];
        let primary: Vec<String> =
            choices(&list, None).iter().filter(|c| c.primary).map(|c| format!("{} {}", c.quality, c.detail)).collect();
        assert_eq!(primary, vec!["Best Highest quality, chosen by yt-dlp", "2160p60 MP4 · AV1", "1080p MP4 · H.264"]);
    }

    #[test]
    fn unknown_codec_is_left_out() {
        let f = RawFormat { vcodec: None, acodec: None, ..v("hls-720p", "mp4", "", 720, 30.0, 2000.0) };
        let got = choices(&[f], None);
        assert_eq!((got[1].quality.as_str(), got[1].detail.as_str()), ("720p", "MP4"));
        assert_eq!(got[1].format, "hls-720p+ba[ext=m4a]/hls-720p+ba/hls-720p");
    }

    #[test]
    fn selectors() {
        assert!(valid_selector("137+ba[ext=m4a]/137+ba/137"));
        assert!(valid_selector(BEST));
        assert!(!valid_selector("-x"));
        assert!(!valid_selector("1 2"));
        assert!(!valid_selector(""));
    }
}
