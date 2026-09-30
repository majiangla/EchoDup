//! Worker 子进程模式：GUI 主进程通过 `--worker <role> <payload.json>` 拉起自身，
//! 把耗时的解析 / ASR / 复制 / 导出放到独立进程，避免 UI 卡顿。
//!
//! 协议：stdout 逐行输出 JSON；最终输出 `{"type":"done",...}` 或 `{"type":"error","msg":...}`。
//! GUI 侧见 crate::proc。

use crate::config::Config;
use crate::core::{audio_io, slice_detect_new};
use crate::gui::{asr, clipboard, player};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize, Deserialize)]
pub struct AnalyzeReq {
    pub audio: String,
    pub cfg: Config,
    pub cache_wav: String,
    pub result_json: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SliceReq {
    pub gid: usize,
    pub start: f64,
    pub end: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AsrReq {
    pub cache_wav: String,
    pub slices: Vec<SliceReq>,
    pub result_json: String,
}

/// 复制 / 导出共用载荷
#[derive(Debug, Serialize, Deserialize)]
pub struct ClipReq {
    pub cache_wav: String,
    pub ranges: Vec<(f64, f64)>,
    pub gap_sec: f64,
    pub speed: f32,
    pub name: String,
    /// 复制：写出剪贴板临时 wav 的目录
    pub temp_dir: Option<String>,
    /// 导出：目标文件路径（None = 复制到剪贴板）
    pub out_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AsrSegOut {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

// ---------------- JSON 行协议 ----------------

fn emit(v: &serde_json::Value) {
    println!("{v}");
}

fn emit_done() {
    emit(&serde_json::json!({"type": "done"}));
}

fn emit_done_path(p: &std::path::Path) {
    emit(&serde_json::json!({"type": "done", "path": p.to_string_lossy()}));
}

fn emit_error(msg: impl AsRef<str>) {
    emit(&serde_json::json!({"type": "error", "msg": msg.as_ref()}));
}

fn load_req<T: for<'de> Deserialize<'de>>(path: &str) -> Result<T, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("读取载荷 {path} 失败: {e}"))?;
    serde_json::from_str(&s).map_err(|e| format!("解析载荷失败: {e}"))
}

fn stage(s: &str) {
    emit(&serde_json::json!({"type": "stage", "stage": s}));
}

// ---------------- 角色实现 ----------------

fn run_analyze(payload: &str) -> Result<(), String> {
    let req: AnalyzeReq = load_req(payload)?;
    let audio_path = PathBuf::from(&req.audio);
    let cache_wav = PathBuf::from(&req.cache_wav);
    let result_json = PathBuf::from(&req.result_json);

    stage("decode");
    let audio = audio_io::decode(&audio_path, req.cfg.target_sample_rate)
        .map_err(|e| format!("解码失败: {e:#}"))?;

    // 红蓝紫分段检测（取代原 Wang 指纹 detect + refine）：
    // AB = 重复对（ASR 只需转写 A），C = 不重复段（直接 ASR）。
    stage("detect");
    let p = slice_detect_new::DetectParams {
        threshold_db: req.cfg.threshold_db,
        min_silence: req.cfg.min_silence,
        min_sound: req.cfg.min_sound,
        zone_tol: req.cfg.zone_tolerance,
    };
    let out = slice_detect_new::detect(&audio, &p);

    // 写 16bit 单声道缓存 wav（GUI 读取用于波形/播放/复制/导出）
    clipboard::write_wav_file(&cache_wav, &audio.mono, audio.sample_rate, 1)
        .map_err(|e| format!("写缓存 wav 失败: {e:#}"))?;

    let total_duration = audio.mono.len() as f64 / audio.sample_rate as f64;
    let groups_json: Vec<serde_json::Value> = out
        .ab
        .iter()
        .enumerate()
        .map(|(i, g)| {
            serde_json::json!({
                "group_id": i + 1,
                "a0": g.a0, "a1": g.a1,
                "b0": g.b0, "b1": g.b1,
                "confidence": g.sim,
                "offset_sec": g.b0 - g.a0,
            })
        })
        .collect();
    let c_json: Vec<serde_json::Value> = out
        .c
        .iter()
        .enumerate()
        .map(|(i, c)| {
            serde_json::json!({
                "id": out.ab.len() + 1 + i,
                "start": c.start,
                "end": c.end,
            })
        })
        .collect();
    let out_json = serde_json::json!({
        "sample_rate": audio.sample_rate,
        "total_duration": total_duration,
        "groups": groups_json,
        "c_segments": c_json,
    });
    std::fs::write(&result_json, serde_json::to_string_pretty(&out_json).unwrap())
        .map_err(|e| format!("写结果 json 失败: {e}"))?;

    Ok(())
}

fn run_asr(payload: &str) -> Result<(), String> {
    let req: AsrReq = load_req(payload)?;
    let (mono, sr) = clipboard::read_wav_mono(&PathBuf::from(&req.cache_wav))
        .map_err(|e| format!("读缓存 wav 失败: {e:#}"))?;

    let mut results: Vec<serde_json::Value> = Vec::new();
    for s in &req.slices {
        let si = (s.start * sr as f64).round() as usize;
        let ei = (s.end * sr as f64).round() as usize;
        let slice = mono.get(si..ei).unwrap_or(&[]).to_vec();
        if slice.is_empty() {
            emit(&serde_json::json!({"type": "group_error", "gid": s.gid, "msg": "切片为空"}));
            continue;
        }
        match asr::transcribe(&slice) {
            Ok(segs) => {
                let segs_json: Vec<AsrSegOut> = segs
                    .iter()
                    .map(|x| AsrSegOut { start: x.start, end: x.end, text: x.text.clone() })
                    .collect();
                results.push(serde_json::json!({"gid": s.gid, "segs": segs_json}));
                emit(&serde_json::json!({
                    "type": "group",
                    "gid": s.gid,
                    "segs": segs_json,
                }));
            }
            Err(e) => {
                emit(&serde_json::json!({"type": "group_error", "gid": s.gid, "msg": e}));
            }
        }
    }

    let out = serde_json::json!({ "results": results });
    std::fs::write(&req.result_json, serde_json::to_string_pretty(&out).unwrap())
        .map_err(|e| format!("写 asr 结果 json 失败: {e}"))?;
    Ok(())
}

/// 从缓存 wav 按时间区间拼接样本（段间留 gap_sec 静音）
fn build_clip(req: &ClipReq) -> Result<(Vec<f32>, u32), String> {
    let (mono, sr) = clipboard::read_wav_mono(&PathBuf::from(&req.cache_wav))
        .map_err(|e| format!("读缓存 wav 失败: {e:#}"))?;
    let gap = (req.gap_sec * sr as f64) as usize;
    let mut out: Vec<f32> = Vec::new();
    for (k, &(s, e)) in req.ranges.iter().enumerate() {
        if k > 0 {
            out.extend(std::iter::repeat(0.0f32).take(gap));
        }
        let si = (s * sr as f64).round() as usize;
        let ei = (e * sr as f64).round() as usize;
        if let Some(sl) = mono.get(si..ei) {
            out.extend_from_slice(sl);
        }
    }
    if out.is_empty() {
        return Err("所选区间为空".into());
    }
    let out = if (req.speed - 1.0).abs() < 1e-3 || out.len() < sr as usize {
        out
    } else {
        player::do_stretch(&out, sr, req.speed)
    };
    Ok((out, sr))
}

fn run_clip(payload: &str, is_export: bool) -> Result<Option<String>, String> {
    let req: ClipReq = load_req(payload)?;
    let (samples, sr) = build_clip(&req)?;

    if is_export {
        let Some(out_path) = req.out_path.as_ref() else {
            return Err("缺少 out_path".into());
        };
        clipboard::write_wav_file(PathBuf::from(out_path).as_path(), &samples, sr, 1)
            .map_err(|e| format!("写 wav 失败: {e:#}"))?;
        Ok(Some(out_path.clone()))
    } else {
        let p = clipboard::copy_wav(&samples, sr, 1, &req.name)
            .map_err(|e| format!("复制失败: {e:#}"))?;
        Ok(Some(p.to_string_lossy().into_owned()))
    }
}

pub fn run(args: &[String]) -> ExitCode {
    let Some(role) = args.first() else {
        eprintln!("worker: missing role");
        return ExitCode::from(2);
    };
    let payload = args.get(1).map(String::as_str).unwrap_or("");
    let r: Result<Option<String>, String> = match role.as_str() {
        "analyze" => run_analyze(payload).map(|_| None),
        "asr" => run_asr(payload).map(|_| None),
        "copy" => run_clip(payload, false),
        "export" => run_clip(payload, true),
        other => {
            eprintln!("worker: unknown role {other}");
            return ExitCode::from(2);
        }
    };
    match r {
        Ok(path) => {
            match path {
                Some(p) => emit_done_path(std::path::Path::new(&p)),
                None => emit_done(),
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            emit_error(&e);
            ExitCode::from(3)
        }
    }
}
