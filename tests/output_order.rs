use std::path::PathBuf;
use wav_converter::output_order::arrange_output;

#[test]
fn empty_job_has_no_verified_directories_and_leaves_existing_files_alone() {
    let fixture = tempfile::tempdir().unwrap();
    let existing = fixture.path().join("07 existing.wav");
    std::fs::write(&existing, b"existing song").unwrap();

    let report = arrange_output(fixture.path(), &[]).unwrap();

    assert!(!report.physical_order_verified);
    assert!(report.checked_directories.is_empty());
    assert_eq!(std::fs::read(existing).unwrap(), b"existing song");
    assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 1);
    let missing_root = fixture.path().join("not created");
    let report = arrange_output(&missing_root, &[]).unwrap();
    assert!(!report.physical_order_verified);
    assert!(report.checked_directories.is_empty());
    assert!(!missing_root.exists());
}

#[test]
fn missing_output_fails_before_rebuilding_existing_directories() {
    let fixture = tempfile::tempdir().unwrap();
    let album = fixture.path().join("album");
    std::fs::create_dir(&album).unwrap();
    let existing = album.join("07 existing.wav");
    let missing = album.join("01 missing.wav");
    std::fs::write(&existing, b"existing song").unwrap();

    let error = arrange_output(&album, &[existing.clone(), missing.clone()]).unwrap_err();

    assert!(format!("{error:#}").contains(&missing.display().to_string()));
    assert_eq!(std::fs::read(&existing).unwrap(), b"existing song");
    assert_eq!(std::fs::read_dir(&album).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 1);
}

#[test]
fn directory_cannot_stand_in_for_a_completed_output_file() {
    let fixture = tempfile::tempdir().unwrap();
    let directory = fixture.path().join("01 not a file.wav");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("keep.txt"), b"untouched").unwrap();

    let error = arrange_output(fixture.path(), std::slice::from_ref(&directory)).unwrap_err();

    assert!(format!("{error:#}").contains("일반 파일이 아닙니다"));
    assert_eq!(
        std::fs::read(directory.join("keep.txt")).unwrap(),
        b"untouched"
    );
}

#[test]
#[cfg(any(unix, windows))]
#[cfg_attr(windows, ignore = "requires Windows symbolic-link privilege")]
fn linked_output_is_rejected_without_moving_its_target() {
    let fixture = tempfile::tempdir().unwrap();
    let album = fixture.path().join("album");
    std::fs::create_dir(&album).unwrap();
    let target = fixture.path().join("original.wav");
    let link = album.join("01 linked.wav");
    std::fs::write(&target, b"original song").unwrap();
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(&target, &link);
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(&target, &link);
    result.unwrap();

    let error = arrange_output(&album, std::slice::from_ref(&link)).unwrap_err();

    assert!(format!("{error:#}").contains("일반 파일이 아닙니다"));
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read(target).unwrap(), b"original song");
}

#[test]
fn drive_root_uses_dedicated_output_folder() {
    use wav_converter::output_order::effective_output_dir;
    #[cfg(windows)]
    let root = std::path::Path::new("E:\\");
    #[cfg(not(windows))]
    let root = std::path::Path::new("/");
    assert_eq!(effective_output_dir(root, true), root.join("WAV"));
    assert_eq!(effective_output_dir(root, false), root);
    let album = root.join("Album");
    assert_eq!(effective_output_dir(&album, true), album);
}

#[test]
#[ignore = "requires WAV_ORDER_TEST_ROOT pointing to an attached FAT card"]
fn fat_card_repeated_rebuild_with_holes_and_unpadded_numbers() {
    let root = std::env::var_os("WAV_ORDER_TEST_ROOT").expect("card root");
    let fixture = tempfile::tempdir_in(root).unwrap();
    let album = fixture.path().join("album");
    std::fs::create_dir(&album).unwrap();
    let mut paths = Vec::new();
    for number in (14..=28).chain(1..=13) {
        let file = album.join(format!(
            "{number}. {}.wav",
            "long name ".repeat(number % 4 + 1)
        ));
        std::fs::write(&file, number.to_string()).unwrap();
        paths.push(file);
    }
    for _ in 0..3 {
        let hole = album.join("temporary long filename creating deleted slots.tmp");
        std::fs::write(&hole, b"temporary").unwrap();
        std::fs::remove_file(hole).unwrap();
        assert!(
            arrange_output(&album, &paths)
                .unwrap()
                .physical_order_verified
        );
        let actual: Vec<usize> = std::fs::read_dir(&album)
            .unwrap()
            .map(|e| {
                e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .split('.')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect();
        assert_eq!(actual, (1..=28).collect::<Vec<_>>());
        for path in &paths {
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .split('.')
                    .next()
                    .unwrap()
            );
        }
    }
}

fn fixture(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("wav-order-{label}-{stamp}"));
    std::fs::create_dir(&root).unwrap();
    root
}

#[test]
fn existing_disc_two_first_is_reinserted_without_changing_files() {
    let root = fixture("existing");
    let mut outputs = Vec::new();
    for disc in ["Disc 2", "Disc 1"] {
        let dir = root.join(disc);
        std::fs::create_dir(&dir).unwrap();
        for track in ["03.wav", "01.wav", "02.wav"] {
            let path = dir.join(track);
            std::fs::write(&path, format!("{disc}/{track}")).unwrap();
            outputs.push(path);
        }
    }
    std::fs::write(root.join("cover.jpg"), b"keep cover").unwrap();
    let previous_song = root.join("Disc 2").join("04 previous.wav");
    std::fs::write(&previous_song, b"keep previous song").unwrap();
    for _ in 0..2 {
        let report = arrange_output(&root, &outputs).unwrap();
        assert_eq!(report.checked_directories.len(), 3);
        for (directory, names) in &report.checked_directories {
            let actual: Vec<_> = std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(*names, actual);
        }
        assert!(report.checked_directories.iter().any(|(directory, names)| {
            directory == &root.join("Disc 2")
                && names.contains(&std::ffi::OsString::from("04 previous.wav"))
        }));
        assert_eq!(
            std::fs::read(&previous_song).unwrap(),
            b"keep previous song"
        );
        for path in &outputs {
            let relative = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            assert_eq!(std::fs::read_to_string(path).unwrap(), relative);
        }
        assert_eq!(
            std::fs::read(root.join("cover.jpg")).unwrap(),
            b"keep cover"
        );
        let discs: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.file_type().unwrap().is_dir())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(discs, ["Disc 1", "Disc 2"]);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(windows)]
fn locked_file_rolls_back_already_moved_entries() {
    use std::os::windows::fs::OpenOptionsExt;
    let root = fixture("locked");
    let first = root.join("01.wav");
    let locked = root.join("02.wav");
    std::fs::write(&first, b"first").unwrap();
    std::fs::write(&locked, b"locked").unwrap();
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&locked)
        .unwrap();
    assert!(arrange_output(&root, &[first.clone(), locked.clone()]).is_err());
    drop(handle);
    assert_eq!(std::fs::read(&first).unwrap(), b"first");
    assert_eq!(std::fs::read(&locked).unwrap(), b"locked");
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
    std::fs::remove_dir_all(root).unwrap();
}
