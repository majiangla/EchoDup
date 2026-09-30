//! 红蓝紫（A/B/C）分段检测 —— 从 EchoDup-Slint（EasySliceAudioPlayer 切分算法台）
//! 完整移植到 EchoDup。
//!
//! 取代原 detect（Wang 音频指纹）作为 ASR 前的检测步骤。语义：
//! - **A/B（红蓝）**：重复内容对。按静音切分 + 提示音（绿线）+ 边界区（橙块）把
//!   音频切成"部分"，部分内静音把音频切成子段，按时长相似 + 音频指纹（Haitsma）
//!   认可的一对记 A / B；ASR 只需转写 A（B 是重复）。
//! - **C（淡紫）**：橙灰相间中连续灰块升级的不重复段，直接 ASR。
//!
//! 与 Slint 侧实现保持同源：包络 / pcm11k / pcm5k 由 `audio_io::decode` 在原始
//! 采样率上构建（rodio 同源解码），本模块只做检测编排。

use crate::core::audio_io::AudioData;

/// 时间区间（秒）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub start: f64,
    pub end: f64,
}

// ============================================================
// 静音检测（Audacity label-sounds.ny TYPE 3 移植，Peak 测量）
// ============================================================

fn threshold_linear(threshold_db: f64) -> f64 {
    10f64.powf(threshold_db / 20.0)
}

/// 在 100 Hz 包络上找声音段（find_sounds 移植，Peak 测量）。
fn find_sounds(envelope: &[f32], control_rate: f64, threshold_db: f64, min_sound: f64, min_silence: f64) -> Vec<Span> {
    let thresh = threshold_linear(threshold_db);
    let snd_samples = min_sound * control_rate;
    let sil_samples = min_silence * control_rate;

    let mut sounds: Vec<Span> = Vec::new();
    let mut sil_count: u64 = 0;
    let mut snd_count: u64 = 0;
    let mut snd_start: u64 = 0;

    for (i, &v) in envelope.iter().enumerate() {
        let sample_count = i as u64;
        if (v as f64) < thresh {
            if (sil_count as f64) >= sil_samples && (snd_count as f64) >= snd_samples {
                sounds.push(Span {
                    start: snd_start as f64 / control_rate,
                    end: (sample_count - sil_count) as f64 / control_rate,
                });
                snd_count = 0;
            }
            if snd_count > 0 {
                snd_count += 1;
            }
            sil_count += 1;
        } else {
            if snd_count == 0 {
                snd_start = sample_count;
            }
            sil_count = 0;
            snd_count += 1;
        }
    }
    if snd_count > 0 {
        sounds.push(Span {
            start: snd_start as f64 / control_rate,
            end: (envelope.len() as u64 - sil_count) as f64 / control_rate,
        });
    }
    sounds
}

/// 静音区域（声音之间的间隙 + 首尾静音）。
fn silence_regions(sounds: &[Span], duration: f64) -> Vec<Span> {
    let mut regions = Vec::new();
    match sounds.first() {
        Some(first) => {
            if first.start > 0.0 {
                regions.push(Span { start: 0.0, end: first.start });
            }
        }
        None => {
            return if duration > 0.0 {
                vec![Span { start: 0.0, end: duration }]
            } else {
                Vec::new()
            };
        }
    }
    if sounds.len() >= 2 {
        for pair in sounds.windows(2) {
            regions.push(Span { start: pair[0].end, end: pair[1].start });
        }
    }
    if let Some(last) = sounds.last() {
        if last.end < duration {
            regions.push(Span { start: last.end, end: duration });
        }
    }
    regions
}

/// 一步静音检测：返回静音区间列表。
pub fn detect_silences(
    peak_env: &[f32],
    control_rate: f64,
    duration: f64,
    threshold_db: f64,
    min_silence: f64,
    min_sound: f64,
) -> Vec<Span> {
    let sounds = find_sounds(peak_env, control_rate, threshold_db, min_sound, min_silence);
    silence_regions(&sounds, duration)
}

