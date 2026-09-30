use anyhow::{anyhow, Result};
use rodio::Decoder;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub struct AudioData {
    pub mono: Vec<f32>,
    pub sample_rate: u32,
    /// 原始采样率 / 4 的降采样（44.1k→11025），供提示音 screst 频谱检测用
    /// （与 Slint `load_track` 的 pcm11k 构建一致：声道平均后每 4 帧平均，余数丢弃）。
    pub pcm11k: Vec<f32>,
    /// 原始采样率上的 5 kHz 降采样（audiofp Haitsma 指纹用，与 Slint `load_track` 一致：
    /// 窗口平均，ratio = sample_rate / 5000）。
    pub pcm5k: Vec<f32>,
    /// 原始采样率上构建的 ≈100 Hz 包络（与 Slint `load_track` 一致：窗口 = ratio×声道数
    /// 个交错样本的峰值/平均/RMS）。静音/绿线检测必须基于它，16 kHz 重采样包络的
    /// 边界会有 ±0.01 s 级差异，会改变 grow_min 二分临界处的静音条数。
    pub peak_env: Vec<f32>,
    pub avg_env: Vec<f32>,
    pub rms_env: Vec<f32>,
    pub original_sample_rate: u32,
    pub original_channels: u16,
    /// 包络采样率 = original_sample_rate / round(original_sample_rate / 100) ≈ 100 Hz。
    pub control_rate: f64,
}

pub fn decode(path: &Path, target_sr: u32) -> Result<AudioData> {
    // 与 Slint `load_track` 完全一致：rodio（mp3=minimp3 后端）解码为交错 f32 流。
    // 注意：不同 mp3 解码器（symphonia vs minimp3）的样本流有 ~0.04s 级边界差异，
    // 会改变 grow_min 二分临界处的静音条数，必须与 Slint 同源。
    let file = File::open(path)?;
    let decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| anyhow!("decode failed: {e}"))?;
    let original_sample_rate = decoder.sample_rate();
    let original_channels = decoder.channels();
    use rodio::Source;
    let mut samples: Vec<f32> = Vec::new();
    for s in decoder.convert_samples::<f32>() {
        samples.push(s);
    }

    let mut mono: Vec<f32> = Vec::new();

    // 原始采样率上构建 ≈100 Hz 包络（Slint `load_track` 逻辑：窗口 = ratio×ch 交错样本）。
    let ratio = ((original_sample_rate as f64 / 100.0).round() as usize).max(1);
    let win = ratio * original_channels as usize;
    let mut peak_env: Vec<f32> = Vec::new();
    let mut avg_env: Vec<f32> = Vec::new();
    let mut rms_env: Vec<f32> = Vec::new();
    let mut w_peak = 0.0f32;
    let mut w_abs = 0.0f64;
    let mut w_sq = 0.0f64;
    let mut w_cnt = 0usize;

    // ① 包络：交错流窗口（与 Slint 一致，窗口 = ratio×声道 个交错样本）。
    for &v in &samples {
        let a = v.abs();
        if a > w_peak {
            w_peak = a;
        }
        w_abs += v as f64;
        w_sq += (v as f64) * (v as f64);
        w_cnt += 1;
        if w_cnt == win {
            peak_env.push(w_peak);
            avg_env.push((w_abs / w_cnt as f64) as f32);
            rms_env.push((w_sq / w_cnt as f64).sqrt() as f32);
            w_peak = 0.0;
            w_abs = 0.0;
            w_sq = 0.0;
            w_cnt = 0;
        }
    }
    // 尾部不足一窗。
    if w_cnt > 0 {
        peak_env.push(w_peak);
        avg_env.push((w_abs / w_cnt as f64) as f32);
        rms_env.push((w_sq / w_cnt as f64).sqrt() as f32);
    }

    // ② 声道平均 → mono（供重采样/播放）。
    let ch = original_channels as usize;
    for frame in samples.chunks(ch) {
        let s: f32 = frame.iter().sum();
        mono.push(s / ch as f32);
    }

    // 原始采样率 mono 上构建 pcm11k（每 4 帧平均，余数丢弃，与 Slint 一致）。
    let mut pcm11k: Vec<f32> = Vec::new();
    {
        let mut acc = 0.0f64;
        let mut cnt = 0usize;
        for &v in &mono {
            acc += v as f64;
            cnt += 1;
            if cnt == 4 {
                pcm11k.push((acc / 4.0) as f32);
                acc = 0.0;
                cnt = 0;
            }
        }
    }

    // 原始采样率 mono 上构建 pcm5k（窗口平均，ratio = sample_rate / 5000，与 Slint 一致）。
    let mut pcm5k: Vec<f32> = Vec::new();
    {
        let ratio5k = original_sample_rate as f64 / 5000.0;
        let mut acc = 0.0f64;
        let mut cnt = 0.0f64;
        for &v in &mono {
            acc += v as f64;
            cnt += 1.0;
            if cnt >= ratio5k {
                pcm5k.push((acc / cnt) as f32);
                acc = 0.0;
                cnt = 0.0;
            }
        }
        if cnt > 0.0 {
            pcm5k.push((acc / cnt) as f32);
        }
    }

    let ratio = ((original_sample_rate as f64 / 100.0).round() as usize).max(1);
    let control_rate = original_sample_rate as f64 / ratio as f64;

    let mono = if original_sample_rate != target_sr {
        resample_linear(&mono, original_sample_rate, target_sr)
    } else {
        mono
    };

    Ok(AudioData {
        mono,
        sample_rate: target_sr,
        pcm11k,
        pcm5k,
        peak_env,
        avg_env,
        rms_env,
        original_sample_rate,
        original_channels,
        control_rate,
    })
}
fn resample_linear(input: &[f32], from_sr: u32, to_sr: u32) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    let ratio = from_sr as f64 / to_sr as f64;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 * ratio;
        let idx = src as usize;
        let frac = (src - idx as f64) as f32;
        let s0 = *input.get(idx).unwrap_or(&0.0);
        let s1 = *input.get(idx + 1).unwrap_or(&s0);
        out.push(s0 * (1.0 - frac) + s1 * frac);
    }
    out
}
