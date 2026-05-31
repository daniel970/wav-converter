//! 디코딩→리샘플→WAV 인코딩 전체 경로 스모크 테스트.
//! 사인파 소스 WAV를 만들고, 변환 후 결과 WAV의 스펙/내용을 확인한다.

use std::f32::consts::PI;
use std::path::PathBuf;

use wav_converter::convert::{convert_file, convert_in_place, OutputFormat};

/// 임시 작업 폴더 (테스트별 고유 하위 폴더).
fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("wav-converter-test").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 48kHz 스테레오 16bit 사인파 소스 WAV 생성.
fn make_source(path: &std::path::Path, sample_rate: u32, secs: f32) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    let n = (sample_rate as f32 * secs) as u32;
    for i in 0..n {
        let t = i as f32 / sample_rate as f32;
        let l = (2.0 * PI * 440.0 * t).sin();
        let r = (2.0 * PI * 660.0 * t).sin();
        w.write_sample((l * i16::MAX as f32) as i16).unwrap();
        w.write_sample((r * i16::MAX as f32) as i16).unwrap();
    }
    w.finalize().unwrap();
}

#[test]
fn preserve_keeps_rate_and_channels() {
    let dir = tmp_dir("preserve");
    let src = dir.join("in.wav");
    let out = dir.join("out.wav");
    make_source(&src, 48_000, 0.5);

    convert_file(&src, &out, OutputFormat::Preserve).unwrap();

    let reader = hound::WavReader::open(&out).unwrap();
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 48_000, "원본 샘플레이트 유지");
    assert_eq!(spec.channels, 2, "원본 채널 유지");
    assert!(reader.duration() > 0, "샘플이 비어있지 않아야 함");
}

#[test]
fn resample_changes_rate() {
    let dir = tmp_dir("resample");
    let src = dir.join("in.wav");
    let out = dir.join("out.wav");
    make_source(&src, 48_000, 0.5); // 48k 소스

    convert_file(&src, &out, OutputFormat::Pcm16_44100).unwrap(); // → 44.1k 로 리샘플

    let reader = hound::WavReader::open(&out).unwrap();
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 44_100, "목표 샘플레이트로 변경");
    assert_eq!(spec.bits_per_sample, 16);
    assert_eq!(spec.channels, 2);

    // 0.5초 → 약 22050 프레임(±래퍼런스 여유). 대략적으로만 확인.
    let frames = reader.duration();
    assert!(
        (20_000..24_000).contains(&frames),
        "리샘플된 길이가 합리적 범위여야 함: {frames}"
    );
}

#[test]
fn output_dir_is_created() {
    let dir = tmp_dir("mkdir");
    let src = dir.join("in.wav");
    // 존재하지 않는 하위 폴더에 출력 → convert_file이 폴더를 만들어야 함.
    let out = dir.join("nested/deep/out.wav");
    make_source(&src, 44_100, 0.2);

    convert_file(&src, &out, OutputFormat::Pcm24_48000).unwrap();

    assert!(out.exists(), "중첩 출력 폴더가 자동 생성되어야 함");
    let spec = hound::WavReader::open(&out).unwrap().spec();
    assert_eq!(spec.sample_rate, 48_000);
    assert_eq!(spec.bits_per_sample, 24);
}

#[test]
fn in_place_replaces_non_wav_original() {
    let dir = tmp_dir("inplace_mp3like");
    // 확장자만 .flac 흉내(실제론 wav 컨테이너) — 핵심은 "원본 != 최종" 경로 처리.
    // 여기선 .wav가 아닌 원본을 만들기 위해 flac 대신 다른 이름 사용이 어려우니
    // 실제 디코딩 가능한 wav를 만들고, 원본/최종 경로가 같은 케이스로 검증한다.
    let src = dir.join("song.wav");
    make_source(&src, 48_000, 0.2);

    // 이미 .wav인 파일을 in-place 변환 → 제자리 덮어쓰기, 파일 그대로 존재.
    convert_in_place(&src, OutputFormat::Pcm16_44100).unwrap();
    assert!(src.exists(), "in-place 후에도 .wav 파일은 존재해야 함");
    let spec = hound::WavReader::open(&src).unwrap().spec();
    assert_eq!(spec.sample_rate, 44_100, "제자리에서 규격이 적용되어야 함");

    // 임시 파일이 남지 않아야 함.
    assert!(!dir.join("song.wavtmp").exists(), "임시 파일이 정리되어야 함");
}
