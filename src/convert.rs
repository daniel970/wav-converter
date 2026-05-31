//! 음원 디코딩 → (필요시) 리샘플링 → WAV 인코딩 핵심 로직.
//!
//! - 디코딩: symphonia (mp3/flac/m4a/aac/alac/ogg/wav/aiff 등 거의 모든 포맷)
//! - 리샘플링: rubato (고품질 Sinc)
//! - 인코딩: hound (PCM WAV)

use std::fs::File;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// 사용자가 GUI 드롭다운에서 고르는 출력 규격.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// 원본의 샘플레이트/채널을 그대로 유지 (리샘플링 없음, 음질 손실 없음).
    Preserve,
    Pcm16_44100,
    Pcm16_48000,
    Pcm24_48000,
    Pcm24_96000,
}

impl OutputFormat {
    /// 드롭다운에 표시할 모든 항목 (순서 고정).
    pub const ALL: [OutputFormat; 5] = [
        OutputFormat::Preserve,
        OutputFormat::Pcm16_44100,
        OutputFormat::Pcm16_48000,
        OutputFormat::Pcm24_48000,
        OutputFormat::Pcm24_96000,
    ];

    pub fn label(self) -> &'static str {
        match self {
            OutputFormat::Preserve => "원본 유지 (무손실)",
            OutputFormat::Pcm16_44100 => "16bit · 44.1kHz (CD 표준)",
            OutputFormat::Pcm16_48000 => "16bit · 48kHz",
            OutputFormat::Pcm24_48000 => "24bit · 48kHz",
            OutputFormat::Pcm24_96000 => "24bit · 96kHz (하이레졸루션)",
        }
    }

    /// 목표 샘플레이트. None이면 원본 유지.
    fn target_rate(self) -> Option<u32> {
        match self {
            OutputFormat::Preserve => None,
            OutputFormat::Pcm16_44100 => Some(44_100),
            OutputFormat::Pcm16_48000 | OutputFormat::Pcm24_48000 => Some(48_000),
            OutputFormat::Pcm24_96000 => Some(96_000),
        }
    }

    /// 목표 비트심도. None이면 원본 추정값 사용.
    fn target_bits(self) -> Option<u16> {
        match self {
            OutputFormat::Preserve => None,
            OutputFormat::Pcm16_44100 | OutputFormat::Pcm16_48000 => Some(16),
            OutputFormat::Pcm24_48000 | OutputFormat::Pcm24_96000 => Some(24),
        }
    }
}

/// symphonia로 디코딩한 결과 (인터리브 f32 샘플 + 메타).
struct Decoded {
    /// 인터리브된 샘플 [L,R,L,R,...] (모노면 [M,M,...]). 범위는 대략 [-1.0, 1.0].
    samples: Vec<f32>,
    sample_rate: u32,
    channels: usize,
    /// 원본 비트심도 추정 (lossy 코덱은 알 수 없어 None).
    src_bits: Option<u32>,
}

/// 파일 하나를 디코딩 → 변환 → WAV로 저장.
pub fn convert_file(input: &Path, output: &Path, fmt: OutputFormat) -> Result<()> {
    let decoded = decode(input).with_context(|| format!("디코딩 실패: {}", input.display()))?;

    let target_rate = fmt.target_rate().unwrap_or(decoded.sample_rate);
    let target_bits = fmt.target_bits().unwrap_or_else(|| {
        // 원본 유지: 원본이 16bit 초과면 24bit, 아니면 16bit.
        match decoded.src_bits {
            Some(b) if b > 16 => 24,
            _ => 16,
        }
    });

    // 필요하면 리샘플링.
    let samples = if target_rate != decoded.sample_rate {
        resample(
            &decoded.samples,
            decoded.channels,
            decoded.sample_rate,
            target_rate,
        )
        .with_context(|| format!("리샘플링 실패: {}", input.display()))?
    } else {
        decoded.samples
    };

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("폴더 생성 실패: {}", parent.display()))?;
    }

    write_wav(output, &samples, decoded.channels, target_rate, target_bits)
        .with_context(|| format!("WAV 저장 실패: {}", output.display()))?;

    Ok(())
}

