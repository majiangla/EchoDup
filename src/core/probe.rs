//! --probe 探查模式：只看指纹匹配分布，不做最终选择。
//! 输出：全部匹配对 (t1, t2, offset)、offset 直方图、按 offset 聚类的连续候选段。

use std::collections::HashMap;

/// 匹配对（帧号）
#[derive(Clone, Copy)]
pub struct MatchPair {
    pub t1: u32,
    pub t2: u32,
}

/// offset 直方图 bin（帧）
#[derive(Clone)]
pub struct HistBin {
    pub offset_frame: i64, // bin 中心的帧偏移
    pub count: usize,
}

/// 候选连续段（秒）
pub struct ProbeSegment {
    pub offset_frame: i64,
    pub t1_start: f64,
    pub t1_end: f64,
    pub t2_start: f64,
    pub t2_end: f64,
    pub pairs: usize,
}

/// 探查参数
pub struct ProbeParams {
    /// offset 直方图 bin 宽度（帧，1 帧 = 1/62.5 s ≈ 16ms）
    pub bin_frames: i64,
    /// 最小 offset（帧）：排除 t1≈t2 的自配对
    pub min_offset_frame: i64,
    /// 最大 offset（帧）：0 = 不限
    pub max_offset_frame: i64,
    /// 同一 offset 簇内，匹配对按 t1 聚成连续段的间隔容差（帧）
    pub seg_gap_frame: i64,
    /// offset 簇匹配窗口（帧）：簇中心 ± 窗口
    pub cluster_window_frame: i64,
    /// 峰值判定：bin 计数须 > 全局平均的倍数
    pub peak_ratio: f64,
}

impl Default for ProbeParams {
    fn default() -> Self {
        ProbeParams {
            bin_frames: 4,        // ~64ms
            min_offset_frame: 60, // ~1s
            max_offset_frame: 0,
            seg_gap_frame: 125,   // ~2s
            cluster_window_frame: 8, // ~128ms
            peak_ratio: 1.5,      // 朗读重复段匹配密度可低至 ~15 对/s，2.0 会漏掉弱重复
        }
    }
}

/// 从指纹哈希列表生成全部匹配对（同 hash 跨时间两两配对；排除 t1==t2 与过近/过远 offset）。
pub fn match_pairs(hashes: &[(u32, u32)], p: &ProbeParams) -> Vec<MatchPair> {
    let mut index: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(t, h) in hashes {
        index.entry(h).or_default().push(t);
    }
    let mut pairs: Vec<MatchPair> = Vec::new();
    for ts in index.values() {
        if ts.len() < 2 {
            continue;
        }
        for i in 0..ts.len() {
            for j in (i + 1)..ts.len() {
                let (a, b) = (ts[i], ts[j]);
                let off = b as i64 - a as i64;
                if off < p.min_offset_frame {
                    continue;
                }
                if p.max_offset_frame > 0 && off > p.max_offset_frame {
                    continue;
                }
                pairs.push(MatchPair { t1: a, t2: b });
            }
        }
    }
    // 排序（t1 升序，t2 升序）便于聚类
    pairs.sort_by(|x, y| x.t1.cmp(&y.t1).then(x.t2.cmp(&y.t2)));
    pairs
}

/// offset 直方图（bin 宽度参数化；返回按计数降序的峰值候选 + 全量 bins）
pub fn offset_histogram(pairs: &[MatchPair], p: &ProbeParams) -> (Vec<HistBin>, Vec<HistBin>) {
    let mut raw: HashMap<i64, usize> = HashMap::new();
    for pr in pairs {
        let off = pr.t2 as i64 - pr.t1 as i64;
        let bin = (off as f64 / p.bin_frames as f64).round() as i64;
        *raw.entry(bin).or_default() += 1;
    }
    let mut bins: Vec<HistBin> = raw
        .into_iter()
        .map(|(b, c)| HistBin {
            offset_frame: b * p.bin_frames,
            count: c,
        })
        .collect();
    bins.sort_by(|x, y| x.offset_frame.cmp(&y.offset_frame));
    let avg = bins.iter().map(|b| b.count).sum::<usize>() as f64 / bins.len().max(1) as f64;
    let mut peaks: Vec<HistBin> = bins
        .iter()
        .filter(|b| b.count as f64 > avg * p.peak_ratio)
        .cloned()
        .collect();
    peaks.sort_by(|x, y| y.count.cmp(&x.count));
    (peaks, bins)
}

