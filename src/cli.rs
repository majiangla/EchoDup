use crate::config::Config;
use crate::core::{audio_io, fingerprint, output, probe, slice_detect_new};
use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(name = "echo_dup", version, about = "Audio duplicate segment detector")]
struct Args {
    /// Input audio file
    input: PathBuf,

    /// Output directory
    #[arg(short, long, default_value = ".")]
    out: PathBuf,

    /// 静音阈值（dB），默认 -30
    #[arg(long, default_value_t = -30.0)]
    threshold_db: f64,

    /// 最小静音时长（秒），默认 1.0
    #[arg(long, default_value_t = 1.0)]
    min_silence: f64,

    /// 最短声音时长（秒），默认 1.0
    #[arg(long, default_value_t = 1.0)]
    min_sound: f64,

    /// 边界容差（"夹住"判定距离，秒），默认 2.0
    #[arg(long, default_value_t = 2.0)]
    zone_tolerance: f64,

    /// Print JSON summary to stdout
    #[arg(long, default_value_t = false)]
    json_stdout: bool,

    /// 只提取音频指纹（audiofp/Wang），不跑重复检测；打印统计并保存 <stem>_fingerprint.txt
    #[arg(long, default_value_t = false)]
    fingerprint: bool,

    /// 探查模式：输出指纹全部匹配对、offset 直方图与候选连续段（不做最终选择）
    #[arg(long, default_value_t = false)]
    probe: bool,

    /// 探查：offset 直方图 bin 宽度（帧，1 帧≈16ms），默认 4
    #[arg(long, default_value_t = 4)]
    probe_bin: i64,

    /// 探查：最小 offset（秒），排除自配对，默认 1.0
    #[arg(long, default_value_t = 1.0)]
    probe_min_offset: f64,

    /// 探查：最大 offset（秒），0=不限
    #[arg(long, default_value_t = 0.0)]
    probe_max_offset: f64,
}

