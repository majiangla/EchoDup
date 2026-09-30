//! 音频指纹提取（audiofp / Wang 算法）。
//! 特征提取层从 log-mel 声学特征切换到音频指纹；匹配算法待用户确认指纹结果后接入。

use anyhow::Result;

/// Wang 指纹提取结果。
pub struct FingerprintData {
    /// (t_anchor, hash) 哈希对；t_anchor 为帧号，绝对时间 = t_anchor / frames_per_sec 秒
    pub hashes: Vec<(u32, u32)>,
    pub frames_per_sec: f32,
    pub algorithm: &'static str,
    /// 指纹覆盖时长（秒，按最后一帧换算）
    pub duration_sec: f64,
}

/// 提取整段音频的 Wang 指纹。
/// 输入：mono f32 采样（[-1,1]）及原始采样率；内部用窗函数 sinc 重采样到 8kHz。
pub fn extract_fingerprint(samples: &[f32], sr: u32) -> Result<FingerprintData> {
    use audiofp::classical::Wang;
    use audiofp::dsp::resample::SincResampler;
    use audiofp::{Fingerprinter, SampleRate};

    let mono = if sr == 8000 {
        samples.to_vec()
    } else {
        SincResampler::new(sr, 8000).process(samples)
    };

    let mut wang = Wang::default();
    let fp = wang
        .extract(&mono, SampleRate::HZ_8000)
        .map_err(|e| anyhow::anyhow!("audiofp extract: {e}"))?;

    let fps = fp.frames_per_sec;
    let hashes: Vec<(u32, u32)> = fp.hashes.iter().map(|h| (h.t_anchor, h.hash)).collect();
    let last_frame = hashes.iter().map(|&(t, _)| t).max().unwrap_or(0) as f64;
    let duration_sec = if fps > 0.0 { last_frame / fps as f64 } else { 0.0 };

    Ok(FingerprintData {
        hashes,
        frames_per_sec: fps,
        algorithm: "wang-v1",
        duration_sec,
    })
}