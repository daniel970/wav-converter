use std::cmp::Ordering;
use std::path::{Path, PathBuf};

pub fn has_track_prefix(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let digits = stem.bytes().take_while(|c| c.is_ascii_digit()).count();
    digits > 0 && stem[digits..].starts_with(|c: char| c == '.' || c.is_whitespace())
}

pub fn numbered_wav_path(path: &Path, remove_artist: bool, track: Option<u32>) -> PathBuf {
    if !has_track_prefix(path) {
        if let Some(number) = track.filter(|n| *n > 0) {
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let numbered = path.with_file_name(format!("{number} {stem}.wav"));
            let cleaned = wav_path(&numbered, remove_artist);
            let stem = cleaned.file_stem().unwrap().to_string_lossy();
            let (_, title) = stem.split_once(' ').unwrap();
            return cleaned.with_file_name(format!("{number}. {title}.wav"));
        }
    }
    wav_path(path, remove_artist)
}

fn parse_track(value: &str) -> Option<u32> {
    value
        .split('/')
        .next()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
}

/// Read track metadata (# in Explorer), without decoding the audio.
pub fn read_track_number(path: &Path) -> anyhow::Result<Option<u32>> {
    use symphonia::core::{
        io::MediaSourceStream,
        meta::{MetadataRevision, StandardTagKey},
        probe::Hint,
    };
    fn from_revision(revision: &MetadataRevision) -> Option<u32> {
        revision
            .tags()
            .iter()
            .filter(|t| t.std_key == Some(StandardTagKey::TrackNumber))
            .find_map(|t| parse_track(&t.value.to_string()))
    }
    let file = std::fs::File::open(path)?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut probed = symphonia::default::get_probe().format(
        &hint,
        stream,
        &Default::default(),
        &Default::default(),
    )?;
    if let Some(metadata) = probed.metadata.get() {
        if let Some(number) = metadata.current().and_then(from_revision) {
            return Ok(Some(number));
        }
    }
    Ok(probed.format.metadata().current().and_then(from_revision))
}

pub fn filename_warning(paths: &[PathBuf]) -> Option<String> {
    let names: std::collections::BTreeSet<_> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_ascii())
        .collect();
    if names.is_empty() {
        return None;
    }
    let preview = names
        .iter()
        .take(10)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!("변환 결과 {}개에 영문(ASCII) 범위를 벗어난 문자가 포함되어 있습니다.\nDAP에서 파일이나 폴더를 인식하지 못할 수 있습니다.\n\n{}{}\n\n파일은 정상적으로 저장되었습니다. 파일명·하위 폴더명을 확인해 주세요.",
        names.len(), preview, if names.len() > 10 { format!("\n외 {}개 (전체 목록은 로그 참고)", names.len() - 10) } else { String::new() }))
}

/// Only strip a numbered `artist - title` pattern; preserve other names.
pub fn wav_path(path: &Path, remove_artist: bool) -> PathBuf {
    let normal = path.with_extension("wav");
    if !remove_artist {
        return normal;
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return normal;
    };
    let digits = stem.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return normal;
    }
    let rest = &stem[digits..];
    let rest = rest.strip_prefix('.').unwrap_or(rest);
    if !rest.starts_with(char::is_whitespace) {
        return normal;
    }
    let Some((artist, title)) = rest.trim_start().split_once(" - ") else {
        return normal;
    };
    if artist.trim().is_empty() || title.trim().is_empty() {
        return normal;
    }
    path.with_file_name(format!("{} {}.wav", &stem[..digits], title.trim()))
}

/// Compare digit runs numerically, including CD 2 before CD 10.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut x, mut y) = (a.as_str(), b.as_str());
    while !x.is_empty() && !y.is_empty() {
        let nx = x.bytes().take_while(|c| c.is_ascii_digit()).count();
        let ny = y.bytes().take_while(|c| c.is_ascii_digit()).count();
        let order = if nx > 0 && ny > 0 {
            let dx = x[..nx].trim_start_matches('0');
            let dy = y[..ny].trim_start_matches('0');
            let result = dx.len().cmp(&dy.len()).then_with(|| dx.cmp(dy));
            x = &x[nx..];
            y = &y[ny..];
            result
        } else {
            let cx = x.chars().next().unwrap();
            let cy = y.chars().next().unwrap();
            x = &x[cx.len_utf8()..];
            y = &y[cy.len_utf8()..];
            cx.cmp(&cy)
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    x.len().cmp(&y.len()).then_with(|| a.cmp(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn track_numbers_support_totals_and_reject_invalid_values() {
        assert_eq!(parse_track("03/28"), Some(3));
        assert_eq!(parse_track(" 12 "), Some(12));
        for value in ["0", "-1", "track 3", "", "99999999999999"] {
            assert_eq!(parse_track(value), None);
        }
    }
    #[test]
    fn prefixes_only_unnumbered_names_and_combines_artist_removal() {
        assert_eq!(
            numbered_wav_path(Path::new("Disc 1/Usagi Flap.mp3"), false, Some(1)),
            Path::new("Disc 1/1. Usagi Flap.wav")
        );
        assert_eq!(
            numbered_wav_path(Path::new("F1ghtback.mp3"), false, Some(23)),
            Path::new("23. F1ghtback.wav")
        );
        assert_eq!(
            numbered_wav_path(Path::new("Artist - Title.mp3"), true, Some(2)),
            Path::new("2. Title.wav")
        );
        assert_eq!(
            numbered_wav_path(Path::new("01. Title.mp3"), false, Some(9)),
            Path::new("01. Title.wav")
        );
        assert_eq!(
            numbered_wav_path(Path::new("Title.mp3"), false, None),
            Path::new("Title.wav")
        );
    }
    #[test]
    fn removes_artist_and_preserves_title_hyphens() {
        assert_eq!(
            wav_path(Path::new("CD 1/01 ミツキヨ - Song - Remix.mp3"), true),
            Path::new("CD 1/01 Song - Remix.wav")
        );
        assert_eq!(
            wav_path(Path::new("02. KARUT - Funky Road.flac"), true),
            Path::new("02 Funky Road.wav")
        );
    }
    #[test]
    fn leaves_disabled_or_unrecognized_names_unchanged() {
        for name in [
            "01 Song.mp3",
            "Artist - Song.mp3",
            "01 - Song.mp3",
            "01 Artist - .mp3",
        ] {
            assert_eq!(
                wav_path(Path::new(name), true),
                Path::new(name).with_extension("wav")
            );
        }
        assert_eq!(
            wav_path(Path::new("01 Artist - Song.mp3"), false),
            Path::new("01 Artist - Song.wav")
        );
    }
    #[test]
    fn sorts_disc_and_track_numbers() {
        let mut names = ["CD 10/1", "CD 2/10", "CD 2/2", "CD 1/14", "CD 1/01"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            ["CD 1/01", "CD 1/14", "CD 2/2", "CD 2/10", "CD 10/1"]
        );
    }
}
