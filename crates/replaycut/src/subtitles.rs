//! Subtitles on a cut (R15, since 3.11), the parts that need no service:
//! the models the transcription runs on, the SRT the `whisper` filter writes
//! and the SRT/VTT we hand out again.
//!
//! Everything here is local. The only thing that ever leaves this PC is the
//! one request that fetches a model, and only when somebody asks for it.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::db::Segment;

/// A model the UI offers. `sha256` is the file's own hash, which Hugging
/// Face publishes as the LFS object id of that file; it is checked once
/// after a download and once when a file that was put there by hand shows
/// up for the first time.
pub struct Model {
    /// What the API and the settings call it.
    pub name: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// The three sizes the UI offers, smallest first. Anything else - the
/// quantised builds, `large` - can be put into the models folder by hand;
/// `docs/settings.md` says so.
pub const MODELS: [Model; 3] = [
    Model {
        name: "base",
        file: "ggml-base.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
        bytes: 147_951_465,
        sha256: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
    },
    Model {
        name: "small",
        file: "ggml-small.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
        bytes: 487_601_967,
        sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    },
    Model {
        name: "medium",
        file: "ggml-medium.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
        bytes: 1_533_763_059,
        sha256: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
    },
];

/// Silero, the voice activity detector. It costs under a megabyte and keeps
/// whisper from inventing sentences in the silence between two callouts,
/// which in a game clip is most of the running time. It comes down with the
/// first model and is used whenever it is there.
pub const VAD: Model = Model {
    name: "vad",
    file: "ggml-silero-v5.1.2.bin",
    url: "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin",
    bytes: 885_098,
    sha256: "29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf",
};

pub fn model(name: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.name == name)
}

/// Where the models live: `<data-dir>/models`. The data directory survives
/// an update, so nothing is ever fetched twice.
pub fn models_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("models")
}

/// A model that is not there yet, is coming down, or can be used.
pub const ABSENT: &str = "absent";
pub const DOWNLOADING: &str = "downloading";
pub const READY: &str = "ready";

/// A download in flight. The UI polls `GET /api/subtitles/models`, so this
/// is all the progress there is; a model is not a job and takes no place in
/// the queue.
#[derive(Debug, Clone, Default)]
pub struct Download {
    pub percent: u8,
    pub error: Option<String>,
}

/// The file of a model in the models folder, whether it is there or not.
pub fn file_of(data_dir: &Path, m: &Model) -> PathBuf {
    models_dir(data_dir).join(m.file)
}

/// A model that may be used: the file is there and it is the file we meant.
///
/// The hash is worth a third of a second on 148 MB, so it is checked once -
/// after a download, and the first time a file somebody dropped into the
/// folder by hand is seen. `<file>.ok` remembers that it was.
pub fn ready(data_dir: &Path, m: &Model) -> Option<PathBuf> {
    let path = file_of(data_dir, m);
    if std::fs::metadata(&path).ok()?.len() != m.bytes {
        return None;
    }
    let marker = path.with_extension("ok");
    if marker.is_file() {
        return Some(path);
    }
    match crate::update::sha256_hex(&path) {
        Ok(h) if h == m.sha256 => {
            let _ = std::fs::write(&marker, m.sha256);
            Some(path)
        }
        Ok(h) => {
            tracing::warn!(
                "the model {} in {} is not the file it should be ({h}); \
                 delete it and download it again",
                m.name,
                path.display()
            );
            None
        }
        Err(e) => {
            tracing::warn!("cannot read the model {}: {e:#}", m.name);
            None
        }
    }
}

/// Fetch a model, checksum and all. The `.part` file keeps a half download
/// out of the way; there is no resuming, a broken one is simply fetched
/// again. `progress` is called with whole percent.
pub async fn fetch(data_dir: &Path, m: &Model, mut progress: impl FnMut(u8)) -> Result<PathBuf> {
    use futures_util::StreamExt;

    let dir = models_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(m.file);
    let part = path.with_extension("part");
    let res = crate::update::client(std::time::Duration::from_secs(60 * 60))?
        .get(m.url)
        .send()
        .await?
        .error_for_status()?;
    let total = res.content_length().unwrap_or(m.bytes).max(1);
    let mut file = tokio::fs::File::create(&part).await?;
    let mut stream = res.bytes_stream();
    let (mut done, mut last) = (0u64, 0u8);
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&part);
                return Err(e.into());
            }
        };
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        done += chunk.len() as u64;
        let pct = ((done * 100) / total).min(99) as u8;
        if pct != last {
            last = pct;
            progress(pct);
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    drop(file);

    let hash = crate::update::sha256_hex(&part)?;
    if hash != m.sha256 {
        let _ = std::fs::remove_file(&part);
        bail!(
            "the download of {} is not the file it should be - got {hash}, expected {}",
            m.name,
            m.sha256
        );
    }
    std::fs::rename(&part, &path)?;
    let _ = std::fs::write(path.with_extension("ok"), m.sha256);
    Ok(path)
}