// ============================================================
// 绿线（切分点）：包络 crest 峰 + ≥4 s 长停顿合并
// ============================================================

/// 用包络近似 FFmpeg aspectralstats 的 crest 序列，返回 (帧时间 ms, crest 值)。
fn crest_series(peak_env: &[f32], rms_env: &[f32], control_rate: f64, sample_rate: u32) -> Vec<(i64, f64)> {
    let n = peak_env.len().min(rms_env.len());
    if n == 0 {
        return Vec::new();
    }
    let ratio = (sample_rate as f64 / control_rate).round().max(1.0);
    let win_n = ((49152.0 / ratio).round() as usize).max(1);
    let hop_n = ((win_n as f64 * 0.4).round() as usize).max(1);

    let mut out = Vec::new();
    let mut k = 0usize;
    while k < n {
        let end = (k + win_n).min(n);
        let cnt = (end - k) as f64;
        let mut peak = 0.0f32;
        let mut sq = 0.0f64;
        for i in k..end {
            if peak_env[i] > peak {
                peak = peak_env[i];
            }
            let r = rms_env[i] as f64;
            sq += r * r;
        }
        let rms = (sq / cnt).sqrt();
        let crest = if rms > 1e-9 { (peak as f64) / rms } else { 0.0 };
        let t_ms = (k as f64 / control_rate * 1000.0).round() as i64;
        out.push((t_ms, crest));
        k += hop_n;
    }
    out
}

/// 计算切分位置（绿线，秒，升序去重）。
/// crest 阈值低→高重跑一次（≥20 个切分点则升到 1100）；停顿结束点距已有切分点
/// >2 s 时补加；**开头 5 s 内的切分点不承认**。
pub fn analyse_slices(
    peak_env: &[f32],
    rms_env: &[f32],
    control_rate: f64,
    sample_rate: u32,
    duration: f64,
) -> Vec<f64> {
    let crest = crest_series(peak_env, rms_env, control_rate, sample_rate);

    let mut passing_std = 800.0f64;
    let mut raised = false;
    let mut result: Vec<i64> = Vec::new();
    loop {
        let mut merge_flag = 0i64;
        result.clear();
        for &(t_ms, v) in &crest {
            if v >= passing_std && merge_flag <= 0 {
                merge_flag = 10;
                result.push(t_ms);
            } else {
                merge_flag -= 1;
            }
        }
        if result.len() >= 20 && !raised {
            raised = true;
            passing_std = 1100.0;
            continue;
        }
        break;
    }

    // 停顿补点：-50 dB / ≥4 s 静音的结束点，与已有切分点相距 ≤2 s 视为重复。
    let det = detect_silences(peak_env, control_rate, duration, -50.0, 4.0, 0.05);
    let mut extra: Vec<i64> = Vec::new();
    let dur_ms = (duration * 1000.0).round() as i64;
    for s in &det {
        let end_ms = (s.end * 1000.0).round() as i64;
        if end_ms >= dur_ms - 100 {
            continue;
        }
        let dup = result.iter().any(|&x| (x - end_ms).abs() <= 2000)
            || extra.iter().any(|&x| (x - end_ms).abs() <= 2000);
        if !dup {
            extra.push(end_ms);
        }
    }
    result.append(&mut extra);
    result.sort_unstable();
    result.dedup();
    result
        .into_iter()
        .map(|ms| ms as f64 / 1000.0)
        .filter(|&t| t >= 5.0)
        .collect()
}

// ============================================================
// 橙块（边界区）：切分点前后静音"夹住"合并 + 绿线双向对齐
// ============================================================