/// symphonia로 임의 포맷을 인터리브 f32로 디코딩.
fn decode(path: &Path) -> Result<Decoded> {
    let file = File::open(path).with_context(|| format!("파일 열기 실패: {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .context("지원하지 않는 포맷이거나 손상된 파일")?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("오디오 트랙을 찾을 수 없음"))?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .context("이 코덱용 디코더가 없음")?;

    let channels = codec_params
        .channels
        .map(|c| c.count())
        .ok_or_else(|| anyhow!("채널 정보 없음"))?;
    let sample_rate = codec_params
        .sample_rate
        .ok_or_else(|| anyhow!("샘플레이트 정보 없음"))?;
    let src_bits = codec_params.bits_per_sample;

    let mut samples: Vec<f32> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // 스트림 끝.
            Err(SymphoniaError::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(SymphoniaError::ResetRequired) => {
                // 트랙 구성 변경: 단순화를 위해 여기서 종료.
                break;
            }
            Err(e) => return Err(e).context("패킷 읽기 실패"),
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                if sample_buf.is_none() {
                    let spec = *audio_buf.spec();
                    let duration = audio_buf.capacity() as u64;
                    sample_buf = Some(SampleBuffer::<f32>::new(duration, spec));
                }
                let buf = sample_buf.as_mut().unwrap();
                buf.copy_interleaved_ref(audio_buf);
                samples.extend_from_slice(buf.samples());
            }
            // 일부 손상 패킷은 건너뛰고 계속 진행.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(e).context("디코딩 실패"),
        }
    }

    if samples.is_empty() {
        return Err(anyhow!("디코딩된 오디오 샘플이 없음"));
    }

    Ok(Decoded {
        samples,
        sample_rate,
        channels,
        src_bits,
    })
}

/// 인터리브 샘플을 rubato로 리샘플링. 내부적으로 채널 분리 → 처리 → 재인터리브.
fn resample(
    interleaved: &[f32],
    channels: usize,
    from_rate: u32,
    to_rate: u32,
) -> Result<Vec<f32>> {
    use rubato::{
        Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
        WindowFunction,
    };

    // 채널 분리 (planar).
    let frames = interleaved.len() / channels;
    let mut planar: Vec<Vec<f32>> = vec![Vec::with_capacity(frames); channels];
    for frame in interleaved.chunks_exact(channels) {
        for (ch, &s) in frame.iter().enumerate() {
            planar[ch].push(s);
        }
    }

    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };

    let chunk = 1024;
    let mut resampler = SincFixedIn::<f32>::new(
        to_rate as f64 / from_rate as f64,
        2.0,
        params,
        chunk,
        channels,
    )
    .context("리샘플러 생성 실패")?;

    let mut out_planar: Vec<Vec<f32>> = vec![Vec::new(); channels];

    let mut pos = 0;
    while pos + chunk <= frames {
        let block: Vec<&[f32]> = planar.iter().map(|c| &c[pos..pos + chunk]).collect();
        let processed = resampler.process(&block, None).context("리샘플 처리 실패")?;
        for (ch, data) in processed.into_iter().enumerate() {
            out_planar[ch].extend(data);
        }
        pos += chunk;
    }

    // 남은 부분 (chunk 미만)은 process_partial로 마무리.
    if pos < frames {
        let block: Vec<Vec<f32>> = planar.iter().map(|c| c[pos..].to_vec()).collect();
        let refs: Vec<&[f32]> = block.iter().map(|v| v.as_slice()).collect();
        let processed = resampler
            .process_partial(Some(refs.as_slice()), None)
            .context("리샘플 마무리 실패")?;
        for (ch, data) in processed.into_iter().enumerate() {
            out_planar[ch].extend(data);
        }
    }

    // 재인터리브.
    let out_frames = out_planar[0].len();
    let mut out = Vec::with_capacity(out_frames * channels);
    for f in 0..out_frames {
        for ch in 0..channels {
            out.push(out_planar[ch][f]);
        }
    }
    Ok(out)
}

/// 인터리브 f32 샘플을 PCM WAV로 저장.
fn write_wav(
    output: &Path,
    interleaved: &[f32],
    channels: usize,
    sample_rate: u32,
    bits: u16,
) -> Result<()> {
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate,
        bits_per_sample: bits,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(output, spec).context("WAV 파일 생성 실패")?;

    match bits {
        16 => {
            for &s in interleaved {
                let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
                writer.write_sample(v)?;
            }
        }
        24 => {
            const MAX_24: f32 = 8_388_607.0; // 2^23 - 1
            for &s in interleaved {
                let v = (s.clamp(-1.0, 1.0) * MAX_24).round() as i32;
                writer.write_sample(v)?;
            }
        }
        other => return Err(anyhow!("지원하지 않는 비트심도: {other}")),
    }

    writer.finalize().context("WAV 마무리 실패")?;
    Ok(())
}

/// 변환 대상으로 시도할 확장자들. (실제 지원 여부는 디코딩 시점에 판단)
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "wav", "m4a", "aac", "ogg", "oga", "aiff", "aif", "aifc", "alac", "caf", "mp4",
    "mka", "mp1", "mp2", "wv", "ape",
];

/// 확장자가 오디오 후보인지.
pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            AUDIO_EXTENSIONS.contains(&e.as_str())
        })
        .unwrap_or(false)
}