/// 对每个 offset 峰值簇，把匹配对聚成连续候选段。
pub fn cluster_segments(
    pairs: &[MatchPair],
    peaks: &[HistBin],
    p: &ProbeParams,
) -> Vec<ProbeSegment> {
    let mut segs: Vec<ProbeSegment> = Vec::new();
    // 簇：相邻峰值（offset 差 <= cluster_window_frame*2）合并
    let mut clusters: Vec<(i64, i64)> = Vec::new(); // (center, window)
    for pk in peaks {
        let c = pk.offset_frame;
        let w = p.cluster_window_frame.max(p.bin_frames * 2);
        if let Some(last) = clusters.last_mut() {
            if (c - last.0).abs() <= w * 2 {
                // 合并：中心取两峰中点，窗口扩展覆盖两峰
                // （若直接用后一个 peak 覆盖 center，会把前一个峰的对甩出窗口）
                let mid = (last.0 + c) / 2;
                let half = (c - last.0).abs() / 2;
                last.0 = mid;
                last.1 = last.1.max(w) + half;
                continue;
            }
        }
        clusters.push((c, w));
    }
    for (center, w) in clusters {
        // 收集该簇窗口内的匹配对
        let mut cs: Vec<(u32, u32)> = pairs
            .iter()
            .filter(|pr| {
                let off = pr.t2 as i64 - pr.t1 as i64;
                (off - center).abs() <= w
            })
            .map(|pr| (pr.t1, pr.t2))
            .collect();
        if cs.len() < 2 {
            continue;
        }
        cs.sort_by(|x, y| x.0.cmp(&y.0).then(x.1.cmp(&y.1)));
        // 按 t1 贪心聚连续段
        let fps = 62.5f64;
        let mut t1_s = cs[0].0 as i64;
        let mut t1_e = cs[0].0 as i64;
        let mut t2_s = cs[0].1 as i64;
        let mut t2_e = cs[0].1 as i64;
        let mut count = 1usize;
        let mut flush = |t1_s: &mut i64, t1_e: &mut i64, t2_s: &mut i64, t2_e: &mut i64, count: &mut usize, segs: &mut Vec<ProbeSegment>| {
            if *count >= 3 {
                segs.push(ProbeSegment {
                    offset_frame: center,
                    t1_start: *t1_s as f64 / fps,
                    t1_end: *t1_e as f64 / fps,
                    t2_start: *t2_s as f64 / fps,
                    t2_end: *t2_e as f64 / fps,
                    pairs: *count,
                });
            }
            *count = 0;
        };
        for &(a, b) in cs.iter().skip(1) {
            if a as i64 - t1_e > p.seg_gap_frame {
                flush(&mut t1_s, &mut t1_e, &mut t2_s, &mut t2_e, &mut count, &mut segs);
                t1_s = a as i64;
                t1_e = a as i64;
                t2_s = b as i64;
                t2_e = b as i64;
                count = 1;
            } else {
                t1_e = a as i64;
                t2_s = t2_s.min(b as i64);
                t2_e = t2_e.max(b as i64);
                count += 1;
            }
        }
        flush(&mut t1_s, &mut t1_e, &mut t2_s, &mut t2_e, &mut count, &mut segs);
    }
    segs.sort_by(|x, y| {
        let dx = (x.t1_end - x.t1_start) + (x.t2_end - x.t2_start);
        let dy = (y.t1_end - y.t1_start) + (y.t2_end - y.t2_start);
        dy.partial_cmp(&dx).unwrap()
    });
    segs
}