/// 找出被绿色切分点"夹住"的静音并合并成边界区（与 Slint `boundary_zones` 一致）。
fn boundary_zones(slices: &[f64], silences: &[Span], tol_before: f64, tol_after: f64) -> Vec<(f64, f64)> {
    let mut zones: Vec<(f64, f64)> = Vec::new();
    for &c in slices {
        let before = silences
            .iter()
            .filter(|s| s.start <= c && s.end >= c - tol_before)
            .min_by(|a, b| a.start.total_cmp(&b.start));
        let after = silences
            .iter()
            .filter(|s| s.start >= c && s.start <= c + tol_after)
            .max_by(|a, b| a.end.total_cmp(&b.end));
        match (before, after) {
            (Some(b), Some(a)) => zones.push((b.start, a.end)),
            (Some(b), None) => zones.push((b.start, c.max(b.end))),
            (None, Some(a)) => zones.push((c.min(a.start), a.end)),
            (None, None) => {}
        }
    }
    zones.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for z in zones {
        if let Some(last) = merged.last_mut() {
            if z.0 <= last.1 {
                last.1 = last.1.max(z.1);
                continue;
            }
        }
        merged.push(z);
    }
    merged
}

/// 每个切分点对应的提示音跨度（绿线区间）：静音夹住为主、频谱 crest（screst）峰兜底。
/// 频谱路径在 `pcm11k`（原始采样率/4）上做 STFT（4096 窗 / 1024 hop），与 Slint
/// `beep_spans` 的 pcm11k 行为一致（16 kHz 只有 8 kHz 带宽，screst 判定会失真）。
/// **开头 5 s 内开始的绿块不承认**。
pub fn beep_spans(
    pcm11k: &[f32],
    sr11k: u32,
    slices: &[f64],
    silences: &[Span],
    tol_before: f64,
    tol_after: f64,
    max_span: f64,
) -> Vec<(f64, f64)> {
    const SCREST_MIN: f64 = 150.0;
    let min_start = 5.0f64;
    let mut out = Vec::new();
    for &c in slices {
        // ① 静音夹住（精确路径）。
        let before = silences
            .iter()
            .filter(|s| s.start <= c && s.end >= c - tol_before)
            .max_by(|a, b| a.end.total_cmp(&b.end));
        let after = silences
            .iter()
            .filter(|s| s.start >= c && s.start <= c + tol_after)
            .min_by(|a, b| a.start.total_cmp(&b.start));
        if let (Some(b), Some(a)) = (before, after) {
            let (s, e) = (b.end, a.start);
            if e > s && e - s <= max_span && s >= min_start {
                out.push((s, e));
                continue;
            }
        }
        // ② 频谱 crest 峰兜底（pcm11k 上直算）。
        let sr = sr11k as f64;
        let win = 4096usize;
        let hop = 1024usize;
        let frame_s = win as f64 / sr;
        let (lo_t, hi_t) = (c - 2.0, c + 3.0);
        let i0 = (((lo_t - 0.4) * sr).max(0.0)) as usize;
        let i1 = (((hi_t + 0.4) * sr) as usize).min(pcm11k.len().saturating_sub(win));
        if i1 <= i0 {
            continue;
        }
        let fft = rustfft::FftPlanner::<f32>::new().plan_fft_forward(win);
        let hann: Vec<f32> = (0..win)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / win as f32).cos())
            .collect();
        let mut buf: Vec<rustfft::num_complex::Complex<f32>> =
            vec![rustfft::num_complex::Complex::new(0.0, 0.0); win];
        let mut peak: Option<(f64, f64)> = None;
        let mut frames: Vec<(f64, f64)> = Vec::new();
        let mut c0 = i0;
        while c0 < i1 {
            let s0 = c0 - win / 2;
            if s0 + win > pcm11k.len() {
                break;
            }
            for (i, b) in buf.iter_mut().enumerate() {
                let v = pcm11k[s0 + i];
                b.re = v * hann[i];
                b.im = 0.0;
            }
            fft.process(&mut buf);
            let half = win / 2;
            let mut tot = 0.0f64;
            let mut mags: Vec<f64> = Vec::with_capacity(half);
            for cc in buf[1..=half].iter() {
                let m = (cc.re * cc.re + cc.im * cc.im).sqrt() as f64;
                tot += m;
                mags.push(m);
            }
            let mean = tot / mags.len() as f64;
            let max = mags.iter().cloned().fold(0.0f64, f64::max);
            let screst = max / (mean + 1e-12);
            let tm = c0 as f64 / sr;
            if tm >= lo_t && tm <= hi_t {
                frames.push((tm, screst));
                if screst >= SCREST_MIN && peak.map_or(true, |(_, pv)| screst > pv) {
                    peak = Some((tm, screst));
                }
            }
            c0 += hop;
        }
        let Some((pt, pv)) = peak else { continue };
        let th = (pv * 0.3).max(SCREST_MIN);
        let mut s = pt;
        let mut e = pt + frame_s;
        for &(tm, v) in &frames {
            if v >= th {
                s = s.min(tm);
                e = e.max(tm + frame_s);
            }
        }
        s = s.max(lo_t);
        e = e.min(hi_t + frame_s);
        if e - s > max_span {
            let mid = pt + frame_s / 2.0;
            s = (mid - max_span / 2.0).max(lo_t);
            e = (mid + max_span / 2.0).min(hi_t + frame_s);
        }
        if e > s && s >= min_start {
            out.push((s, e));
        }
    }
    out
}

