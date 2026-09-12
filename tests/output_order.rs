use std::path::PathBuf;
use wav_converter::output_order::arrange_output;

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
    for _ in 0..2 {
        arrange_output(&root, &outputs).unwrap();
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
