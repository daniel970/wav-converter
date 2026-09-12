use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use wav_converter::convert::{is_cancelled, OutputFormat};
use wav_converter::output_order::arrange_output;
use wav_converter::transaction::OutputTransaction;

fn source(path: &Path, rate: u32) {
    let mut writer = hound::WavWriter::create(
        path,
        hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .unwrap();
    for sample in 0..1024_i16 {
        writer.write_sample(sample).unwrap();
    }
    writer.finalize().unwrap();
}

#[test]
fn cancelling_whole_job_restores_overwrites_and_removes_completed_new_outputs() {
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("input.wav");
    source(&input, 44_100);
    let input_bytes = fs::read(&input).unwrap();
    let output = fixture.path().join("output");
    fs::create_dir(&output).unwrap();
    let overwritten = output.join("01 existing.wav");
    let untouched = output.join("keep.txt");
    fs::write(&overwritten, b"original destination bytes").unwrap();
    fs::write(&untouched, b"unrelated").unwrap();
    let new = output.join("new album").join("02 new.wav");
    let cancel = AtomicBool::new(false);
    let mut transaction = OutputTransaction::new(&output).unwrap();
    transaction
        .convert(&input, &overwritten, OutputFormat::Preserve, &cancel)
        .unwrap();
    transaction
        .convert(&input, &new, OutputFormat::Preserve, &cancel)
        .unwrap();
    assert_ne!(
        fs::read(&overwritten).unwrap(),
        b"original destination bytes"
    );
    assert!(new.exists());
    cancel.store(true, Ordering::Release);
    let error = transaction
        .convert(
            &input,
            &output.join("03 cancelled.wav"),
            OutputFormat::Preserve,
            &cancel,
        )
        .unwrap_err();
    assert!(is_cancelled(&error));
    transaction.rollback().unwrap();
    assert_eq!(
        fs::read(&overwritten).unwrap(),
        b"original destination bytes"
    );
    assert_eq!(fs::read(&untouched).unwrap(), b"unrelated");
    assert_eq!(fs::read(&input).unwrap(), input_bytes);
    assert!(!new.parent().unwrap().exists());
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 2);
}

#[test]
fn failed_conversion_does_not_replace_existing_destination_or_leave_output_folder() {
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("broken.wav");
    fs::write(&input, b"not audio").unwrap();
    let output = fixture.path().join("output");
    let mut transaction = OutputTransaction::new(&output).unwrap();
    assert!(transaction
        .convert(
            &input,
            &output.join("new.wav"),
            OutputFormat::Preserve,
            &AtomicBool::new(false)
        )
        .is_err());
    assert!(!output.exists());
    fs::create_dir(&output).unwrap();
    let destination = output.join("existing.wav");
    fs::write(&destination, b"keep original").unwrap();
    assert!(transaction
        .convert(
            &input,
            &destination,
            OutputFormat::Preserve,
            &AtomicBool::new(false)
        )
        .is_err());
    transaction.commit().unwrap();
    assert_eq!(fs::read(destination).unwrap(), b"keep original");
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 2);
}

#[test]
fn cancellation_after_directory_ordering_still_restores_every_original() {
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("source.wav");
    source(&input, 44_100);
    let output = fixture.path().join("output");
    fs::create_dir_all(output.join("Disc 2")).unwrap();
    let original = output.join("Disc 2").join("02 original.wav");
    fs::write(&original, b"original").unwrap();
    let new = output.join("Disc 1").join("01 new.wav");
    let mut transaction = OutputTransaction::new(&output).unwrap();
    let cancel = AtomicBool::new(false);
    transaction
        .convert(&input, &original, OutputFormat::Preserve, &cancel)
        .unwrap();
    transaction
        .convert(&input, &new, OutputFormat::Preserve, &cancel)
        .unwrap();
    arrange_output(&output, &[original.clone(), new.clone()]).unwrap();
    transaction.rollback().unwrap();
    assert_eq!(fs::read(original).unwrap(), b"original");
    assert!(!new.parent().unwrap().exists());
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 2);
}

#[test]
fn in_place_sources_survive_cancellation_and_are_removed_only_on_commit() {
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("01 Artist - Title.wav");
    let output = fixture.path().join("01 Title.wav");
    source(&input, 48_000);
    let original = fs::read(&input).unwrap();
    {
        let mut transaction = OutputTransaction::new(fixture.path()).unwrap();
        transaction
            .convert(
                &input,
                &output,
                OutputFormat::Pcm16_44100,
                &AtomicBool::new(false),
            )
            .unwrap();
        transaction.defer_source_removal(&input);
        assert_eq!(fs::read(&input).unwrap(), original);
        transaction.rollback().unwrap();
    }
    assert_eq!(fs::read(&input).unwrap(), original);
    assert!(!output.exists());
    let mut transaction = OutputTransaction::new(fixture.path()).unwrap();
    transaction
        .convert(
            &input,
            &output,
            OutputFormat::Pcm16_44100,
            &AtomicBool::new(false),
        )
        .unwrap();
    transaction.defer_source_removal(&input);
    transaction.commit().unwrap();
    assert!(!input.exists());
    assert_eq!(
        hound::WavReader::open(output).unwrap().spec().sample_rate,
        44_100
    );
}