/// 单切分点、指定前后容差的橙块（供扩大容差补橙块用）。
fn zone_for_slice(c: f64, silences: &[Span], tol_before: f64, tol_after: f64) -> Option<(f64, f64)> {
    let before = silences
        .iter()
        .filter(|s| s.start <= c && s.end >= c - tol_before)
        .min_by(|a, b| a.start.total_cmp(&b.start));
    let after = silences
        .iter()
        .filter(|s| s.start >= c && s.start <= c + tol_after)
        .max_by(|a, b| a.end.total_cmp(&b.end));
    match (before, after) {
        (Some(b), Some(a)) => Some((b.start, a.end)),
        (Some(b), None) => Some((b.start, c.max(b.end))),
        (None, Some(a)) => Some((c.min(a.start), a.end)),
        (None, None) => None,
    }
}

/// 橙块 ↔ 绿线双向对齐（与 Slint `zones_with_beeps` 一致）：
/// 只保留有绿线覆盖的橙块；绿线无橙块时逐步扩大边界容差（tol_init → tol_max，step 步进）。
pub fn zones_with_beeps(
    slices: &[f64],
    silences: &[Span],
    beeps: &[(f64, f64)],
    tol_init: f64,
    tol_max: f64,
    step: f64,
) -> Vec<(f64, f64)> {
    let mut zones: Vec<(f64, f64)> = boundary_zones(slices, silences, tol_init, tol_init)
        .into_iter()
        .filter(|&(zs, ze)| beeps.iter().any(|&(bs, be)| bs < ze && be > zs))
        .collect();
    for &(bs, be) in beeps {
        if zones.iter().any(|&(zs, ze)| bs < ze && be > zs) {
            continue;
        }
        let mid = (bs + be) / 2.0;
        let Some(&c) = slices
            .iter()
            .min_by(|a, b| (*a - mid).abs().total_cmp(&(*b - mid).abs()))
        else {
            continue;
        };
        let mut tol = tol_init + step;
        while tol <= tol_max {
            if let Some(z) = zone_for_slice(c, silences, tol, tol) {
                zones.push(z);
                break;
            }
            tol += step;
        }
    }
    zones.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for z in zones {
        if let Some(last) = merged.last_mut() {
            if z.0 <= last.1 {
                last.1 = last.1.max(z.1);
                continue;
            }
        }
        merged.push(z);
    }
    merged
}

// ============================================================
// AB / C 分段
// ============================================================

pub const PART_GRAY: u8 = 1;
pub const PART_A: u8 = 2;
pub const PART_B: u8 = 3;
pub const PART_C: u8 = 4;

/// AB 音频指纹相似度门槛（Haitsma BER 匹配分数），达标才认可该对。
pub const AB_FP_THRESHOLD: f64 = 0.60;
/// AB 指纹匹配的对齐帧数下限（过滤短片段/静音主导的虚高对）。
pub const AB_MIN_VOTES: u32 = 400;