/// The languages the UI offers besides `auto`. whisper knows many more; a
/// model file put there by hand takes any ISO 639-1 code the API is given,
/// this list is only what the menu shows.
pub const LANGUAGES: [(&str, &str); 10] = [
    ("auto", "Detect"),
    ("de", "German"),
    ("en", "English"),
    ("fr", "French"),
    ("es", "Spanish"),
    ("it", "Italian"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("tr", "Turkish"),
];

/// `auto` or two letters; anything else is a typo rather than a language.
pub fn known_language(code: &str) -> bool {
    code == "auto" || (code.len() == 2 && code.bytes().all(|b| b.is_ascii_lowercase()))
}

/// The most a `PUT` accepts, so a broken client cannot fill the store.
pub const MAX_SEGMENTS: usize = 2000;
pub const MAX_TEXT: usize = 500;

/// Read the SRT the `whisper` filter wrote. `offset` is added to every
/// time, which turns the cut file's own time base into the recording's.
///
/// The filter's SRT is not quite the norm and we take it as it comes: it
/// numbers from 0, and segments from neighbouring queue windows overlap by
/// up to a fifth of a second. [`tidy`] sorts that out afterwards.
pub fn parse_srt(text: &str, offset: f64) -> Vec<Segment> {
    let mut out = Vec::new();
    for block in text.replace("\r\n", "\n").split("\n\n") {
        let mut lines = block.trim_matches('\n').lines();
        let Some(first) = lines.next() else { continue };
        // the counter is optional: with it the times are on the second line
        let times = if first.contains("-->") {
            first
        } else {
            match lines.next() {
                Some(l) if l.contains("-->") => l,
                _ => continue,
            }
        };
        let Some((from, to)) = times.split_once("-->") else {
            continue;
        };
        let (Some(start), Some(end)) = (parse_time(from), parse_time(to)) else {
            continue;
        };
        let text = lines.collect::<Vec<_>>().join("\n").trim().to_string();
        if text.is_empty() {
            continue;
        }
        out.push(Segment {
            start: round_ms(start + offset),
            end: round_ms(end + offset),
            text,
        });
    }
    out
}

/// `HH:MM:SS,mmm` or `HH:MM:SS.mmm`, hours optional.
fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    let mut seconds = 0.0;
    for part in s.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(seconds)
}