pub fn run(raw: &[String]) -> ExitCode {
    let filtered: Vec<String> = raw
        .iter()
        .filter(|a| a.as_str() != "--cli")
        .cloned()
        .collect();

    let args = match Args::try_parse_from(&filtered) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    match run_inner(&args) {
        Ok(found) => {
            if found {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(3)
        }
    }
}

fn run_inner(args: &Args) -> Result<bool> {
    if args.probe {
        return run_probe(args);
    }
    if args.fingerprint {
        return run_fingerprint(args);
    }
    let t0 = Instant::now();

    let cfg = Config {
        threshold_db: args.threshold_db,
        min_silence: args.min_silence,
        min_sound: args.min_sound,
        zone_tolerance: args.zone_tolerance,
        ..Config::default()
    };

    let audio = audio_io::decode(&args.input, cfg.target_sample_rate)?;
    let total_duration = audio.mono.len() as f64 / audio.sample_rate as f64;

    // 红蓝紫分段检测（取代原 Wang detect + refine）
    let p = slice_detect_new::DetectParams {
        threshold_db: cfg.threshold_db,
        min_silence: cfg.min_silence,
        min_sound: cfg.min_sound,
        zone_tol: cfg.zone_tolerance,
    };
    let out = slice_detect_new::detect(&audio, &p);

    let groups: Vec<crate::core::detect::RepeatGroup> = out
        .ab
        .iter()
        .enumerate()
        .map(|(i, g)| crate::core::detect::RepeatGroup {
            group_id: i + 1,
            a: (g.a0, g.a1),
            b: (g.b0, g.b1),
            confidence: g.sim,
            match_type: "ab-pair",
            offset_sec: g.b0 - g.a0,
        })
        .collect();

    let src = args
        .input
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("input");
    let stem = args
        .input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");

    let out_doc = output::build(&groups, src, total_duration);

    std::fs::create_dir_all(&args.out)?;
    let json_path = args.out.join(format!("{stem}_repeats.json"));
    let csv_path = args.out.join(format!("{stem}_repeats.csv"));
    let labels_path = args.out.join(format!("{stem}_audacity_labels.txt"));

    output::write_json(&out_doc, &json_path)?;
    output::write_csv(&out_doc, &csv_path)?;
    output::write_labels(&out_doc, &labels_path)?;

    println!(
        "Found {} repeat group(s) + {} independent segment(s) in {:.2?}",
        groups.len(),
        out.c.len(),
        t0.elapsed()
    );
    for g in &groups {
        println!(
            "  G{}: A {:.1}–{:.1}  |  B {:.1}–{:.1}  |  conf {:.2}",
            g.group_id, g.a.0, g.a.1, g.b.0, g.b.1, g.confidence
        );
    }
    for (i, c) in out.c.iter().enumerate() {
        println!("  C{}: {:.1}–{:.1}", i + 1, c.start, c.end);
    }

    if args.json_stdout {
        println!("{}", serde_json::to_string_pretty(&out_doc)?);
    }

    Ok(!groups.is_empty() || !out.c.is_empty())
}

/// --probe：只看指纹匹配分布，不做最终选择。
fn run_probe(args: &Args) -> Result<bool> {
    use std::io::Write;
    let t0 = Instant::now();

    let audio = audio_io::decode(&args.input, 16_000)?;
    let fp = fingerprint::extract_fingerprint(&audio.mono, audio.sample_rate)?;

    let mut pp = probe::ProbeParams::default();
    pp.bin_frames = args.probe_bin;
    pp.min_offset_frame = (args.probe_min_offset * fp.frames_per_sec as f64) as i64;
    pp.max_offset_frame = if args.probe_max_offset > 0.0 {
        (args.probe_max_offset * fp.frames_per_sec as f64) as i64
    } else {
        0
    };

    let fps = fp.frames_per_sec as f64;
    println!("[probe] algorithm={} hashes={} fps={:.1} params={{bin={}min_off={:.1}s max_off={}}}",
        fp.algorithm, fp.hashes.len(), fp.frames_per_sec, pp.bin_frames,
        args.probe_min_offset,
        if pp.max_offset_frame > 0 { format!("{:.1}s", pp.max_offset_frame as f64 / fps) } else { "不限".to_string() });

    let pairs = probe::match_pairs(&fp.hashes, &pp);
    println!("[probe] 匹配对总数 = {}（同一 (t1,t2) 被多个哈希命中时重复计数）", pairs.len());
    // 去重统计（唯一 (t1,t2)）
    let uniq: std::collections::HashSet<(u32, u32)> = pairs.iter().map(|p| (p.t1, p.t2)).collect();
    println!("[probe] 唯一匹配对 = {}", uniq.len());

    let (peaks, bins) = probe::offset_histogram(&pairs, &pp);
    println!("[probe] offset 直方图 top 10 峰值（offset_sec  count）：");
    for pk in peaks.iter().take(10) {
        println!("[probe]   {:8.2}s  {:>8}", pk.offset_frame as f64 / fps, pk.count);
    }
    let total_bins: usize = bins.iter().map(|b| b.count).sum();
    println!("[probe] 直方图 bins={} 总计数={}", bins.len(), total_bins);

    let segs = probe::cluster_segments(&pairs, &peaks, &pp);
    println!("[probe] 候选连续段（按覆盖长度排序，前 20，不做最终选择）：");
    for (i, s) in segs.iter().take(20).enumerate() {
        let len_a = s.t1_end - s.t1_start;
        let len_b = s.t2_end - s.t2_start;
        println!(
            "[probe]   #{:<2} t1 {:7.1}-{:7.1} ({:.1}s)  t2 {:7.1}-{:7.1} ({:.1}s)  offset {:.1}s  pairs={}",
            i + 1, s.t1_start, s.t1_end, len_a, s.t2_start, s.t2_end, len_b,
            s.offset_frame as f64 / fps, s.pairs
        );
    }

    // 保存
    let stem = args.input.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    let out_dir = if args.out.as_os_str().is_empty() { std::path::PathBuf::from(".") } else { args.out.clone() };
    std::fs::create_dir_all(&out_dir)?;
    let pairs_path = out_dir.join(format!("{stem}_probe_pairs.txt"));
    let seg_path = out_dir.join(format!("{stem}_probe_segments.txt"));
    let hist_path = out_dir.join(format!("{stem}_probe_hist.txt"));

    let mut f = std::fs::File::create(&pairs_path)?;
    writeln!(f, "# t1_sec t2_sec offset_sec (帧号转秒：/ {:.1})", fps)?;
    for pr in &pairs {
        writeln!(f, "{:.3} {:.3} {:.3}", pr.t1 as f64 / fps, pr.t2 as f64 / fps, (pr.t2 as i64 - pr.t1 as i64) as f64 / fps)?;
    }
    let mut f = std::fs::File::create(&hist_path)?;
    for b in &bins {
        writeln!(f, "{:.3} {}", b.offset_frame as f64 / fps, b.count)?;
    }
    let mut f = std::fs::File::create(&seg_path)?;
    writeln!(f, "# t1_start t1_end t2_start t2_end offset_sec pairs")?;
    for s in &segs {
        writeln!(f, "{:.3} {:.3} {:.3} {:.3} {:.3} {}", s.t1_start, s.t1_end, s.t2_start, s.t2_end, s.offset_frame as f64 / fps, s.pairs)?;
    }
    println!("[probe] 已保存 -> {} / {} / {}", pairs_path.display(), hist_path.display(), seg_path.display());
    println!("[probe] 探查耗时 {:.2?}", t0.elapsed());

    Ok(true)
}

/// --fingerprint：只提取指纹，输出统计并保存指纹文本文件。
fn run_fingerprint(args: &Args) -> Result<bool> {
    let t0 = Instant::now();

    let audio = audio_io::decode(&args.input, 16_000)?;
    let src_dur = audio.mono.len() as f64 / audio.sample_rate as f64;
    let fp = fingerprint::extract_fingerprint(&audio.mono, audio.sample_rate)?;

    let stem = args
        .input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let out_dir = if args.out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        args.out.clone()
    };
    std::fs::create_dir_all(&out_dir)?;
    let fp_path = out_dir.join(format!("{stem}_fingerprint.txt"));

    // 统计
    let n = fp.hashes.len();
    let per_sec = if src_dur > 0.0 { n as f64 / src_dur } else { 0.0 };
    println!("[fingerprint] algorithm={}  file={}", fp.algorithm, args.input.display());
    println!(
        "[fingerprint] {} hashes, {:.1} fps, 覆盖 {:.1}s（源音频 {:.1}s）",
        n, fp.frames_per_sec, fp.duration_sec, src_dur
    );
    println!("[fingerprint] 密度 {:.0} hashes/s, 提取耗时 {:.2?}", per_sec, t0.elapsed());

    // 前 10 个示例
    println!("[fingerprint] 前 10 个哈希 (t_sec, t_anchor, hash):");
    for (t, h) in fp.hashes.iter().take(10) {
        println!("[fingerprint]   {:8.2}s  {:>10}  {:08x}", *t as f64 / fp.frames_per_sec as f64, t, h);
    }

    // 按 10 秒分段密度（让用户看指纹随时间的分布）
    let bin = 10.0;
    let bins = (src_dur / bin).ceil() as usize + 1;
    let mut hist = vec![0usize; bins];
    for (t, _) in &fp.hashes {
        let sec = *t as f64 / fp.frames_per_sec as f64;
        let idx = (sec / bin) as usize;
        if idx < bins {
            hist[idx] += 1;
        }
    }
    println!("[fingerprint] 每 {}s 段哈希数：", bin as u32);
    for (i, c) in hist.iter().enumerate() {
        let bar = "#".repeat((*c as f64 / hist.iter().copied().max().unwrap_or(1) as f64 * 40.0).round() as usize);
        println!("[fingerprint]   {:6.0}-{:6.0}s {:>7} {}", i as f64 * bin, (i as f64 + 1.0) * bin, c, bar);
    }

    // 保存
    use std::io::Write;
    let mut f = std::fs::File::create(&fp_path)?;
    writeln!(f, "# EchoDup audio fingerprint ({}), fps={}", fp.algorithm, fp.frames_per_sec)?;
    writeln!(f, "# t_anchor 为帧号，绝对时间 = t_anchor / fps 秒")?;
    writeln!(f, "# <t_sec> <t_anchor> <hash_hex>")?;
    for (t, h) in &fp.hashes {
        writeln!(f, "{:.3} {} {:08x}", *t as f64 / fp.frames_per_sec as f64, t, h)?;
    }
    println!("[fingerprint] 已保存 -> {}", fp_path.display());

    Ok(true)
}
