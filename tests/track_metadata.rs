use wav_converter::naming::read_track_number;

#[test]
fn reads_id3_track_number_without_decoding_audio() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("song.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 44100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).unwrap();
    writer.write_sample(1i16).unwrap();
    writer.finalize().unwrap();
    assert_eq!(read_track_number(&path).unwrap(), None);
    let audio = std::fs::read(&path).unwrap();
    // ID3v2.3 TRCK frame: text encoding 0, value "03/28".
    let mut tagged =
        b"ID3\x03\x00\x00\x00\x00\x00\x10TRCK\x00\x00\x00\x06\x00\x00\x0003/28".to_vec();
    tagged.extend(audio);
    std::fs::write(&path, tagged).unwrap();
    assert_eq!(read_track_number(&path).unwrap(), Some(3));
}