fn round_ms(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Nothing shorter than this is a subtitle: it would flash and be gone.
/// Words that end up in such a sliver belong to the line before them.
const MIN_SEGMENT: f64 = 0.15;

/// Make a transcript out of what the filter produced: in order, without
/// overlaps, and with nothing too short to read.
///
/// The end of a segment is cut back to the start of the next one. Where
/// that leaves less than [`MIN_SEGMENT`], the words are not thrown away -
/// they are appended to the line before, which is where they were spoken.
/// The filter does produce this: a segment cut back to ten milliseconds
/// carried the word "back." that belonged to the sentence before it.
pub fn tidy(mut segments: Vec<Segment>) -> Vec<Segment> {
    segments.retain(|s| !s.text.trim().is_empty());
    segments.sort_by(|a, b| a.start.total_cmp(&b.start));
    // in order and side by side: nobody ends after the next one begins
    for i in 0..segments.len() {
        if let Some(next) = segments.get(i + 1).map(|s| s.start) {
            if segments[i].end > next {
                segments[i].end = next;
            }
        }
    }
    // then fold the slivers into the line they were spoken with: the one
    // before, or - when a sliver comes first - the one after it
    let mut out: Vec<Segment> = Vec::with_capacity(segments.len());
    let mut carried = String::new();
    for s in segments {
        let text = if carried.is_empty() {
            s.text.trim().to_string()
        } else {
            format!("{carried} {}", s.text.trim())
        };
        carried.clear();
        if s.end - s.start >= MIN_SEGMENT {
            out.push(Segment { text, ..s });
        } else if let Some(last) = out.last_mut() {
            last.text = format!("{} {text}", last.text);
            // the sliver's end was already cut back to the next start, so
            // taking it over cannot reach into the line that follows
            last.end = last.end.max(s.end);
        } else {
            carried = text;
        }
    }
    out.retain(|s| s.end > s.start && !s.text.is_empty());
    out
}

/// What `PUT /api/cuts/<id>/subtitles` accepts. The range is the cut's, so
/// a subtitle cannot point outside the picture it belongs to.
pub fn check(segments: &[Segment], start: f64, end: f64) -> Result<()> {
    if segments.len() > MAX_SEGMENTS {
        bail!(
            "too many segments: {} (at most {MAX_SEGMENTS})",
            segments.len()
        );
    }
    // a frame or two of slack, the same tolerance a cut is matched with
    let (low, high) = (start - 0.05, end + 0.05);
    let mut previous: Option<f64> = None;
    for (i, s) in segments.iter().enumerate() {
        if s.text.trim().is_empty() {
            bail!("segment {i} has no text");
        }
        if s.text.chars().count() > MAX_TEXT {
            bail!("segment {i} is longer than {MAX_TEXT} characters");
        }
        if s.end <= s.start {
            bail!("segment {i} ends before it starts");
        }
        if s.start < low || s.end > high {
            bail!(
                "segment {i} ({:.2}-{:.2} s) is outside the cut ({start:.2}-{end:.2} s)",
                s.start,
                s.end
            );
        }
        if let Some(p) = previous {
            if s.start < p {
                bail!("segment {i} overlaps the one before it");
            }
        }
        previous = Some(s.end);
    }
    Ok(())
}

/// SRT as the world expects it: numbered from 1, `HH:MM:SS,mmm`. `offset`
/// is taken off every time, which is how the recording's time base becomes
/// the rendering's.
pub fn to_srt(segments: &[Segment], offset: f64) -> String {
    let mut out = String::new();
    for (i, s) in segments.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            stamp(s.start - offset, ','),
            stamp(s.end - offset, ','),
            s.text.trim()
        ));
    }
    out
}

pub fn to_vtt(segments: &[Segment], offset: f64) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for s in segments {
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            stamp(s.start - offset, '.'),
            stamp(s.end - offset, '.'),
            s.text.trim()
        ));
    }
    out
}