/// 计算两段音频的 Haitsma 指纹匹配结果（输入 5 kHz PCM）。
/// 返回 (score, votes)；无法提取返回 (0, 0)。
pub fn audio_fp_similarity(pcm5k: &[f32], a: (f64, f64), b: (f64, f64)) -> (f64, u32) {
    use audiofp::classical::{Haitsma, HaitsmaFingerprint};
    use audiofp::matching::{HaitsmaMatchConfig, HaitsmaMatcher, Matcher};
    use audiofp::{Fingerprinter, SampleRate};

    const RATE: f64 = 5000.0;
    let ai = (a.0 * RATE) as usize;
    let ae = ((a.1 * RATE) as usize).min(pcm5k.len());
    let bi = (b.0 * RATE) as usize;
    let be = ((b.1 * RATE) as usize).min(pcm5k.len());
    if ae <= ai || be <= bi {
        return (0.0, 0);
    }
    fn extract(samples: &[f32]) -> Option<HaitsmaFingerprint> {
        let mut h = Haitsma::default();
        h.extract(samples, SampleRate::HZ_5000).ok()
    }
    let (Some(fa), Some(fb)) = (extract(&pcm5k[ai..ae]), extract(&pcm5k[bi..be])) else {
        return (0.0, 0);
    };
    let matcher = HaitsmaMatcher::new(HaitsmaMatchConfig::default());
    let m = matcher.match_one(&fa, &fb);
    (m.score as f64, m.votes as u32)
}

/// 指纹确认判定：返回 `Some(sim)`（通过）或 `None`（不通过）。
fn fp_ok(pcm5k: &[f32], a: (f64, f64), b: (f64, f64)) -> Option<f64> {
    let (sim, votes) = audio_fp_similarity(pcm5k, a, b);
    if sim >= AB_FP_THRESHOLD && votes >= AB_MIN_VOTES {
        Some(sim)
    } else {
        None
    }
}

/// 取 `[ps, pe]` 内、时长 ≥ `min_len` 的静音（从低阈值全集 `fine` 过滤）。
fn sils_in(fine: &[Span], ps: f64, pe: f64, min_len: f64) -> Vec<Span> {
    fine.iter()
        .filter(|s| s.start >= ps && s.end <= pe && (s.end - s.start) >= min_len - 1e-9)
        .copied()
        .collect()
}

