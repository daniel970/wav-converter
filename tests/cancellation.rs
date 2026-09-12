//! 실제 변환 중 취소가 디코딩/리샘플 경로를 중단하고 임시 출력을 정리하는지 확인한다.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wav_converter::convert::{convert_file_cancellable, is_cancelled, OutputFormat};

/// 긴 무음 소스를 빠르게 만든다. 실제 PCM WAV 헤더와 5분의 0 샘플 데이터다.
fn make_long_source(path: &Path) {
    let sample_rate = 48_000_u32;
    let data_len = sample_rate * 4 * 300;
    let mut file = fs::File::create(path).unwrap();
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(data_len + 36).to_le_bytes()).unwrap();
    file.write_all(b"WAVEfmt ").unwrap();
    file.write_all(&16_u32.to_le_bytes()).unwrap();
    file.write_all(&1_u16.to_le_bytes()).unwrap();
    file.write_all(&2_u16.to_le_bytes()).unwrap();
    file.write_all(&sample_rate.to_le_bytes()).unwrap();
    file.write_all(&(sample_rate * 4).to_le_bytes()).unwrap();
    file.write_all(&4_u16.to_le_bytes()).unwrap();
    file.write_all(&16_u16.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&data_len.to_le_bytes()).unwrap();
    file.set_len(u64::from(data_len) + 44).unwrap();
}

#[test]
fn already_cancelled_does_not_create_an_output_directory() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("new-album/result.wav");
    let error = convert_file_cancellable(
        &dir.path().join("not-opened.wav"),
        &output,
        OutputFormat::Preserve,
        &AtomicBool::new(true),
    )
    .unwrap_err();

    assert!(is_cancelled(&error));
    assert!(!output.parent().unwrap().exists());
}

fn cancel_after_output_has_started(format: OutputFormat) {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("source.wav");
    make_long_source(&input);
    let output_dir = dir.path().join("output");
    fs::create_dir(&output_dir).unwrap();
    let output = output_dir.join("result.wav");
    let previous_output = b"existing output must survive cancellation";
    fs::write(&output, previous_output).unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let worker_input = input.clone();
    let worker_output = output.clone();
    let worker = std::thread::spawn(move || {
        convert_file_cancellable(&worker_input, &worker_output, format, &worker_cancel)
    });

    // 파일 생성만이 아니라 PCM 데이터가 기록된 다음에 취소하여 곡 중간 취소를 검증한다.
    let wait_started = Instant::now();
    let mut wrote_samples = false;
    while wait_started.elapsed() < Duration::from_secs(10) {
        wrote_samples = fs::read_dir(&output_dir).unwrap().any(|entry| {
            let entry = entry.unwrap();
            entry.path() != output
                && fs::File::open(entry.path())
                    .and_then(|file| file.metadata())
                    .is_ok_and(|metadata| metadata.len() > 4096)
        });
        if wrote_samples || worker.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let requested = Instant::now();
    cancel.store(true, Ordering::Relaxed);
    let result = worker.join().unwrap();

    assert!(
        wrote_samples,
        "변환 결과가 실제 기록된 뒤 취소해야 한다: {result:?}"
    );
    let error = result.unwrap_err();
    assert!(
        is_cancelled(&error),
        "취소를 실패와 구분해야 한다: {error:#}"
    );
    assert!(
        requested.elapsed() < Duration::from_secs(5),
        "곡 끝까지 처리하지 않고 중단해야 한다"
    );
    assert_eq!(fs::read(&output).unwrap(), previous_output);
    assert_eq!(
        fs::read_dir(&output_dir).unwrap().count(),
        1,
        "임시 WAV가 남으면 안 된다"
    );
    assert_eq!(
        hound::WavReader::open(input).unwrap().duration(),
        48_000 * 300
    );
}

#[test]
fn cancel_during_direct_conversion_removes_partial_and_preserves_existing_output() {
    cancel_after_output_has_started(OutputFormat::Preserve);
}

#[test]
fn cancel_during_resampling_removes_partial_and_preserves_existing_output() {
    cancel_after_output_has_started(OutputFormat::Pcm24_96000);
}
