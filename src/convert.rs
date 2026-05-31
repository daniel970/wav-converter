//! 음원 디코딩 → (필요시) 리샘플링 → WAV 인코딩 핵심 로직.
//!
//! - 디코딩: symphonia (mp3/flac/m4a/aac/alac/ogg/wav/aiff 등 거의 모든 포맷)
//! - 리샘플링: rubato (고품질 Sinc)
//! - 인코딩: hound (PCM WAV)
//!
//! 메모리 안전성: 파일 전체를 메모리에 올리지 않고 **패킷 단위로 스트리밍**하여
//! 변환한다. 1시간짜리 무손실 음원도 일정한 적은 메모리만 사용한다.

use std::fs::File;
use std::io::{Seek, Write};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader};
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

/// 디코딩 컨텍스트 (스트리밍용).
struct DecCtx {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    channels: usize,
    sample_rate: u32,
    src_bits: Option<u32>,
}

/// 파일 하나를 디코딩 → 변환 → WAV로 저장 (스트리밍).
pub fn convert_file(input: &Path, output: &Path, fmt: OutputFormat) -> Result<()> {
    let mut ctx = open_decoder(input)
        .with_context(|| format!("디코딩 준비 실패: {}", input.display()))?;

    let target_rate = fmt.target_rate().unwrap_or(ctx.sample_rate);
    let target_bits = fmt.target_bits().unwrap_or_else(|| match ctx.src_bits {
        Some(b) if b > 16 => 24,
        _ => 16,
    });

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("폴더 생성 실패: {}", parent.display()))?;
    }

    let spec = hound::WavSpec {
        channels: ctx.channels as u16,
        sample_rate: target_rate,
        bits_per_sample: target_bits,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(output, spec)
        .with_context(|| format!("WAV 파일 생성 실패: {}", output.display()))?;

    if target_rate == ctx.sample_rate {
        stream_direct(&mut ctx, &mut writer, target_bits)
            .with_context(|| format!("변환 실패: {}", input.display()))?;
    } else {
        stream_resampled(&mut ctx, &mut writer, target_rate, target_bits)
            .with_context(|| format!("리샘플 변환 실패: {}", input.display()))?;
    }

    writer
        .finalize()
        .with_context(|| format!("WAV 마무리 실패: {}", output.display()))?;
    Ok(())
}

/// 원본 대체 변환: 임시 파일로 변환 후, 원본 삭제 + 임시를 최종 `.wav`로 이동.
/// (mp3 등은 원본 삭제 후 .wav 생성, 이미 .wav면 제자리 덮어쓰기)
pub fn convert_in_place(file: &Path, fmt: OutputFormat) -> Result<()> {
    let final_path = file.with_extension("wav");
    let tmp = file.with_extension("wavtmp");

    convert_file(file, &tmp, fmt)?;

    // 원본 확장자가 wav가 아니면(=원본과 최종 경로가 다르면) 원본 삭제.
    if file != final_path {
        std::fs::remove_file(file)
            .with_context(|| format!("원본 삭제 실패: {}", file.display()))?;
    }
    std::fs::rename(&tmp, &final_path)
        .with_context(|| format!("임시 파일 이동 실패: {}", final_path.display()))?;
    Ok(())
}

/// symphonia 디코더를 열고 트랙 정보를 수집.
fn open_decoder(path: &Path) -> Result<DecCtx> {
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

    let format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("오디오 트랙을 찾을 수 없음"))?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();

    let decoder = symphonia::default::get_codecs()
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

    Ok(DecCtx {
        format,
        decoder,
        track_id,
        channels,
        sample_rate,
        src_bits,
    })
}

/// 패킷을 하나씩 디코딩하며 인터리브 f32 슬라이스를 콜백에 넘긴다.
/// (메모리 사용을 일정하게 유지)
fn decode_loop(ctx: &mut DecCtx, mut on_samples: impl FnMut(&[f32]) -> Result<()>) -> Result<()> {
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut cap: u64 = 0;
    let mut got_any = false;

    loop {
        let packet = match ctx.format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(e).context("패킷 읽기 실패"),
        };

        if packet.track_id() != ctx.track_id {
            continue;
        }

        match ctx.decoder.decode(&packet) {
            Ok(audio) => {
                let need = audio.capacity() as u64;
                if sample_buf.is_none() || need > cap {
                    let spec = *audio.spec();
                    sample_buf = Some(SampleBuffer::<f32>::new(need, spec));
                    cap = need;
                }
                let sb = sample_buf.as_mut().unwrap();
                sb.copy_interleaved_ref(audio);
                got_any = true;
                on_samples(sb.samples())?;
            }
            // 손상 패킷은 건너뛰고 계속.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(e).context("디코딩 실패"),
        }
    }

    if !got_any {
        return Err(anyhow!("디코딩된 오디오 샘플이 없음"));
    }
    Ok(())
}