/// 自适应增大最小静音长度：目标该部分静音数 ∈ [2,3]，二分逼近。
fn grow_min(fine: &[Span], ps: f64, pe: f64, init: &[Span], min_len: f64) -> (Vec<Span>, f64) {
    let max_dur = init.iter().map(|s| s.end - s.start).fold(0.0f64, f64::max);
    let mut lo = min_len;
    let mut hi = max_dur + 0.5;
    while hi - lo > 0.05 {
        let mid = (lo + hi) / 2.0;
        let cand = sils_in(fine, ps, pe, mid);
        if (2..=3).contains(&cand.len()) {
            return (cand, mid);
        }
        if cand.len() >= 4 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (sils_in(fine, ps, pe, hi), hi)
}

/// 自适应减小最小静音长度：目标该部分静音数 ≥2（尽量 ∈[2,3]），二分逼近。
fn shrink_min(fine: &[Span], ps: f64, pe: f64, init: &[Span], min_len: f64) -> (Vec<Span>, f64) {
    let mut best = init.to_vec();
    let mut lo = 0.0;
    let mut hi = min_len;
    while hi - lo > 0.01 {
        let mid = (lo + hi) / 2.0;
        let cand = sils_in(fine, ps, pe, mid);
        if cand.len() >= 2 {
            if cand.len() <= 3 {
                return (cand, mid);
            }
            lo = mid;
            best = cand;
        } else {
            hi = mid;
        }
    }
    (best, lo)
}

/// 按边界区把整条音频切成"部分"，再对每个部分内部细分（与 Slint `segment_parts` 一致）：
/// - 部分内无静音 → 整段灰；恰 1 条静音（两侧差 ≤10%）→ 两侧 A/B；
///   2~3 条 → 最长连续重复模式（QGDEFDEF → 第一 DEF=A、第二 DEF=B）或相邻块扩大搜索；
/// - ≥4 条 → 增大最小静音长度；恰 1 条且两侧差 >10% → 减小最小静音长度，再走 AB；
/// - 所有候选 A/B 需通过 Haitsma 指纹（sim ≥ 0.60 且 votes ≥ 400）；
/// - 橙灰相间中连续 ≥2 个灰块 → 升级 C（淡紫），单一灰块保持灰；
/// - **第一个橙块之前的区间不承认任何 AB**。
/// 返回 (start, end, kind) 秒列表（升序）。
pub fn segment_parts(
    zones: &[(f64, f64)],
    silences: &[Span],
    silences_fine: &[Span],
    min_len_init: f64,
    duration: f64,
    pcm5k: &[f32],
) -> Vec<(f64, f64, u8)> {
    fn same_len(a: f64, b: f64) -> bool {
        let tol = 0.5f64.max(0.05 * a.abs().max(b.abs()));
        (a - b).abs() <= tol
    }

    let mut segs: Vec<(f64, f64)> = Vec::new();
    let mut cursor = 0.0f64;
    for &(zs, ze) in zones {
        if zs > cursor + 0.05 {
            segs.push((cursor, zs));
        }
        cursor = cursor.max(ze);
    }
    if cursor < duration - 0.05 {
        segs.push((cursor, duration));
    }

    let mut out: Vec<(f64, f64, u8)> = Vec::new();
    for (ps, pe) in segs {
        if ps <= 0.05 {
            continue;
        }
        let mut inner: Vec<Span> = silences
            .iter()
            .filter(|s| s.start >= ps && s.end <= pe)
            .copied()
            .collect();
        if inner.len() >= 4 {
            inner = grow_min(silences_fine, ps, pe, &inner, min_len_init).0;
        }
        if inner.len() == 1 {
            let s = inner[0];
            let la = s.start - ps;
            let lb = pe - s.end;
            if la > 0.05 && lb > 0.05 && (la - lb).abs() > 0.1 * la.max(lb) {
                inner = shrink_min(silences_fine, ps, pe, &inner, min_len_init).0;
            }
        }
        let mut blocks: Vec<(f64, f64, u8)> = Vec::new();
        match inner.len() {
            0 => blocks.push((ps, pe, PART_GRAY)),
            1 => {
                let s = inner[0];
                let has_a = s.start > ps + 0.05;
                let has_b = s.end < pe - 0.05;
                if has_a && has_b {
                    let la = s.start - ps;
                    let lb = pe - s.end;
                    if (la - lb).abs() <= 0.1 * la.max(lb) && fp_ok(pcm5k, (ps, s.start), (s.end, pe)).is_some() {
                        blocks.push((ps, s.start, PART_A));
                        blocks.push((s.end, pe, PART_B));
                    }
                }
            }
            _ => {
                let mut subs: Vec<(f64, f64)> = Vec::new();
                let mut c = ps;
                for s in &inner {
                    if s.start > c + 0.05 {
                        subs.push((c, s.start));
                    }
                    c = c.max(s.end);
                }
                if pe > c + 0.05 {
                    subs.push((c, pe));
                }
                let lens: Vec<f64> = subs.iter().map(|x| x.1 - x.0).collect();
                let n = lens.len();

                // ① 最长连续重复时长模式。
                let mut best: Option<(usize, usize)> = None;
                'outer: for block in (1..=n / 2).rev() {
                    for start in 0..=(n - 2 * block) {
                        let ok = (0..block).all(|k| same_len(lens[start + k], lens[start + block + k]));
                        if ok {
                            best = Some((start, block));
                            break 'outer;
                        }
                    }
                }
                let touch_left = ps > 0.05;
                let touch_right = pe < duration - 0.05;
                let repeat_touches_edge = best.map_or(false, |(st, bl)| {
                    (st == 0 && touch_left) || (st + 2 * bl - 1 == n - 1 && touch_right)
                });
                if let Some((st, bl)) = best.filter(|_| repeat_touches_edge) {
                    let a_span = (subs[st].0, subs[st + bl - 1].1);
                    let b_span = (subs[st + bl].0, subs[st + 2 * bl - 1].1);
                    if fp_ok(pcm5k, a_span, b_span).is_some() {
                        blocks.push((a_span.0, a_span.1, PART_A));
                        blocks.push((b_span.0, b_span.1, PART_B));
                    }
                } else {
                    // ② 扩大搜索：至少一边真贴橙，整体时长偏差 ≤ 10%。
                    let mut best2: Option<(usize, usize, usize, f64, f64)> = None;
                    for p in 0..n.saturating_sub(1) {
                        for ai in 0..=p {
                            for bi in (p + 1)..n {
                                let left_ok = ai == 0 && touch_left;
                                let right_ok = bi == n - 1 && touch_right;
                                if !(left_ok || right_ok) {
                                    continue;
                                }
                                let la: f64 = lens[ai..=p].iter().sum();
                                let lb: f64 = lens[p + 1..=bi].iter().sum();
                                let dev = (la - lb).abs() / la.max(lb);
                                if dev <= 0.10 {
                                    let total = la + lb;
                                    let better = match best2 {
                                        None => true,
                                        Some((_, _, _, bd, bt)) => {
                                            dev < bd - 1e-9 || ((dev - bd).abs() < 1e-9 && total > bt)
                                        }
                                    };
                                    if better {
                                        best2 = Some((ai, p, bi, dev, total));
                                    }
                                }
                            }
                        }
                    }
                    if let Some((ai, p, bi, _, _)) = best2 {
                        let a_span = (subs[ai].0, subs[p].1);
                        let b_span = (subs[p + 1].0, subs[bi].1);
                        if fp_ok(pcm5k, a_span, b_span).is_some() {
                            blocks.push((a_span.0, a_span.1, PART_A));
                            blocks.push((b_span.0, b_span.1, PART_B));
                        }
                    }
                }
            }
        }
        blocks.sort_by(|x, y| x.0.total_cmp(&y.0));
        out.extend(blocks);
    }
    out.sort_by(|x, y| x.0.total_cmp(&y.0));

    // 橙灰相间升级 C（淡紫）。
    let mut seq: Vec<(f64, f64, u8)> = Vec::new();
    for &(zs, ze) in zones {
        seq.push((zs, ze, 0));
    }
    for &(ps, pe, k) in &out {
        seq.push((ps, pe, k));
    }
    seq.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut to_c: Vec<(f64, f64)> = Vec::new();
    for i in 0..seq.len() {
        if seq[i].2 != PART_GRAY {
            continue;
        }
        let left_is_zone = i > 0 && seq[i - 1].2 == 0;
        let right_is_zone = i + 1 < seq.len() && seq[i + 1].2 == 0;
        if !(left_is_zone && right_is_zone) {
            continue;
        }
        let left2_gray = i >= 2 && seq[i - 2].2 == PART_GRAY;
        let right2_gray = i + 2 < seq.len() && seq[i + 2].2 == PART_GRAY;
        if left2_gray || right2_gray {
            to_c.push((seq[i].0, seq[i].1));
        }
    }
    for p in out.iter_mut() {
        if p.2 == PART_GRAY
            && to_c.iter().any(|&(s, e)| (s - p.0).abs() < 0.01 && (e - p.1).abs() < 0.01)
        {
            p.2 = PART_C;
        }
    }
    out
}

