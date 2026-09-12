use wav_converter::convert::{convert_in_place_to, OutputFormat};
use wav_converter::naming::wav_path;

#[test]
fn renamed_in_place_conversion_overwrites_existing_destination() {
    let root = std::env::temp_dir().join(format!("wav-naming-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("01 Artist - Song.wav");
    let target = wav_path(&source, true);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 44100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&source, spec).unwrap();
    for _ in 0..1000 {
        writer.write_sample(100i16).unwrap();
    }
    writer.finalize().unwrap();
    std::fs::write(&target, b"existing output").unwrap();
    convert_in_place_to(&source, &target, OutputFormat::Preserve).unwrap();
    assert!(!source.exists());
    assert_eq!(hound::WavReader::open(&target).unwrap().duration(), 1000);
    std::fs::remove_file(&target).unwrap();
    std::fs::remove_dir(&root).unwrap();
}

#[test]
fn failed_conversion_preserves_existing_output() {
    let root = std::env::temp_dir().join(format!("wav-failed-overwrite-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("bad.mp3");
    let target = root.join("out.wav");
    std::fs::write(&source, b"invalid audio").unwrap();
    std::fs::write(&target, b"previous output").unwrap();
    assert!(
        wav_converter::convert::convert_file(&source, &target, OutputFormat::Preserve).is_err()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"previous output");
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
    std::fs::remove_file(source).unwrap();
    std::fs::remove_file(target).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn non_ascii_warning_checks_result_paths() {
    use wav_converter::naming::filename_warning;
    assert!(filename_warning(&["Disc 1/01 Song - Remix!.wav".into()]).is_none());
    for path in [
        "Disc 1/01 노래.wav",
        "日本語/01 Song.wav",
        "Disc 2/01 中文.wav",
    ] {
        assert!(filename_warning(&[path.into()]).unwrap().contains(path));
    }
    let cleaned = wav_path(std::path::Path::new("01 ミツキヨ - Song.mp3"), true);
    assert!(filename_warning(&[cleaned]).is_none());
}