#[test]
fn input_that_was_overwritten_by_an_earlier_track_decodes_its_original_bytes() {
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("input.wav");
    let later_input = fixture.path().join("later.wav");
    let later_output = fixture.path().join("last.wav");
    source(&input, 44_100);
    source(&later_input, 48_000);
    let original_later = fs::read(&later_input).unwrap();
    let mut transaction = OutputTransaction::new(fixture.path()).unwrap();
    let cancel = AtomicBool::new(false);
    transaction
        .convert(&input, &later_input, OutputFormat::Preserve, &cancel)
        .unwrap();
    transaction.defer_source_removal(&input);
    transaction
        .convert(&later_input, &later_output, OutputFormat::Preserve, &cancel)
        .unwrap();
    transaction.defer_source_removal(&later_input);
    assert_eq!(
        hound::WavReader::open(&later_output)
            .unwrap()
            .spec()
            .sample_rate,
        48_000
    );
    transaction.rollback().unwrap();
    assert_eq!(fs::read(&later_input).unwrap(), original_later);
    assert!(input.exists());
    assert!(!later_output.exists());
}

#[test]
fn repeated_destination_restores_pre_job_bytes_and_same_path_commit_keeps_result() {
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("input.wav");
    source(&input, 48_000);
    let original = fs::read(&input).unwrap();
    {
        let mut transaction = OutputTransaction::new(fixture.path()).unwrap();
        let cancel = AtomicBool::new(false);
        transaction
            .convert(&input, &input, OutputFormat::Pcm16_44100, &cancel)
            .unwrap();
        transaction
            .convert(&input, &input, OutputFormat::Preserve, &cancel)
            .unwrap();
        assert_eq!(
            hound::WavReader::open(&input).unwrap().spec().sample_rate,
            48_000
        );
        transaction.rollback().unwrap();
    }
    assert_eq!(fs::read(&input).unwrap(), original);
    let mut transaction = OutputTransaction::new(fixture.path()).unwrap();
    transaction
        .convert(
            &input,
            &input,
            OutputFormat::Pcm16_44100,
            &AtomicBool::new(false),
        )
        .unwrap();
    transaction.defer_source_removal(&input);
    transaction.commit().unwrap();
    assert_eq!(
        hound::WavReader::open(&input).unwrap().spec().sample_rate,
        44_100
    );
}

#[test]
fn drop_rolls_back_and_keeps_unrelated_files_added_to_new_output_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("input.wav");
    source(&input, 44_100);
    let output = fixture.path().join("output");
    {
        let mut transaction = OutputTransaction::new(&output).unwrap();
        transaction
            .convert(
                &input,
                &output.join("new.wav"),
                OutputFormat::Preserve,
                &AtomicBool::new(false),
            )
            .unwrap();
        fs::write(output.join("unrelated.txt"), b"keep").unwrap();
    }
    assert!(!output.join("new.wav").exists());
    assert_eq!(fs::read(output.join("unrelated.txt")).unwrap(), b"keep");
}

#[test]
#[cfg(windows)]
fn locked_output_keeps_recovery_backup_until_a_later_rollback_can_restore_it() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("input.wav");
    source(&input, 44_100);
    let output = fixture.path().join("output");
    fs::create_dir(&output).unwrap();
    let destination = output.join("existing.wav");
    fs::write(&destination, b"original before job").unwrap();
    let mut transaction = OutputTransaction::new(&output).unwrap();
    transaction
        .convert(
            &input,
            &destination,
            OutputFormat::Preserve,
            &AtomicBool::new(false),
        )
        .unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&destination)
        .unwrap();
    let error = transaction.rollback().unwrap_err();
    assert!(format!("{error:#}").contains("복구 폴더"));
    let workspace = fs::read_dir(fixture.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".wav-converter-job-")
        })
        .unwrap()
        .path();
    assert!(workspace.join("recovery.txt").exists());
    let backup = fs::read_dir(&workspace)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("original-"))
        .unwrap()
        .path();
    assert_eq!(fs::read(backup).unwrap(), b"original before job");
    drop(lock);
    transaction.rollback().unwrap();
    assert_eq!(fs::read(destination).unwrap(), b"original before job");
    assert!(!workspace.exists());
}

#[test]
#[cfg(windows)]
fn failed_source_cleanup_preparation_is_still_fully_reversible() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = tempfile::tempdir().unwrap();
    let first = fixture.path().join("first.wav");
    let second = fixture.path().join("second.wav");
    source(&first, 44_100);
    source(&second, 48_000);
    let first_bytes = fs::read(&first).unwrap();
    let second_bytes = fs::read(&second).unwrap();
    let first_output = fixture.path().join("new first.wav");
    let second_output = fixture.path().join("new second.wav");
    let mut transaction = OutputTransaction::new(fixture.path()).unwrap();
    let cancel = AtomicBool::new(false);
    transaction
        .convert(&first, &first_output, OutputFormat::Preserve, &cancel)
        .unwrap();
    transaction.defer_source_removal(&first);
    transaction
        .convert(&second, &second_output, OutputFormat::Preserve, &cancel)
        .unwrap();
    transaction.defer_source_removal(&second);
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&second)
        .unwrap();
    assert!(transaction.commit().is_err());
    assert!(!transaction.is_committed());
    assert!(!first.exists()); // Journaled before the second source failed.
    transaction.rollback().unwrap();
    drop(lock);
    assert_eq!(fs::read(first).unwrap(), first_bytes);
    assert_eq!(fs::read(second).unwrap(), second_bytes);
    assert!(!first_output.exists());
    assert!(!second_output.exists());
}