// ============================================================
// 顶层入口
// ============================================================

/// 检测参数（GUI 可调；其余沿用 Slint 侧已验证默认值）。
#[derive(Debug, Clone, Copy)]
pub struct DetectParams {
    /// 静音阈值（dB），默认 -30。
    pub threshold_db: f64,
    /// 最小静音时长（秒），默认 1.0。
    pub min_silence: f64,
    /// 最短声音时长（秒），默认 1.0。
    pub min_sound: f64,
    /// 边界容差（秒），默认 2.0。
    pub zone_tol: f64,
}

impl Default for DetectParams {
    fn default() -> Self {
        Self {
            threshold_db: -30.0,
            min_silence: 1.0,
            min_sound: 1.0,
            zone_tol: 2.0,
        }
    }
}

/// 一对认可的 AB 重复对。
#[derive(Debug, Clone, Copy)]
pub struct AbPair {
    pub a0: f64,
    pub a1: f64,
    pub b0: f64,
    pub b1: f64,
    /// Haitsma 指纹相似度（≥0.60 才认可）。
    pub sim: f32,
}

/// 一段认可的 C（不重复段，直接 ASR）。
#[derive(Debug, Clone, Copy)]
pub struct CSeg {
    pub start: f64,
    pub end: f64,
}

/// 检测输出：AB 对 + C 段 + 中间产物（绿线/橙块，供调试与 UI 复用）。
#[derive(Debug, Default)]
pub struct SliceDetectOut {
    pub ab: Vec<AbPair>,
    pub c: Vec<CSeg>,
    pub slices: Vec<f64>,
    pub zones: Vec<(f64, f64)>,
    pub parts: Vec<(f64, f64, u8)>,
}

