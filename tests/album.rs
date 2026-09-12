//! Opt-in reproduction using a real album; sources are never modified.
use wav_converter::convert::{convert_file, is_audio_file, OutputFormat};

#[test]
#[ignore = "requires WAV_TEST_ALBUM and writes conversions to a temporary directory"]
fn repeat_album_conversion() {
    let input = std::path::PathBuf::from(std::env::var_os("WAV_TEST_ALBUM").expect("album path"));
    let output = std::env::temp_dir().join(format!("wav-album-check-{}", std::process::id()));
    let files: Vec<_> = walkdir::WalkDir::new(&input)
        .into_iter()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().is_file() && is_audio_file(e.path()))
        .map(|e| e.into_path())
        .collect();
    assert!(!files.is_empty());
    for format in OutputFormat::ALL {
        for file in &files {
            println!("Checking {format:?}: {}", file.display());
            let dest = output
                .join(file.strip_prefix(&input).unwrap())
                .with_extension("wav");
            convert_file(file, &dest, format).unwrap();
            let reader = hound::WavReader::open(&dest).unwrap();
            assert!(reader.duration() > 0);
            drop(reader);
            std::fs::remove_file(dest).unwrap();
        }
    }
}
