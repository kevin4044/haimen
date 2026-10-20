//! 临时诊断工具：对比 Silero VAD 与能量阈值在同一段录音上的判别力。
//!
//! 用法：
//!   cargo run --release --example vad_probe -- /tmp/vad_test.wav
//!
//! 输出每 32ms（512 样本）一行的概率与 RMS，方便直观看两种判据的分野。
//!
//! 存在的意义：能量阈值（`SILENCE_RMS_THRESHOLD = 3000`）在真机上是拍脑袋定的，
//! 而实测人说话时 RMS 只有 800~1500 —— 阈值高了一倍多。这个工具用来拿数据
//! 决定「到底该不该换成 Silero」。

use std::env;
use std::fs;

use voice_activity_detector::VoiceActivityDetector;

/// Silero 要求 16kHz 下每块恰好 512 个样本（32ms）。
const CHUNK: usize = 512;
const SAMPLE_RATE: u32 = 16_000;

/// 当前生产代码用的能量阈值，用来做对照。
const RMS_THRESHOLD: f64 = 3000.0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args()
        .nth(1)
        .ok_or("用法: vad_probe <16kHz mono 16-bit WAV>")?;
    let samples = read_wav_i16(&path)?;
    println!(
        "样本数 {}（{:.1} 秒）",
        samples.len(),
        samples.len() as f64 / SAMPLE_RATE as f64
    );

    let mut vad = VoiceActivityDetector::builder()
        .sample_rate(SAMPLE_RATE)
        .chunk_size(CHUNK)
        .build()?;

    println!();
    println!(" 时间    Silero   能量RMS   能量判据   Silero判据");
    println!(" ------  -------  --------  ---------  ----------");

    let mut silero_voice = 0usize;
    let mut energy_voice = 0usize;
    let mut total = 0usize;
    // 统计两种判据的一致率，这才是「换不换」的直接依据
    let mut agree = 0usize;

    for (idx, chunk) in samples.chunks_exact(CHUNK).enumerate() {
        let prob = vad.predict(chunk.to_vec());
        let rms = rms_of(chunk);

        let silero_is_voice = prob > 0.5;
        let energy_is_voice = rms >= RMS_THRESHOLD;

        total += 1;
        if silero_is_voice {
            silero_voice += 1;
        }
        if energy_is_voice {
            energy_voice += 1;
        }
        if silero_is_voice == energy_is_voice {
            agree += 1;
        }

        // 每 10 块（约 320ms）打一行，避免刷屏
        if idx % 10 == 0 {
            println!(
                " {:5.2}s  {:7.3}  {:8.0}  {:9}  {}",
                idx as f64 * CHUNK as f64 / SAMPLE_RATE as f64,
                prob,
                rms,
                if energy_is_voice { "人声" } else { "静音" },
                if silero_is_voice { "人声" } else { "静音" },
            );
        }
    }

    println!();
    println!(
        "统计（共 {} 块 = {:.1} 秒）",
        total,
        total as f64 * CHUNK as f64 / SAMPLE_RATE as f64
    );
    println!(
        "  Silero 判为人声：{:5.1}%",
        silero_voice as f64 * 100.0 / total as f64
    );
    println!(
        "  能量判为人声：  {:5.1}%  （阈值 {:.0}）",
        energy_voice as f64 * 100.0 / total as f64,
        RMS_THRESHOLD
    );
    println!(
        "  两者一致率：    {:5.1}%",
        agree as f64 * 100.0 / total as f64
    );

    Ok(())
}

/// 去直流后的 RMS（与生产代码 `compute_pcm_stats` 同一算法）。
fn rms_of(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mean = samples.iter().map(|&s| s as f64).sum::<f64>() / samples.len() as f64;
    let sum_sq: f64 = samples
        .iter()
        .map(|&s| {
            let d = s as f64 - mean;
            d * d
        })
        .sum();
    (sum_sq / samples.len() as f64).sqrt()
}

/// 读 16kHz 单声道 16-bit WAV 的 PCM 数据。
///
/// 只支持这一种格式：诊断工具够用，不引第三方解码库。
fn read_wav_i16(path: &str) -> Result<Vec<i16>, Box<dyn std::error::Error>> {
    let bytes = fs::read(path)?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("不是合法 WAV".into());
    }

    // 逐块找 fmt / data，不假定固定偏移
    let mut pos = 12usize;
    let mut channels = 1u16;
    let mut sample_rate = SAMPLE_RATE;
    let mut bits = 16u16;
    let mut data_start = None;
    let mut data_len = 0usize;

    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body = pos + 8;

        if id == b"fmt " && body + 16 <= bytes.len() {
            channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
            sample_rate = u32::from_le_bytes([
                bytes[body + 4],
                bytes[body + 5],
                bytes[body + 6],
                bytes[body + 7],
            ]);
            bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
        } else if id == b"data" {
            data_start = Some(body);
            data_len = size.min(bytes.len().saturating_sub(body));
            break;
        }
        pos = body + size + (size & 1); // 块按偶数字节对齐
    }

    let start = data_start.ok_or("WAV 里找不到 data 块")?;
    if channels != 1 || bits != 16 || sample_rate != SAMPLE_RATE {
        return Err(format!(
            "只支持 16kHz/mono/16bit，实际 {sample_rate}Hz/{channels}ch/{bits}bit"
        )
        .into());
    }

    Ok(bytes[start..start + data_len]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect())
}
