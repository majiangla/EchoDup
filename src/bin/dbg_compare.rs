//! 临时调试：打印红蓝紫管线中间产物（绿线/橙块/parts/AB）。构建：cargo run --release --bin dbg_compare -- <file>

#[path = "../core/mod.rs"]
mod core;

use core::{audio_io, slice_detect_new};

fn fmt_span(s: f64, e: f64) -> String {
    format!("{:.2}-{:.2}", s, e)
}

fn main() {
    let mut files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        files = vec![
            r"C:\Users\Desktop\Documents\Code\EchoDup\sample\英语听力模拟试题 12 浙江嘉兴高三基测  音频.mp3".to_string(),
            r"C:\Users\Desktop\Documents\Code\EchoDup\sample\2027届高三年级期初学情检测（二）听力音频.mp3".to_string(),
        ];
    }
    for f in &files {
        let audio = audio_io::decode(std::path::Path::new(f), 16000).unwrap();
        let p = slice_detect_new::DetectParams::default();
        // 打印主静音全集（-30dB/1s），包络用 decode 内置的原始采样率包络
        let peak_env = &audio.peak_env;
        let sils = slice_detect_new::detect_silences(peak_env, 100.0, audio.mono.len() as f64 / audio.sample_rate as f64, p.threshold_db, p.min_silence, p.min_sound);
        println!("  det.silences(-30/1s)={}", sils.len());
        for s in &sils {
            print!("  [{:.2}-{:.2}]", s.start, s.end);
        }
        println!();
        let fine = slice_detect_new::detect_silences(peak_env, 100.0, audio.mono.len() as f64 / audio.sample_rate as f64, p.threshold_db, 0.3, p.min_sound);
        println!("  fine.silences(-30/0.3s)={}", fine.len());
        let out = slice_detect_new::detect(&audio, &p);
        println!("==== {}", std::path::Path::new(f).file_name().unwrap().to_string_lossy());
        println!(
            "  dur {:.1}s  slices(绿线)={}",
            audio.mono.len() as f64 / audio.sample_rate as f64,
            out.slices.len()
        );
        for (i, s) in out.slices.iter().enumerate() {
            print!("  [{}{}]", i, fmt_span(*s, *s));
        }
        println!();
        println!("  zones(橙块)={}", out.zones.len());
        for z in &out.zones {
            print!("  {}", fmt_span(z.0, z.1));
        }
        println!();
        println!("  parts={}", out.parts.len());
        for pr in &out.parts {
            let k = match pr.2 {
                slice_detect_new::PART_A => "A",
                slice_detect_new::PART_B => "B",
                slice_detect_new::PART_C => "C",
                _ => "灰",
            };
            print!("  {}:{} ", k, fmt_span(pr.0, pr.1));
        }
        println!();
        println!("  AB={} C={}", out.ab.len(), out.c.len());
        for a in &out.ab {
            println!("    A {}  B {}  sim={:.2}", fmt_span(a.a0, a.a1), fmt_span(a.b0, a.b1), a.sim);
        }
        for c in &out.c {
            println!("    C {} ", fmt_span(c.start, c.end));
        }
        // 手工检查 Slint 认可的 4 对（EchoDup 缺失），用 EchoDup 的 pcm5k 算指纹分数
        let pcm5k = &audio.pcm5k;
        let pairs = [
            ((98.24, 110.76), (112.86, 121.46)),
            ((219.34, 234.16), (236.43, 247.30)),
            ((260.15, 275.39), (277.62, 288.81)),
            ((937.43, 999.46), (1001.48, 1061.57)),
        ];
        for (x, y) in pairs {
            let (sim, votes) = slice_detect_new::audio_fp_similarity(pcm5k, x, y);
            println!("    [check] A {} B {} sim={:.2} votes={}", fmt_span(x.0, x.1), fmt_span(y.0, y.1), sim, votes);
        }
        // [dbg] 手工复算 segment_parts 用的静音集合：对每个橙块间部分打印内静音数
        let dur = audio.mono.len() as f64 / audio.sample_rate as f64;
        let sils2 = slice_detect_new::detect_silences(peak_env, 100.0, dur, p.threshold_db, p.min_silence, p.min_sound);
        let fine2 = slice_detect_new::detect_silences(peak_env, 100.0, dur, p.threshold_db, 0.3, p.min_sound);
        let parts2 = slice_detect_new::segment_parts(&out.zones, &sils2, &fine2, p.min_silence, dur, pcm5k);
        println!("  [dbg] 手工 segment_parts 复算：{} 块", parts2.len());
        for pr in &parts2 {
            let k = match pr.2 {
                slice_detect_new::PART_A => "A",
                slice_detect_new::PART_B => "B",
                slice_detect_new::PART_C => "C",
                _ => "灰",
            };
            print!("  {}:{} ", k, fmt_span(pr.0, pr.1));
        }
        println!();
        let mut cursor = 0.0f64;
        for &(zs, ze) in &out.zones {
            if zs > cursor + 0.05 {
                let n = sils2.iter().filter(|s| s.start >= cursor && s.end <= zs).count();
                println!("  [dbg] 部分 [{:.2},{:.2}] 内主静音={} 细静音={}", cursor, zs, n,
                    fine2.iter().filter(|s| s.start >= cursor && s.end <= zs && (s.end - s.start) >= p.min_silence - 1e-9).count());
            }
            cursor = cursor.max(ze);
        }
        if cursor < dur - 0.05 {
            let n = sils2.iter().filter(|s| s.start >= cursor && s.end <= dur).count();
            println!("  [dbg] 部分 [{:.2},{:.2}] 内主静音={} 细静音={}", cursor, dur, n,
                fine2.iter().filter(|s| s.start >= cursor && s.end <= dur && (s.end - s.start) >= p.min_silence - 1e-9).count());
        }
        // [dbg] 复现 grow_min 二分轨迹（部分 (26.5,336.8)）
        {
            let ps = 26.51f64;
            let pe = 336.82f64;
            let init: Vec<slice_detect_new::Span> = sils2
                .iter()
                .filter(|s| s.start >= ps && s.end <= pe)
                .copied()
                .collect();
            let max_dur = init.iter().map(|s| s.end - s.start).fold(0.0f64, f64::max);
            println!("  [dbg] grow 复现: init={} max_dur={:.3}", init.len(), max_dur);
            let mut lo = p.min_silence;
            let mut hi = max_dur + 0.5;
            let mut steps = 0;
            while hi - lo > 0.05 && steps < 30 {
                let mid = (lo + hi) / 2.0;
                let cand: Vec<slice_detect_new::Span> = fine2
                    .iter()
                    .filter(|s| s.start >= ps && s.end <= pe && (s.end - s.start) >= mid - 1e-9)
                    .copied()
                    .collect();
                println!(
                    "    mid={:.3} cand={} ({}..{})",
                    mid,
                    cand.len(),
                    cand.first().map(|s| format!("{:.2}-{:.2}", s.start, s.end)).unwrap_or_default(),
                    cand.last().map(|s| format!("{:.2}-{:.2}", s.start, s.end)).unwrap_or_default()
                );
                if (2..=3).contains(&cand.len()) {
                    break;
                }
                if cand.len() >= 4 {
                    lo = mid;
                } else {
                    hi = mid;
                }
                steps += 1;
            }
        }
    }
}