/// 리샘플링 없이 바로 기록.
fn stream_direct<W: Write + Seek>(
    ctx: &mut DecCtx,
    writer: &mut hound::WavWriter<W>,
    bits: u16,
) -> Result<()> {
    decode_loop(ctx, |interleaved| write_interleaved(writer, interleaved, bits))
}

/// 리샘플링하며 스트리밍 기록.
fn stream_resampled<W: Write + Seek>(
    ctx: &mut DecCtx,
    writer: &mut hound::WavWriter<W>,
    target_rate: u32,
    bits: u16,
) -> Result<()> {
    use rubato::{
        Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
        WindowFunction,
    };

    let channels = ctx.channels;
    let from_rate = ctx.sample_rate;

    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };

    let chunk = 1024;
    let mut resampler = SincFixedIn::<f32>::new(
        target_rate as f64 / from_rate as f64,
        2.0,
        params,
        chunk,
        channels,
    )
    .context("리샘플러 생성 실패")?;

    // 채널별 대기 버퍼 (chunk 만큼 모이면 처리).
    let mut pending: Vec<Vec<f32>> = vec![Vec::with_capacity(chunk * 2); channels];

    decode_loop(ctx, |interleaved| {
        for frame in interleaved.chunks_exact(channels) {
            for (c, &s) in frame.iter().enumerate() {
                pending[c].push(s);
            }
        }
        while pending[0].len() >= chunk {
            let block: Vec<Vec<f32>> = pending.iter().map(|c| c[..chunk].to_vec()).collect();
            for c in pending.iter_mut() {
                c.drain(..chunk);
            }
            let refs: Vec<&[f32]> = block.iter().map(|v| v.as_slice()).collect();
            let out = resampler.process(&refs, None).context("리샘플 처리 실패")?;
            write_planar(writer, &out, bits)?;
        }
        Ok(())
    })?;

    // 남은 부분 flush.
    if !pending[0].is_empty() {
        let refs: Vec<&[f32]> = pending.iter().map(|v| v.as_slice()).collect();
        let out = resampler
            .process_partial(Some(refs.as_slice()), None)
            .context("리샘플 마무리 실패")?;
        write_planar(writer, &out, bits)?;
    }

    Ok(())
}

/// 인터리브 f32 → PCM 기록.
fn write_interleaved<W: Write + Seek>(
    writer: &mut hound::WavWriter<W>,
    interleaved: &[f32],
    bits: u16,
) -> Result<()> {
    match bits {
        16 => {
            for &s in interleaved {
                writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16)?;
            }
        }
        24 => {
            const MAX_24: f32 = 8_388_607.0; // 2^23 - 1
            for &s in interleaved {
                writer.write_sample((s.clamp(-1.0, 1.0) * MAX_24).round() as i32)?;
            }
        }
        other => return Err(anyhow!("지원하지 않는 비트심도: {other}")),
    }
    Ok(())
}

/// 채널별(planar) f32 → 인터리브하여 PCM 기록.
fn write_planar<W: Write + Seek>(
    writer: &mut hound::WavWriter<W>,
    planar: &[Vec<f32>],
    bits: u16,
) -> Result<()> {
    if planar.is_empty() {
        return Ok(());
    }
    let frames = planar[0].len();
    let channels = planar.len();
    match bits {
        16 => {
            for f in 0..frames {
                for ch in planar.iter().take(channels) {
                    writer.write_sample((ch[f].clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16)?;
                }
            }
        }
        24 => {
            const MAX_24: f32 = 8_388_607.0;
            for f in 0..frames {
                for ch in planar.iter().take(channels) {
                    writer.write_sample((ch[f].clamp(-1.0, 1.0) * MAX_24).round() as i32)?;
                }
            }
        }
        other => return Err(anyhow!("지원하지 않는 비트심도: {other}")),
    }
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