/// 红蓝紫检测主流程：包络 → 静音 → 绿线 → 提示音 → 橙块 → A/B/C 分段。
pub fn detect(audio: &AudioData, p: &DetectParams) -> SliceDetectOut {
    let sr = audio.sample_rate as f64;
    let duration = audio.mono.len() as f64 / sr;
    if audio.mono.len() < 8192 || sr <= 0.0 {
        return SliceDetectOut::default();
    }

    let peak_env = &audio.peak_env;
    let rms_env = &audio.rms_env;
    let control_rate = audio.control_rate;
    let pcm5k = &audio.pcm5k;

    // 主静音（GUI 阈值）与低阈值全集（min_silence=0.3s，供部分内自适应调节）。
    let silences = detect_silences(
        peak_env,
        control_rate,
        duration,
        p.threshold_db,
        p.min_silence,
        p.min_sound,
    );
    let silences_fine = detect_silences(peak_env, control_rate, duration, p.threshold_db, 0.3, p.min_sound);

    let slices = analyse_slices(peak_env, rms_env, control_rate, audio.original_sample_rate, duration);
    let sr11k = audio.original_sample_rate / 4;
    let beeps = beep_spans(&audio.pcm11k, sr11k, &slices, &silences, p.zone_tol, p.zone_tol, 4.0);
    let zones = zones_with_beeps(&slices, &silences, &beeps, p.zone_tol, 60.0, 1.0);
    let parts = segment_parts(&zones, &silences, &silences_fine, p.min_silence, duration, pcm5k);

    // 提取 AB 对与 C 段（parts 中 A/B 成对相邻，A 之后最近的 B 即其配对）。
    let mut ab: Vec<AbPair> = Vec::new();
    let mut c: Vec<CSeg> = Vec::new();
    let mut i = 0usize;
    while i < parts.len() {
        let pt = parts[i];
        if pt.2 == PART_A {
            let mut j = i + 1;
            while j < parts.len() && parts[j].2 != PART_B {
                j += 1;
            }
            if j < parts.len() {
                let bp = parts[j];
                ab.push(AbPair { a0: pt.0, a1: pt.1, b0: bp.0, b1: bp.1, sim: 0.0 });
                i = j;
            }
        } else if pt.2 == PART_C {
            c.push(CSeg { start: pt.0, end: pt.1 });
        }
        i += 1;
    }
    // 补 sim：对每对重算指纹分数（fp_ok 内部已算，这里为输出直接重算一次）。
    for pair in ab.iter_mut() {
        pair.sim = audio_fp_similarity(pcm5k, (pair.a0, pair.a1), (pair.b0, pair.b1)).0 as f32;
    }

    SliceDetectOut { ab, c, slices, zones, parts }
}
