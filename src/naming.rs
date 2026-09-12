use std::cmp::Ordering;
use std::path::{Path, PathBuf};

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