fn stamp(seconds: f64, decimal: char) -> String {
    let t = seconds.max(0.0);
    let ms = (t * 1000.0).round() as u64;
    let (h, m, s, rest) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    format!("{h:02}:{m:02}:{s:02}{decimal}{rest:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: f64, end: f64, text: &str) -> Segment {
        Segment {
            start,
            end,
            text: text.into(),
        }
    }

    /// Exactly what the filter wrote on the box on 2026-09-20: numbered
    /// from 0, and segment 2 ends 200 ms after segment 3 begins.
    const AS_THE_FILTER_WRITES_IT: &str = "0\n\
        00:00:00,000 --> 00:00:02,240\nHe is coming from the left side.\n\n\
        1\n00:00:02,240 --> 00:00:06,590\nTake the smoke, I will go around\n\n\
        2\n00:00:06,590 --> 00:00:07,240\nthe back.\n\n\
        3\n00:00:07,040 --> 00:00:09,040\nNice shot.\n";

    #[test]
    fn the_srt_of_the_filter_is_read_with_its_counter_starting_at_zero() {
        let segments = parse_srt(AS_THE_FILTER_WRITES_IT, 0.0);
        assert_eq!(segments.len(), 4);
        assert_eq!(
            segments[0],
            seg(0.0, 2.24, "He is coming from the left side.")
        );
        assert_eq!(segments[3], seg(7.04, 9.04, "Nice shot."));
    }

    #[test]
    fn the_times_move_with_the_cut_into_the_recording() {
        let segments = parse_srt(AS_THE_FILTER_WRITES_IT, 120.5);
        assert_eq!(segments[0].start, 120.5);
        assert_eq!(segments[0].end, 122.74);
    }

    #[test]
    fn overlapping_segments_are_cut_back_to_the_next_one() {
        let segments = tidy(parse_srt(AS_THE_FILTER_WRITES_IT, 0.0));
        assert_eq!(segments.len(), 4);
        // 6.59 -> 7.24 ended after 7.04 began
        assert_eq!(segments[2], seg(6.59, 7.04, "the back."));
        for pair in segments.windows(2) {
            assert!(pair[0].end <= pair[1].start, "{pair:?} still overlap");
        }
        assert!(check(&segments, 0.0, 20.0).is_ok());
    }

    /// Seen on the box: a segment cut back to ten milliseconds carried the
    /// word "back." Those words belong to the line before them, not to the
    /// bin and not to a subtitle nobody can read.
    #[test]
    fn a_sliver_gives_its_words_to_the_line_before_it() {
        let segments = tidy(vec![
            seg(2.256, 7.016, "Take the smoke, I will go around the"),
            seg(7.016, 7.24, "back."),
            seg(7.026, 9.026, "Nice shot."),
        ]);
        assert_eq!(segments.len(), 2);
        assert_eq!(
            segments[0].text,
            "Take the smoke, I will go around the back."
        );
        assert_eq!(segments[1], seg(7.026, 9.026, "Nice shot."));
        assert!(segments
            .iter()
            .all(|s| s.end - s.start >= MIN_SEGMENT || s.end - s.start > 0.0));
    }

    /// A segment the next one swallows whole keeps no room of its own, and
    /// a segment without text is not one. Neither loses a word: what was
    /// said moves into the line that has the room.
    #[test]
    fn a_segment_with_no_room_left_hands_its_words_on() {
        let segments = tidy(vec![
            seg(1.0, 5.0, "swallowed"),
            seg(1.0, 2.0, "the line with the room"),
            seg(2.0, 3.0, "   "),
        ]);
        assert_eq!(segments.len(), 1);
        assert_eq!(
            segments[0],
            seg(1.0, 2.0, "swallowed the line with the room")
        );
    }

    #[test]
    fn hours_and_line_breaks_survive_the_round_trip() {
        let segments = vec![
            seg(3661.5, 3663.25, "one hour in"),
            seg(3664.0, 3666.0, "two\nlines"),
        ];
        let srt = to_srt(&segments, 0.0);
        assert!(srt.starts_with("1\n01:01:01,500 --> 01:01:03,250\none hour in"));
        assert_eq!(parse_srt(&srt, 0.0), segments);
    }

    #[test]
    fn the_export_takes_the_offset_off_again() {
        let segments = vec![seg(120.5, 122.74, "hello")];
        assert!(to_srt(&segments, 120.0).contains("00:00:00,500 --> 00:00:02,740"));
        let vtt = to_vtt(&segments, 120.0);
        assert!(vtt.starts_with("WEBVTT\n\n00:00:00.500 --> 00:00:02.740\nhello"));
    }

    #[test]
    fn a_put_that_would_break_the_transcript_is_refused() {
        let cut = (6.0, 12.0);
        assert!(check(&[seg(6.0, 8.0, "fine")], cut.0, cut.1).is_ok());
        assert!(check(&[seg(8.0, 8.0, "no length")], cut.0, cut.1).is_err());
        assert!(check(&[seg(6.0, 8.0, "  ")], cut.0, cut.1).is_err());
        assert!(check(&[seg(2.0, 4.0, "before the cut")], cut.0, cut.1).is_err());
        assert!(check(&[seg(10.0, 20.0, "past the cut")], cut.0, cut.1).is_err());
        assert!(check(
            &[seg(6.0, 9.0, "first"), seg(8.0, 10.0, "overlaps")],
            cut.0,
            cut.1
        )
        .is_err());
        let many: Vec<Segment> = (0..MAX_SEGMENTS + 1).map(|_| seg(6.0, 7.0, "x")).collect();
        assert!(check(&many, cut.0, cut.1).is_err());
    }

    #[test]
    fn the_model_catalogue_is_what_the_spec_pinned() {
        assert_eq!(model("base").unwrap().bytes, 147_951_465);
        assert!(model("large").is_none());
        for m in MODELS.iter().chain(std::iter::once(&VAD)) {
            assert_eq!(m.sha256.len(), 64, "{} has no hash", m.name);
            assert!(m.url.ends_with(m.file), "{} url and file disagree", m.name);
        }
    }

    #[test]
    fn a_language_is_auto_or_two_letters() {
        assert!(known_language("auto"));
        assert!(known_language("de"));
        assert!(!known_language("German"));
        assert!(!known_language("DE"));
        assert!(!known_language(""));
    }
}
