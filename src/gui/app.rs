//! EchoDup Slint 主应用。
//!
//! 架构：耗时操作（解析 / ASR / 复制 / 导出）全部在独立子进程中执行
//! （见 crate::worker 与 crate::proc），UI 线程只做：
//!   - 定时器轮询子进程 stdout 的 JSON 行（30ms）
//!   - 更新 Slint 属性与模型
//!   - 播放（cpal，低延迟必须留在主进程）与 LCS 对齐（量小，走线程）

use crate::config::Config;
use crate::core::{audio_io, detect, output};
use crate::gui::{align, asr, clipboard, drop, fonts, player};
use crate::proc::{ProcMsg, Worker};
use slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer,
    TimerMode, VecModel,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver};

slint::include_modules!();

/// 当前正在分析的文件缓存（worker 产出，GUI 读取）
struct AnalysisFiles {
    cache_wav: PathBuf,
    result_json: PathBuf,
    payload: PathBuf,
}

pub struct App {
    ui: AppWindow,
    cfg: Config,
    file: Option<PathBuf>,
    audio: Option<audio_io::AudioData>,
    groups: Vec<detect::RepeatGroup>,
    /// C 段（不重复段，直接 ASR）：(gid, start, end)。
    c_items: Vec<(usize, f64, f64)>,
    /// 出现顺序编号：gid -> 1..N（AB 组与 C 段按起点时间统一排序，与原文 Text 标号一致）。
    display_nums: HashMap<usize, usize>,
    total_duration: f64,
    whole_loaded: bool,
    slice_range: Option<(f64, f64)>,
    selected: Option<(usize, u8)>,
    asr_results: HashMap<usize, Vec<asr::AsrSegment>>,
    raw_asr: HashMap<usize, Vec<asr::AsrSegment>>,
    /// B 段 ASR（与 A 共用 gid，单独存储）
    asr_results_b: HashMap<usize, Vec<asr::AsrSegment>>,
    raw_asr_b: HashMap<usize, Vec<asr::AsrSegment>>,
    /// gid -> 当前组头选中的段（0=A, 1=B）
    tabs: HashMap<usize, u8>,
    /// 转写阶段：0 = C+A（按时间序），1 = B，2 = 完成
    asr_phase: u8,
    /// 勾选集：(gid, side, sid)；side 0=A 1=B，sid=usize::MAX 表示整段（组头）
    checked: HashSet<(usize, u8, usize)>,
    transcript: String,
    player: player::Player,
    speed: f32,
    applied_speed: f32,
    dragging: bool,
    /// 进度条跳转标记：跳转完成后下一帧按命中句子自动切换组头 tab（A/B）
    just_seeked: bool,
    /// ASR 内容分行模式：0=按句号 1=按换行 2=按 M:/W:
    split_mode: u8,
    rows: Vec<RowData>,
    /// 波形基础峰值（1600 列粗缓存），resize 时池化到实际像素列数
    peaks_base: Vec<f32>,
    /// 当前波形列数（避免重复重算）
    peaks_len: usize,
    /// 最近播放/聚焦的 text（gid）；Ctrl+C/X/A/S 与 ↑↓/数字键等以此为“当前 text”
    last_gid: Option<usize>,
    /// 正在按“句子”播放的 (gid, sid)；连续播放读完一句后据此续读下一句
    playing_sentence: Option<(usize, u8, usize)>,
    // 子进程
    analyze: Option<Worker>,
    asr_worker: Option<Worker>,
    analysis_files: Option<AnalysisFiles>,
    copy_workers: Vec<Worker>,
    // 线程通道：对齐 / 文件对话框
    align_tx: std::sync::mpsc::Sender<Vec<(usize, u8, Vec<asr::AsrSegment>)>>,
    align_rx: Option<Receiver<Vec<(usize, u8, Vec<asr::AsrSegment>)>>>,
    pick_tx: std::sync::mpsc::Sender<Vec<PathBuf>>,
    pick_rx: Option<Receiver<Vec<PathBuf>>>,
    save_rx: Option<Receiver<Option<PathBuf>>>,
    pending_export: Option<ExportJob>,
    temp_dir: PathBuf,
    timer: Timer,
    // Windows OLE 文件拖拽目标（保持存活直到窗口销毁）
    drop_target: Option<drop::DropReg>,
    drop_tick: u32,
    /// 快捷键说明子窗口（非模态，出现时主窗口仍可操作）
    help_window: Option<HelpWindow>,
}

/// 等待用户选完保存路径后发起的导出任务
struct ExportJob {
    kind: ExportKind,
    name: String,
    ranges: Vec<(f64, f64)>,
}

enum ExportKind {
    Group(usize),
    Checked,
    Selected,
}

// ============================================================
// 构造与接线
// ============================================================

impl App {
    fn new(ui: AppWindow, initial: Option<PathBuf>) -> Self {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        let cfg = Config::load_or_default(&exe_dir);
        let temp_dir = std::env::temp_dir().join("echodup");
        let _ = std::fs::create_dir_all(&temp_dir);

        let (align_tx, align_rx) = channel();
        let (pick_tx, pick_rx) = channel();

        // Windows OLE 文件拖拽注册（窗口显示后由 tick 持续确保）
        let mut drop_target: Option<drop::DropReg> = None;
        drop::ensure(&mut drop_target, &pick_tx);

        // 系统字体列表（查看 > 字体 菜单动态项）
        let fonts: Vec<slint::SharedString> = fonts::enum_system_fonts()
            .into_iter()
            .map(|f| f.into())
            .collect();
        if !fonts.is_empty() {
            ui.set_sys_fonts(ModelRc::new(VecModel::from(fonts)));
        }

        let mut app = Self {
            ui,
            cfg,
            file: None,
            audio: None,
            groups: Vec::new(),
            c_items: Vec::new(),
            display_nums: HashMap::new(),
            total_duration: 0.0,
            whole_loaded: false,
            slice_range: None,
            selected: None,
            asr_results: HashMap::new(),
            raw_asr: HashMap::new(),
            asr_results_b: HashMap::new(),
            raw_asr_b: HashMap::new(),
            tabs: HashMap::new(),
            asr_phase: 0,
            checked: HashSet::new(),
            transcript: String::new(),
            player: player::Player::new(),
            speed: 1.0,
            applied_speed: 1.0,
            dragging: false,
            just_seeked: false,
            split_mode: 1,
            rows: Vec::new(),
            peaks_base: Vec::new(),
            peaks_len: 0,
            last_gid: None,
            playing_sentence: None,
            analyze: None,
            asr_worker: None,
            analysis_files: None,
            copy_workers: Vec::new(),
            align_tx,
            align_rx: Some(align_rx),
            pick_tx,
            pick_rx: Some(pick_rx),
            save_rx: None,
            pending_export: None,
            temp_dir,
            timer: Timer::default(),
            drop_target,
            drop_tick: 0,
            help_window: None,
        };

        // 测试开关（仅 debug）：模拟拖入文件，验证“拖入 -> 分析”链路
        #[cfg(debug_assertions)]
        if let Ok(p) = std::env::var("ECHODUP_TEST_DROP") {
            if !p.trim().is_empty() {
                let _ = app.pick_tx.send(vec![PathBuf::from(p)]);
            }
        }

        // 初始参数 -> UI
        app.push_params();
        app.set_speed(1.0);
        if let Some(p) = initial {
            app.ui.set_file_label(SharedString::from(p.to_string_lossy().into_owned()));
            app.file = Some(p);
            app.start_analysis();
        }
        app
    }

    /// 注册所有 Slint 回调与定时器。self_rc 必须是包裹本 App 的 Rc。
    fn wire_up(self_rc: Rc<RefCell<App>>) {
        {
            let app = self_rc.borrow();
            let ui = app.ui.clone_strong();
            // --- 顶栏 ---
            ui.on_open_file({
                let rc = self_rc.clone();
                move || rc.borrow_mut().open_file()
            });
            ui.on_about({
                let rc = self_rc.clone();
                move || rc.borrow_mut().about()
            });
            ui.on_about_douyin({
                let rc = self_rc.clone();
                move || rc.borrow_mut().about_douyin()
            });
            ui.on_open_help({
                let rc = self_rc.clone();
                move || rc.borrow_mut().open_help()
            });
            // --- 左侧 ---
            ui.on_open_txt({
                let rc = self_rc.clone();
                move || rc.borrow_mut().open_txt()
            });
            ui.on_realign({
                let rc = self_rc.clone();
                move || rc.borrow_mut().realign_all()
            });
            ui.on_transcript_edited({
                let rc = self_rc.clone();
                move |t| {
                    let mut app = rc.borrow_mut();
                    app.transcript = t.to_string();
                    app.realign_all();
                }
            });
            ui.on_apply_params({
                let rc = self_rc.clone();
                move || rc.borrow_mut().apply_params()
            });
            // --- 播放 ---
            ui.on_toggle_play({
                let rc = self_rc.clone();
                move || rc.borrow_mut().toggle_play()
            });
            ui.on_speed_released({
                let rc = self_rc.clone();
                move |v| rc.borrow_mut().set_speed(v)
            });
            ui.on_speed_step({
                let rc = self_rc.clone();
                move |d| rc.borrow_mut().speed_step(d)
            });
            ui.on_speed_step_input({
                let rc = self_rc.clone();
                move |d| rc.borrow_mut().speed_step(d)
            });
            ui.on_speed_input_edit({
                let rc = self_rc.clone();
                move |t| rc.borrow_mut().speed_input(&t)
            });
            // --- 快捷键：连续/切换播放 ---
            ui.on_play_next_sentence({
                let rc = self_rc.clone();
                move || rc.borrow_mut().play_next_sentence()
            });
            ui.on_play_prev_sentence({
                let rc = self_rc.clone();
                move || rc.borrow_mut().play_prev_sentence()
            });
            ui.on_play_next_text({
                let rc = self_rc.clone();
                move || rc.borrow_mut().play_next_text()
            });
            ui.on_play_prev_text({
                let rc = self_rc.clone();
                move || rc.borrow_mut().play_prev_text()
            });
            ui.on_play_text_n({
                let rc = self_rc.clone();
                move |n| rc.borrow_mut().play_text_n(n as usize)
            });
            ui.on_copy_current_audio({
                let rc = self_rc.clone();
                move || rc.borrow_mut().copy_current_audio()
            });
            ui.on_copy_current_text({
                let rc = self_rc.clone();
                move || rc.borrow_mut().copy_current_text()
            });
            ui.on_select_all_current({
                let rc = self_rc.clone();
                move || rc.borrow_mut().select_all_current()
            });
            ui.on_select_none_current({
                let rc = self_rc.clone();
                move || rc.borrow_mut().select_none_current()
            });
            ui.on_invert_current({
                let rc = self_rc.clone();
                move || rc.borrow_mut().invert_current()
            });
            ui.on_export_current({
                let rc = self_rc.clone();
                move || rc.borrow_mut().export_current()
            });
            ui.on_export_current_text({
                let rc = self_rc.clone();
                move || rc.borrow_mut().export_current_text()
            });
            ui.on_reanalyze({
                let rc = self_rc.clone();
                move || rc.borrow_mut().apply_params()
            });
            ui.on_export_checked({
                let rc = self_rc.clone();
                move || rc.borrow_mut().export_checked()
            });
            // --- 列表 ---
            ui.on_toggle_checked({
                let rc = self_rc.clone();
                move |u| rc.borrow_mut().toggle_checked(u as usize)
            });
            ui.on_play_sentence({
                let rc = self_rc.clone();
                move |u| rc.borrow_mut().play_sentence_row(u as usize)
            });
            ui.on_copy_text({
                let rc = self_rc.clone();
                move |gid, side| rc.borrow_mut().copy_text(gid as usize, side as u8)
            });
            ui.on_copy_audio({
                let rc = self_rc.clone();
                move |gid, side| rc.borrow_mut().copy_audio(gid as usize, side as u8)
            });
            ui.on_export_group({
                let rc = self_rc.clone();
                move |gid, side| rc.borrow_mut().export_group(gid as usize, side as u8)
            });
            ui.on_set_tab({
                let rc = self_rc.clone();
                move |gid, tab| rc.borrow_mut().set_tab(gid as usize, tab)
            });
            ui.on_set_split_mode({
                let rc = self_rc.clone();
                move |mode| rc.borrow_mut().set_split_mode(mode)
            });
            ui.on_select_side({
                let rc = self_rc.clone();
                move |gid, side| rc.borrow_mut().select_side(gid as usize, side as u8)
            });
            // --- 进度条 ---
            ui.on_bar_drag({
                let rc = self_rc.clone();
                move |ratio, phase| rc.borrow_mut().bar_drag(ratio, phase)
            });
            ui.on_bar_click({
                let rc = self_rc.clone();
                move |ratio| rc.borrow_mut().bar_click(ratio)
            });
            ui.on_bar_dbl({
                let rc = self_rc.clone();
                move |ratio| rc.borrow_mut().bar_dbl(ratio)
            });
            ui.on_seek_time({
                let rc = self_rc.clone();
                move |uidx, which| rc.borrow_mut().seek_time(uidx as usize, which)
            });
            // --- 波形色带点击 ---
            ui.on_wave_click({
                let rc = self_rc.clone();
                move |ratio| rc.borrow_mut().wave_click(ratio)
            });
            // --- 波形宽度变化（动态列数） ---
            ui.on_wave_resized({
                let rc = self_rc.clone();
                move |w| rc.borrow_mut().wave_resized(w as usize)
            });
        }

        // 定时轮询：30ms（Timer 存活在 App 字段中，随事件循环结束而停止）
        self_rc.borrow_mut().timer.start(
            TimerMode::Repeated,
            std::time::Duration::from_millis(30),
            {
                let rc = self_rc.clone();
                move || rc.borrow_mut().tick()
            },
        );
    }

    fn push_params(&mut self) {
        let c = &self.cfg;
        self.ui.set_threshold_db(c.threshold_db as f32);
        self.ui.set_min_silence(c.min_silence as f32);
        self.ui.set_min_sound(c.min_sound as f32);
        self.ui.set_zone_tolerance(c.zone_tolerance as f32);
    }

    fn set_status(&mut self, s: impl Into<SharedString>) {
        self.ui.set_status(s.into());
    }

    // ============================================================
    // 定时轮询
    // ============================================================

    fn tick(&mut self) {
        // 确保 OLE 拖拽已注册（窗口显示后句柄才可用；每 100 tick≈3s 复查句柄变化）
        self.drop_tick += 1;
        if self.drop_tick % 100 == 0 || self.drop_target.is_none() {
            drop::ensure(&mut self.drop_target, &self.pick_tx);
        }
        self.poll_analyze();
        self.poll_asr();
        self.poll_copiers();
        self.poll_align();
        self.poll_pick();
        self.poll_save();
        self.player.feed();
        self.update_playback_view();
    }

    fn poll_analyze(&mut self) {
        let msgs = self.analyze.as_mut().map(|w| w.drain()).unwrap_or_default();
        for m in msgs {
            match m {
                ProcMsg::Json(line) => self.handle_analyze_line(&line),
                ProcMsg::Exit(code) => {
                    let abnormal = code != 0 && self.analyze.is_some();
                    self.analyze = None;
                    if abnormal {
                        self.set_status("分析进程异常退出");
                    }
                }
            }
        }
    }

    fn handle_analyze_line(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("stage") => {
                let s = v.get("stage").and_then(|x| x.as_str()).unwrap_or("");
                let label = match s {
                    "decode" => "解码中…",
                    "detect" => "切分检测中…",
                    _ => "分析中…",
                };
                self.set_status(label);
            }
            Some("error") => {
                let msg = v.get("msg").and_then(|x| x.as_str()).unwrap_or("未知错误");
                self.set_status(format!("错误：{msg}"));
            }
            Some("done") => self.finalize_analysis(),
            _ => {}
        }
    }

    fn poll_asr(&mut self) {
        let msgs = self.asr_worker.as_mut().map(|w| w.drain()).unwrap_or_default();
        for m in msgs {
            match m {
                ProcMsg::Json(line) => self.handle_asr_line(&line),
                ProcMsg::Exit(code) => {
                    let abnormal = code != 0 && self.asr_worker.is_some();
                    self.asr_worker = None;
                    if abnormal {
                        self.set_status("转写进程异常退出");
                    }
                }
            }
        }
    }

    fn handle_asr_line(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("group") => {
                let Some(gid) = v.get("gid").and_then(|g| g.as_u64()) else {
                    return;
                };
                let Some(segs) = v.get("segs").and_then(|s| s.as_array()) else {
                    return;
                };
                let segs: Vec<asr::AsrSegment> = segs
                    .iter()
                    .filter_map(|s| {
                        Some(asr::AsrSegment {
                            start: s.get("start")?.as_f64()?,
                            end: s.get("end")?.as_f64()?,
                            text: s.get("text")?.as_str()?.to_string(),
                        })
                    })
                    .collect();
                let gid = gid as usize;
                let side: u8 = if self.asr_phase == 1 { 1 } else { 0 };
                if side == 1 {
                    self.raw_asr_b.insert(gid, segs.clone());
                } else {
                    self.raw_asr.insert(gid, segs.clone());
                }
                self.apply_alignment_seg(gid, side);
                let n_done = if side == 1 { self.raw_asr_b.len() } else { self.raw_asr.len() };
                let tag = if side == 1 { "B 段" } else { "C+A 段" };
                self.set_status(format!("转写中…（{tag} {n_done} 组）"));
                self.rebuild_rows();
            }
            Some("group_error") => {
                let msg = v.get("msg").and_then(|x| x.as_str()).unwrap_or("转写失败");
                self.set_status(msg);
            }
            Some("done") => {
                self.asr_worker = None;
                if self.asr_phase == 0 && !self.groups.is_empty() {
                    // C+A 全部完成 -> 转写 B 段
                    self.asr_phase = 1;
                    self.set_status("C+A 段转写完成，开始转写 B 段…");
                    self.start_asr_b();
                } else {
                    self.asr_phase = 2;
                    let n = self.raw_asr.len() + self.raw_asr_b.len();
                    self.set_status(format!("转写完成：共 {n} 组（A/B/C）"));
                }
                self.rebuild_rows();
            }
            Some("error") => {
                let msg = v.get("msg").and_then(|x| x.as_str()).unwrap_or("未知错误");
                self.set_status(format!("转写错误：{msg}"));
            }
            _ => {}
        }
    }

    fn poll_copiers(&mut self) {
        let mut done: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < self.copy_workers.len() {
            let msgs = self.copy_workers[i].drain();
            for m in msgs {
                match m {
                    ProcMsg::Json(line) => {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                            match v.get("type").and_then(|t| t.as_str()) {
                                Some("done") => {
                                    let path = v
                                        .get("path")
                                        .and_then(|p| p.as_str())
                                        .unwrap_or("");
                                    self.set_status(format!("已复制: {path}"));
                                }
                                Some("error") => {
                                    let msg = v
                                        .get("msg")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("失败");
                                    self.set_status(format!("复制失败: {msg}"));
                                }
                                _ => {}
                            }
                        }
                    }
                    ProcMsg::Exit(code) => {
                        if code != 0 {
                            self.set_status("复制/导出进程异常退出");
                        }
                        done.push(i);
                        break;
                    }
                }
            }
            i += 1;
        }
        for &idx in done.iter().rev() {
            self.copy_workers.remove(idx);
        }
    }

    fn poll_align(&mut self) {
        let mut results: Vec<(usize, u8, Vec<asr::AsrSegment>)> = Vec::new();
        if let Some(rx) = &self.align_rx {
            while let Ok(r) = rx.try_recv() {
                results.extend(r);
            }
        }
        if !results.is_empty() {
            for (gid, side, segs) in results {
                let res_map = if side == 1 { &mut self.asr_results_b } else { &mut self.asr_results };
                self.checked.retain(|(g, sd, _)| *g != gid || *sd != side);
                for i in 0..segs.len() {
                    self.checked.insert((gid, side, i));
                }
                res_map.insert(gid, segs);
            }
            self.rebuild_rows();
        }
    }

    fn poll_pick(&mut self) {
        let mut files: Vec<PathBuf> = Vec::new();
        if let Some(rx) = &self.pick_rx {
            while let Ok(ps) = rx.try_recv() {
                files.extend(ps);
            }
        }
        if files.is_empty() {
            return;
        }
        let mut audio: Option<PathBuf> = None;
        let mut txt: Vec<PathBuf> = Vec::new();
        for p in files {
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if matches!(ext.as_str(), "wav" | "mp3" | "flac" | "ogg" | "m4a" | "aac") {
                if audio.is_none() {
                    audio = Some(p);
                }
            } else if matches!(ext.as_str(), "txt" | "md") {
                txt.push(p);
            }
        }
        for p in txt {
            if let Ok(t) = std::fs::read_to_string(&p) {
                if !t.trim().is_empty() {
                    self.transcript = t;
                    self.ui.set_transcript(self.transcript.clone().into());
                    self.realign_all();
                }
            }
        }
        if let Some(p) = audio {
            self.file = Some(p.clone());
            self.ui.set_file_label(SharedString::from(p.to_string_lossy().into_owned()));
            self.start_analysis();
        }
    }

    fn poll_save(&mut self) {
        if self.save_rx.is_none() {
            return;
        }
        let mut path: Option<Option<PathBuf>> = None;
        if let Some(rx) = &self.save_rx {
            if let Ok(p) = rx.try_recv() {
                path = Some(p);
            }
        }
        if let Some(p) = path {
            self.save_rx = None;
            if let Some(job) = self.pending_export.take() {
                match p {
                    Some(out_path) => self.spawn_export(job, out_path),
                    None => self.set_status("已取消导出"),
                }
            }
        }
    }

    // ============================================================
    // 播放视图刷新（每帧）
    // ============================================================

    fn update_playback_view(&mut self) {
        if !self.whole_loaded {
            self.ui.set_progress(0.0);
            self.ui.set_time_label("0:00 / 0:00".into());
            self.ui.set_playing(false);
            self.ui.set_playing_gid(-1);
            self.ui.set_playing_sid(-1);
            return;
        }
        // 内容位置 = 原始音频秒（实时 timestretch 下进度按内容推进，与倍速无关）
        let file_pos = self.player.current_time_sec();
        let dur = self.total_duration;

        // 选中片段（点句子/点波形色带）播放到句尾：
        // - 连续播放开启且当前是“句子播放” → 自动续读下一句（跨 Text）
        // - 否则放完即暂停，不循环重播
        if let Some((_, e)) = self.slice_range {
            if self.player.is_playing() && file_pos >= e {
                let mut finished = true;
                if self.ui.get_continuous() {
                    if let Some((gid, _, _)) = self.playing_sentence {
                        finished = !self.play_text_whole_next(gid);
                    }
                }
                if finished {
                    self.player.pause();
                    self.slice_range = None;
                }
            }
        }

        // 高亮当前播放句子
        let (gid, side, sid) = self.current_sentence(file_pos);
        self.ui.set_playing_gid(gid.map(|v| v as i32).unwrap_or(-1));
        self.ui.set_playing_sid(sid.map(|v| v as i32).unwrap_or(-1));
        // 歌词跟随策略：
        // - 点击进度条跳转（just_seeked）：强制聚焦一次（暂停中也聚焦）
        // - 播放中每进一句：聚焦一次
        // - 暂停且未跳转：不滚动，允许用户自由滑动列表
        let mut focus_scroll = false;
        if self.just_seeked {
            self.just_seeked = false;
            focus_scroll = true;
        }
        if let Some(g) = gid {
            // 统一焦点：跳转/播放推进到某句时，同步键盘导航基准（playing_sentence/last_gid），
            // 这样 ←/→/↑/↓ 从跳转后的句子继续，而不是停留在跳转前的旧句子
            let sd = side.unwrap_or(0);
            if self.playing_sentence != Some((g, sd, sid.unwrap_or(0))) {
                self.playing_sentence = Some((g, sd, sid.unwrap_or(0)));
                self.last_gid = Some(g);
                if self.player.is_playing() {
                    focus_scroll = true;
                }
                // 命中句子所在段自动切换组头 tab（A/B 组）：
                // 跳转与普通播放推进都强制切，保证"焦点所在段 == 组头显示的 tab"。
                // 切一次后 tab 匹配，不会反复 rebuild。
                let cur_tab = self.tabs.get(&g).copied().unwrap_or(0);
                if self.groups.iter().any(|gg| gg.group_id == g) && cur_tab != sd {
                    self.set_tab(g, sd as i32);
                }
            }
        }
        if focus_scroll {
            if let Some(g) = gid {
                self.scroll_to_row(g, sid.unwrap_or(0));
            }
        }

        self.ui.set_progress(self.player.progress());
        self.ui.set_time_label(
            format!("{} / {}", fmt_ts(file_pos), fmt_ts(dur)).into(),
        );
        self.ui.set_playing(self.player.is_playing());
    }

    fn current_sentence(&self, file_pos: f64) -> (Option<usize>, Option<u8>, Option<usize>) {
        for g in &self.groups {
            if let Some(segs) = self.asr_results.get(&g.group_id) {
                for (i, seg) in segs.iter().enumerate() {
                    let s = g.a.0 + seg.start;
                    let e = g.a.0 + seg.end;
                    if file_pos >= s && file_pos <= e {
                        return (Some(g.group_id), Some(0), Some(i));
                    }
                }
            }
        }
        for g in &self.groups {
            if let Some(segs) = self.asr_results_b.get(&g.group_id) {
                for (i, seg) in segs.iter().enumerate() {
                    let s = g.b.0 + seg.start;
                    let e = g.b.0 + seg.end;
                    if file_pos >= s && file_pos <= e {
                        return (Some(g.group_id), Some(1), Some(i));
                    }
                }
            }
        }
        for &(gid, s0, _) in &self.c_items {
            if let Some(segs) = self.asr_results.get(&gid) {
                for (i, seg) in segs.iter().enumerate() {
                    let s = s0 + seg.start;
                    let e = s0 + seg.end;
                    if file_pos >= s && file_pos <= e {
                        return (Some(gid), Some(0), Some(i));
                    }
                }
            }
        }
        (None, None, None)
    }

    // ============================================================
    // 顶栏 / 左侧回调
    // ============================================================

    fn open_file(&mut self) {
        let tx = self.pick_tx.clone();
        std::thread::spawn(move || {
            let ps = rfd::FileDialog::new()
                .add_filter(
                    "Audio + Text",
                    &["wav", "mp3", "flac", "ogg", "m4a", "aac", "txt", "md"],
                )
                .add_filter("All files", &["*"])
                .pick_files();
            let _ = tx.send(ps.unwrap_or_default());
        });
        self.set_status("请选择音频文件…");
    }

    /// 打开快捷键说明子窗口（非模态）
    fn open_help(&mut self) {
        if self.help_window.is_none() {
            match HelpWindow::new() {
                Ok(w) => self.help_window = Some(w),
                Err(_) => return,
            }
        }
        if let Some(w) = &self.help_window {
            let _ = w.show();
        }
    }

    fn about(&mut self) {
        let _ = std::process::Command::new("explorer")
            .arg("https://b23.tv/sA4egRE")
            .spawn();
    }

    fn open_txt(&mut self) {
        let tx = self.pick_tx.clone();
        std::thread::spawn(move || {
            let ps = rfd::FileDialog::new()
                .add_filter("Text", &["txt", "md"])
                .add_filter("All files", &["*"])
                .pick_files();
            let _ = tx.send(ps.unwrap_or_default());
        });
        self.set_status("请选择文本文件…");
    }

    fn apply_params(&mut self) {
        self.cfg.threshold_db = self.ui.get_threshold_db() as f64;
        self.cfg.min_silence = self.ui.get_min_silence() as f64;
        self.cfg.min_sound = self.ui.get_min_sound() as f64;
        self.cfg.zone_tolerance = self.ui.get_zone_tolerance() as f64;
        self.start_analysis();
    }

    // ============================================================
    // 分析（子进程）
    // ============================================================

    fn start_analysis(&mut self) {
        let Some(path) = self.file.clone() else {
            return;
        };
        // 清理旧的 worker 与缓存
        if let Some(mut w) = self.analyze.take() {
            w.kill();
        }
        if let Some(mut w) = self.asr_worker.take() {
            w.kill();
        }
        for w in self.copy_workers.iter_mut() {
            w.kill();
        }
        self.copy_workers.clear();
        // 注销文件拖拽目标
        drop::revoke(&mut self.drop_target);
        if let Some(f) = &self.analysis_files {
            let _ = std::fs::remove_file(&f.cache_wav);
            let _ = std::fs::remove_file(&f.result_json);
            let _ = std::fs::remove_file(&f.payload);
        }
        self.analysis_files = None;

        self.groups.clear();
        self.c_items.clear();
        self.display_nums.clear();
        self.audio = None;
        self.asr_results.clear();
        self.raw_asr.clear();
        self.asr_results_b.clear();
        self.raw_asr_b.clear();
        self.tabs.clear();
        self.asr_phase = 0;
        self.checked.clear();
        self.selected = None;
        self.slice_range = None;
        self.whole_loaded = false;
        self.rows.clear();
        self.ui
            .set_rows(ModelRc::from(Rc::new(VecModel::from(Vec::<RowData>::new()))));
        self.ui
            .set_bands(ModelRc::from(Rc::new(VecModel::from(Vec::<BandMark>::new()))));
        self.peaks_base.clear();
        self.peaks_len = 0;
        self.ui.set_wave_peaks(ModelRc::from(Rc::new(VecModel::from(vec![0.0; 64]))));
        self.ui.set_can_play(false);
        self.ui.set_progress(0.0);
        self.ui.set_playing(false);
        self.set_status("分析中…");

        // 写载荷
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("input");
        let cache_wav = self.temp_dir.join(format!("{stem}_cache_{nanos}.wav"));
        let result_json = self.temp_dir.join(format!("{stem}_result_{nanos}.json"));
        let payload = self.temp_dir.join(format!("{stem}_analyze_{nanos}.json"));

        let req = serde_json::json!({
            "audio": path.to_string_lossy(),
            "cfg": self.cfg,
            "cache_wav": cache_wav.to_string_lossy(),
            "result_json": result_json.to_string_lossy(),
        });
        let Ok(payload_json) = serde_json::to_vec(&req) else {
            self.set_status("内部错误：载荷序列化失败");
            return;
        };
        if let Err(e) = std::fs::write(&payload, payload_json) {
            self.set_status(format!("写载荷失败: {e}"));
            return;
        }
        self.analysis_files = Some(AnalysisFiles {
            cache_wav,
            result_json,
            payload: payload.clone(),
        });
        match Worker::spawn("analyze", &payload) {
            Ok(w) => self.analyze = Some(w),
            Err(e) => self.set_status(format!("启动分析进程失败: {e}")),
        }
    }

    fn finalize_analysis(&mut self) {
        // 保留 analysis_files：后续 复制/导出 子进程仍要读缓存 wav
        let Some(f) = self.analysis_files.as_ref() else {
            return;
        };
        let result_json = f.result_json.clone();
        let cache_wav = f.cache_wav.clone();

        // 读结果
        let Ok(text) = std::fs::read_to_string(&result_json) else {
            self.set_status("分析结果缺失");
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            self.set_status("分析结果解析失败");
            return;
        };
        let sr = v.get("sample_rate").and_then(|x| x.as_u64()).unwrap_or(16000) as u32;
        self.total_duration = v.get("total_duration").and_then(|x| x.as_f64()).unwrap_or(0.0);
        self.groups = v
            .get("groups")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|g| {
                        Some(detect::RepeatGroup {
                            group_id: g.get("group_id")?.as_u64()? as usize,
                            a: (g.get("a0")?.as_f64()?, g.get("a1")?.as_f64()?),
                            b: (g.get("b0")?.as_f64()?, g.get("b1")?.as_f64()?),
                            confidence: g.get("confidence")?.as_f64()? as f32,
                            match_type: "perceptual",
                            offset_sec: g.get("offset_sec")?.as_f64()?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        // C 段（不重复段，直接 ASR）：(gid, start, end)。gid 由 worker 分配在 AB 之后。
        self.c_items = v
            .get("c_segments")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| {
                        Some((
                            c.get("id")?.as_u64()? as usize,
                            c.get("start")?.as_f64()?,
                            c.get("end")?.as_f64()?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 出现顺序编号：AB 组与 C 段按起点时间统一编 1..N（与原文 Text 标号同序）
        let mut items: Vec<(f64, usize)> = self
            .groups
            .iter()
            .map(|g| (g.a.0, g.group_id))
            .chain(self.c_items.iter().map(|&(gid, s, _)| (s, gid)))
            .collect();
        items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        self.display_nums = items
            .iter()
            .enumerate()
            .map(|(i, &(_, gid))| (gid, i + 1))
            .collect();

        // 读缓存 wav -> 音频内存
        let (mono, wav_sr) = match clipboard::read_wav_mono(&cache_wav) {
            Ok(x) => x,
            Err(e) => {
                self.set_status(format!("读缓存音频失败: {e:#}"));
                return;
            }
        };
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
        // 播放路径不参与 detect：包络给空（与 16k mono 无实际用途，仅保持类型一致）。
        let audio = audio_io::AudioData {
            mono: mono.clone(),
            sample_rate: sr,
            pcm11k,
            pcm5k: Vec::new(),
            peak_env: Vec::new(),
            avg_env: Vec::new(),
            rms_env: Vec::new(),
            original_sample_rate: wav_sr,
            original_channels: 1,
            control_rate: 0.0,
        };
        self.audio = Some(audio);
        self.player.load(&mono, sr, self.speed);
        self.applied_speed = self.speed;
        self.whole_loaded = true;
        self.ui.set_can_play(true);

        // 波形基础峰值（1600 列缓存），并按当前频谱宽度设置实际列数
        self.peaks_base = wave_peaks(&mono, 1600);
        self.peaks_len = 0;
        self.wave_resized(self.ui.get_wave_w() as usize);

        // 进度条识别色带（A/B 段比例位置 + C 段）
        let d = self.total_duration.max(1e-9);
        let mut marks: Vec<BandMark> = self
            .groups
            .iter()
            .flat_map(|g| {
                [
                    BandMark { x0: (g.a.0 / d) as f32, x1: (g.a.1 / d) as f32, side: 0 },
                    BandMark { x0: (g.b.0 / d) as f32, x1: (g.b.1 / d) as f32, side: 1 },
                ]
            })
            .collect();
        for &(_, s, e) in &self.c_items {
            marks.push(BandMark { x0: (s / d) as f32, x1: (e / d) as f32, side: 2 });
        }
        marks.sort_by(|a, b| a.x0.partial_cmp(&b.x0).unwrap());
        self.ui
            .set_bands(ModelRc::from(Rc::new(VecModel::from(marks))));

        // 导出三件套（快，主进程即可）
        self.export_all(true);

        self.rebuild_rows();
        self.set_status(format!(
            "完成：共 {} 组重复 + {} 段独立",
            self.groups.len(),
            self.c_items.len()
        ));

        // 后台子进程：AB 组只转写 A 段（B 是重复，不重复转写）；C 段直接转写。
        if self.groups.is_empty() && self.c_items.is_empty() {
            return;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let payload = self.temp_dir.join(format!("asr_{nanos}.json"));
        let out_json = self.temp_dir.join(format!("asr_out_{nanos}.json"));
        // C+A 段按时间顺序转写：groups 的 A 段与 C 段统一按起点排序
        let mut ca: Vec<(f64, serde_json::Value)> = self
            .groups
            .iter()
            .map(|g| (g.a.0, serde_json::json!({"gid": g.group_id, "start": g.a.0, "end": g.a.1})))
            .collect();
        for &(gid, s, e) in &self.c_items {
            ca.push((s, serde_json::json!({"gid": gid, "start": s, "end": e})));
        }
        ca.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let slices: Vec<serde_json::Value> = ca.into_iter().map(|(_, v)| v).collect();
        self.asr_phase = 0;
        let req = serde_json::json!({
            "cache_wav": cache_wav.to_string_lossy(),
            "slices": slices,
            "result_json": out_json.to_string_lossy(),
        });
        if let Ok(bytes) = serde_json::to_vec(&req) {
            if std::fs::write(&payload, bytes).is_ok() {
                if let Ok(w) = Worker::spawn("asr", &payload) {
                    self.asr_worker = Some(w);
                    self.set_status("分析完成，正在转写…");
                }
            }
        }
        let _ = std::fs::remove_file(&result_json);
    }

    // ============================================================
    // ASR / 对齐
    // ============================================================

    /// gid -> 出现顺序编号（1..N；AB 组与 C 段统一按时间排序）
    fn disp_num(&self, gid: usize) -> usize {
        self.display_nums.get(&gid).copied().unwrap_or(gid)
    }

    /// 从原文中取出 Text N 块（N 为出现顺序编号，与原文标号一致）
    fn transcript_block(&self, gid: usize) -> String {
        block_for(&self.transcript, self.disp_num(gid))
    }

    fn apply_alignment_seg(&mut self, gid: usize, side: u8) {
        // 先取 owned block，避免与 res_map 可变借用冲突
        let block = self.transcript_block(gid);
        let raw_map = if side == 1 { &self.raw_asr_b } else { &self.raw_asr };
        let res_map = if side == 1 { &mut self.asr_results_b } else { &mut self.asr_results };
        if self.transcript.trim().is_empty() {
            if let Some(segs) = raw_map.get(&gid) {
                self.checked.retain(|(g, sd, _)| *g != gid || *sd != side);
                for i in 0..segs.len() {
                    self.checked.insert((gid, side, i));
                }
                res_map.insert(gid, segs.clone());
            }
            return;
        }
        let Some(raw) = raw_map.get(&gid) else { return };
        if block.is_empty() {
            self.checked.retain(|(g, sd, _)| *g != gid || *sd != side);
            for i in 0..raw.len() {
                self.checked.insert((gid, side, i));
            }
            res_map.insert(gid, raw.clone());
            return;
        }
        let mode = self.split_mode;
        let tuples: Vec<(f64, f64, String)> =
            raw.iter().map(|s| (s.start, s.end, s.text.clone())).collect();
        let aligned = align::align(&block, &tuples, align::SplitMode::from_repr(mode));
        if aligned.is_empty() {
            self.checked.retain(|(g, sd, _)| *g != gid || *sd != side);
            for i in 0..raw.len() {
                self.checked.insert((gid, side, i));
            }
            res_map.insert(gid, raw.clone());
            return;
        }
        let segs: Vec<asr::AsrSegment> = aligned
            .into_iter()
            .map(|a| asr::AsrSegment { start: a.start, end: a.end, text: a.text })
            .collect();
        self.checked.retain(|(g, sd, _)| *g != gid || *sd != side);
        for i in 0..segs.len() {
            self.checked.insert((gid, side, i));
        }
        res_map.insert(gid, segs);
    }

    /// 组头 TabWidget 切换：记录当前段并重建行
    fn set_tab(&mut self, gid: usize, tab: i32) {
        if !(0..=1).contains(&tab) {
            return;
        }
        self.tabs.insert(gid, tab as u8);
        self.rebuild_rows();
    }

    /// 查看 > ASR 内容分行：切换分行模式，并用缓存的原始 ASR + 原文重新对齐所有组
    fn set_split_mode(&mut self, mode: i32) {
        if !(0..=2).contains(&mode) {
            return;
        }
        self.split_mode = mode as u8;
        let gids: Vec<usize> = self.raw_asr.keys().copied().collect();
        for gid in gids {
            self.apply_alignment_seg(gid, 0);
        }
        let gids_b: Vec<usize> = self.raw_asr_b.keys().copied().collect();
        for gid in gids_b {
            self.apply_alignment_seg(gid, 1);
        }
        self.rebuild_rows();
        let msg: &str = match self.split_mode {
            0 => "ASR 分行：按句号",
            1 => "ASR 分行：按换行",
            _ => "ASR 分行：按原文 M:/W:",
        };
        self.set_status(msg.to_string());
    }

    /// 导出当前 Text 当前段选中文本为 txt（后台线程弹保存框）
    fn export_current_text(&mut self) {
        let Some(gid) = self.current_gid() else {
            self.set_status("未播放任何 Text");
            return;
        };
        let side = self.tabs.get(&gid).copied().unwrap_or(0);
        let segs = if side == 1 {
            self.asr_results_b.get(&gid)
        } else {
            self.asr_results.get(&gid)
        };
        let Some(segs) = segs else {
            self.set_status(format!("Text{} {}段 尚未转写", self.disp_num(gid), if side == 1 { "B" } else { "A" }));
            return;
        };
        let text: Vec<String> = segs
            .iter()
            .enumerate()
            .filter(|(i, _)| self.checked.contains(&(gid, side, *i)))
            .map(|(_, seg)| seg.text.clone())
            .collect();
        if text.is_empty() {
            self.set_status(format!("Text{} 未勾选任何句子", self.disp_num(gid)));
            return;
        }
        let content = text.join("\r\n");
        let dnum = self.disp_num(gid);
        let name = format!("echodup_text{}_sel.txt", dnum);
        self.set_status(format!("正在导出 Text{dnum} 文本…"));
        std::thread::spawn(move || {
            if let Some(p) = rfd::FileDialog::new()
                .set_file_name(&name)
                .add_filter("Text", &["txt"])
                .save_file()
            {
                let _ = std::fs::write(&p, content);
            }
        });
    }

    /// 关于 -> 抖音主页
    fn about_douyin(&mut self) {
        let _ = std::process::Command::new("explorer")
            .arg("https://www.douyin.com/user/MS4wLjABAAAARBhJeagY3LzsQF-643Ho8qqrTN5fr2YIy7RgWDwJK7A")
            .spawn();
    }

    /// 启动 B 段转写：每组 b 区间作为一个切片（gid 与 A 相同）
    fn start_asr_b(&mut self) {
        if self.groups.is_empty() {
            self.asr_phase = 2;
            return;
        }
        let Some(f) = self.analysis_files.as_ref() else {
            self.asr_phase = 2;
            return;
        };
        let cache_wav = f.cache_wav.clone();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let payload = self.temp_dir.join(format!("asr_b_{nanos}.json"));
        let out_json = self.temp_dir.join(format!("asr_b_out_{nanos}.json"));
        let slices: Vec<serde_json::Value> = self
            .groups
            .iter()
            .map(|g| serde_json::json!({"gid": g.group_id, "start": g.b.0, "end": g.b.1}))
            .collect();
        let req = serde_json::json!({
            "cache_wav": cache_wav.to_string_lossy(),
            "slices": slices,
            "result_json": out_json.to_string_lossy(),
        });
        if let Ok(bytes) = serde_json::to_vec(&req) {
            if std::fs::write(&payload, bytes).is_ok() {
                if let Ok(w) = Worker::spawn("asr", &payload) {
                    self.asr_worker = Some(w);
                }
            }
        }
    }

    /// 后台线程对所有组重新对齐，避免卡住 UI
    fn realign_all(&mut self) {
        if self.raw_asr.is_empty() {
            return;
        }
        let raw_a = self.raw_asr.clone();
        let raw_b = self.raw_asr_b.clone();
        let transcript = self.transcript.clone();
        let nums = self.display_nums.clone();
        let mode = self.split_mode;
        let tx = self.align_tx.clone();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            // A/C 段
            for (gid, segs) in &raw_a {
                let num = nums.get(gid).copied().unwrap_or(*gid);
                let block = block_for(&transcript, num);
                let res = if transcript.trim().is_empty() || block.is_empty() {
                    segs.clone()
                } else {
                    let tuples: Vec<(f64, f64, String)> =
                        segs.iter().map(|x| (x.start, x.end, x.text.clone())).collect();
                    align::align(&block, &tuples, align::SplitMode::from_repr(mode))
                        .into_iter()
                        .map(|a| asr::AsrSegment { start: a.start, end: a.end, text: a.text })
                        .collect()
                };
                out.push((*gid, 0, res));
            }
            // B 段：同一段原文分别对齐
            for (gid, segs) in &raw_b {
                let num = nums.get(gid).copied().unwrap_or(*gid);
                let block = block_for(&transcript, num);
                let res = if transcript.trim().is_empty() || block.is_empty() {
                    segs.clone()
                } else {
                    let tuples: Vec<(f64, f64, String)> =
                        segs.iter().map(|x| (x.start, x.end, x.text.clone())).collect();
                    align::align(&block, &tuples, align::SplitMode::from_repr(mode))
                        .into_iter()
                        .map(|a| asr::AsrSegment { start: a.start, end: a.end, text: a.text })
                        .collect()
                };
                out.push((*gid, 1, res));
            }
            let _ = tx.send(out);
        });
    }

    // ============================================================
    // 导出（写 json/csv/labels，快，主进程）
    // ============================================================

    fn export_all(&mut self, silent: bool) {
        let Some(path) = &self.file else { return };
        if self.groups.is_empty() {
            return;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let src = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("input");
        let out = output::build(&self.groups, src, self.total_duration);
        let json = dir.join(format!("{stem}_repeats.json"));
        let csv = dir.join(format!("{stem}_repeats.csv"));
        let labels = dir.join(format!("{stem}_audacity_labels.txt"));
        let _ = output::write_json(&out, &json);
        let _ = output::write_csv(&out, &csv);
        let _ = output::write_labels(&out, &labels);
        if !silent {
            self.set_status(format!("已导出到 {}", dir.display()));
        }
    }

    // ============================================================
    // 播放控制
    // ============================================================

    fn toggle_play(&mut self) {
        if !self.whole_loaded {
            return;
        }
        if self.player.is_playing() {
            self.player.pause();
            return;
        }
        // 从当前位置继续；若已播到末尾则从头开始。
        // 注意：不再隐式跳转到残留的选中片段——否则"播放"每次都从片段中途/起点
        // 开始，进度条已播放的蓝色填充起点就不在 0。点波形/句子等显式操作才会跳转。
        if self.player.progress() >= 0.999 {
            self.player.seek(0.0);
            self.slice_range = None;
        }
        self.player.play();
    }

    fn set_speed(&mut self, v: f32) {
        // 输入范围 0.1–100；engine 实际只支持 0.25–4.0（timestretch 硬范围），
        // 超出自动钳制并显示实际生效值；滑杆显示钳到滑杆范围 0.6–2.0。
        let v = v.clamp(0.1, 100.0);
        let eff = v.clamp(0.25, 4.0);
        self.speed = eff;
        self.ui.set_speed(eff.clamp(0.6, 2.0));
        self.ui.set_speed_input(eff);
        self.ui.set_speed_text(format_speed(eff).into());
        if !self.whole_loaded {
            return;
        }
        if (self.applied_speed - eff).abs() >= 1e-4 {
            self.applied_speed = eff;
            self.player.set_speed(eff);
        }
    }

    fn speed_step(&mut self, delta: f32) {
        let v = (self.speed + delta).clamp(0.1, 100.0);
        self.set_speed(v);
    }

    fn speed_input(&mut self, t: &str) {
        match t.trim().parse::<f32>() {
            Ok(v) => self.set_speed(v),
            Err(_) => {
                // 无效输入：恢复显示当前倍速
                self.ui.set_speed_text(format_speed(self.speed).into());
            }
        }
    }

    // ============================================================
    // 快捷键 / 连续播放 / 数字键定位
    // ============================================================

    /// 按出现顺序返回所有 text 的 gid 列表（与列表渲染一致）
    fn sorted_gids(&self) -> Vec<usize> {
        let mut entries: Vec<(f64, usize)> = self
            .groups
            .iter()
            .map(|g| (g.a.0, g.group_id))
            .chain(self.c_items.iter().map(|&(gid, s, _)| (s, gid)))
            .collect();
        entries.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        entries.into_iter().map(|(_, gid)| gid).collect()
    }

    /// 当前 text：最近播放/聚焦的 gid
    fn current_gid(&self) -> Option<usize> {
        self.last_gid
    }

    fn gid_sentence_count(&self, gid: usize, side: u8) -> usize {
        if side == 1 {
            self.asr_results_b.get(&gid)
        } else {
            self.asr_results.get(&gid)
        }
        .map(|s| s.len())
        .unwrap_or(0)
    }

    /// 播放指定 text 第 side 段（0=A/1=B）的第 sid 句；该段尚无 ASR 则播放整段
    fn play_sentence_id(&mut self, gid: usize, side: u8, sid: usize) {
        let span = if let Some(g) = self.groups.iter().find(|g| g.group_id == gid) {
            Some(if side == 1 { g.b } else { g.a })
        } else if let Some(&(_, s, e)) = self.c_items.iter().find(|&&(g2, _, _)| g2 == gid) {
            Some((s, e))
        } else {
            None
        };
        let Some((s0, e0)) = span else { return };
        let segs = if side == 1 {
            self.asr_results_b.get(&gid)
        } else {
            self.asr_results.get(&gid)
        };
        if let Some(segs) = segs {
            if let Some(seg) = segs.get(sid) {
                self.playing_sentence = Some((gid, side, sid));
                self.last_gid = Some(gid);
                if self.ui.get_continuous() {
                    // 连续播放：从该句一直播到当前段末尾（句间无衔接），播完自动切下一个 Text
                    self.play_sentence(s0 + seg.start, e0);
                } else {
                    self.play_sentence(s0 + seg.start, s0 + seg.end);
                }
                return;
            }
        }
        self.playing_sentence = None;
        self.play_sentence(s0, e0);
        self.last_gid = Some(gid);
    }

    /// 连续播放：当前 Text 播完后切下一个 Text 的 A 段整段。无下一个则 false
    fn play_text_whole_next(&mut self, gid: usize) -> bool {
        let gids = self.sorted_gids();
        if let Some(i) = gids.iter().position(|&g| g == gid) {
            if i + 1 < gids.len() {
                let ng = gids[i + 1];
                if let Some(g) = self.groups.iter().find(|g| g.group_id == ng) {
                    self.playing_sentence = Some((ng, 0, 0));
                    self.play_sentence(g.a.0, g.a.1);
                    return true;
                }
            }
        }
        false
    }

    /// 连续播放续读：同一 text 同段的下一句；读完则换下一个 text 的 A 段第一句。返回是否有下一句
    fn play_next_sentence_inner(&mut self, gid: usize, side: u8, sid: usize) -> bool {
        let n = self.gid_sentence_count(gid, side);
        if n > 0 && sid + 1 < n {
            self.play_sentence_id(gid, side, sid + 1);
            return true;
        }
        let gids = self.sorted_gids();
        if let Some(i) = gids.iter().position(|&g| g == gid) {
            if i + 1 < gids.len() {
                self.play_sentence_id(gids[i + 1], 0, 0);
                return true;
            }
        }
        false
    }

    fn play_next_sentence(&mut self) {
        if !self.whole_loaded {
            return;
        }
        let base = self.playing_sentence.or_else(|| self.last_gid.map(|g| (g, 0, usize::MAX)));
        match base {
            Some((gid, side, sid)) => {
                let n = self.gid_sentence_count(gid, side);
                if n > 0 && sid != usize::MAX && sid + 1 < n {
                    self.play_sentence_id(gid, side, sid + 1);
                    return;
                }
                // 到 Text 末尾 -> 下一个 Text 的 A 段第一句
                let gids = self.sorted_gids();
                if let Some(i) = gids.iter().position(|&g| g == gid) {
                    if i + 1 < gids.len() {
                        self.play_sentence_id(gids[i + 1], 0, 0);
                    }
                }
            }
            None => {
                if let Some(&gid) = self.sorted_gids().first() {
                    self.play_sentence_id(gid, 0, 0);
                }
            }
        }
    }

    fn play_prev_sentence(&mut self) {
        if !self.whole_loaded {
            return;
        }
        let base = self.playing_sentence.or_else(|| self.last_gid.map(|g| (g, 0, usize::MAX)));
        match base {
            Some((gid, side, sid)) => {
                if sid != usize::MAX && sid > 0 {
                    self.play_sentence_id(gid, side, sid - 1);
                    return;
                }
                // 到 Text 开头 -> 上一个 Text 同段的最后一句
                let gids = self.sorted_gids();
                if let Some(i) = gids.iter().position(|&g| g == gid) {
                    if i > 0 {
                        let pg = gids[i - 1];
                        let n = self.gid_sentence_count(pg, side);
                        self.play_sentence_id(pg, side, if n > 0 { n - 1 } else { 0 });
                    }
                }
            }
            None => {
                if let Some(&gid) = self.sorted_gids().first() {
                    self.play_sentence_id(gid, 0, 0);
                }
            }
        }
    }

    fn play_next_text(&mut self) {
        if !self.whole_loaded {
            return;
        }
        let gids = self.sorted_gids();
        if gids.is_empty() {
            return;
        }
        let idx = self.last_gid.and_then(|g| gids.iter().position(|&x| x == g));
        match idx {
            Some(i) if i + 1 < gids.len() => self.play_sentence_id(gids[i + 1], 0, 0),
            None => self.play_sentence_id(gids[0], 0, 0),
            Some(_) => {}
        }
    }

    fn play_prev_text(&mut self) {
        if !self.whole_loaded {
            return;
        }
        let gids = self.sorted_gids();
        if gids.is_empty() {
            return;
        }
        let idx = self.last_gid.and_then(|g| gids.iter().position(|&x| x == g));
        match idx {
            Some(i) if i > 0 => self.play_sentence_id(gids[i - 1], 0, 0),
            None => self.play_sentence_id(gids[0], 0, 0),
            Some(_) => {}
        }
    }

    /// 数字键 1-9 / 0(10)：播放对应 Text 并滚动聚焦
    fn play_text_n(&mut self, n: usize) {
        if !self.whole_loaded {
            return;
        }
        let gid = self
            .display_nums
            .iter()
            .find(|(_, v)| **v == n)
            .map(|(g, _)| *g);
        let Some(gid) = gid else {
            self.set_status(format!("没有第 {n} 个 Text"));
            return;
        };
        let side = self.tabs.get(&gid).copied().unwrap_or(0);
        self.play_sentence_id(gid, side, 0);
        self.scroll_to_gid(gid);
    }

    /// 列表滚动到指定 Text 的组头行（行高：AB 组头 64px / C 组头 36px / 句子 28px，间距 2px）
    fn scroll_to_gid(&mut self, gid: usize) {
        let mut y = 0.0f32;
        for row in &self.rows {
            if row.gid as usize == gid && row.kind == 0 {
                break;
            }
            y += row_h(row) + 2.0;
        }
        self.ui.set_scroll_target_y(y);
        self.ui.set_scroll_target_h(if self.groups.iter().any(|g| g.group_id == gid) { 64.0 } else { 36.0 });
        self.bump_scroll();
    }

    /// 递增 scroll-seq：slint 侧 changed 每次都会触发居中滚动
    /// （scroll-target-y 同值重复不触发，且首次触发时布局可能未就绪）
    fn bump_scroll(&mut self) {
        self.ui.set_scroll_seq(self.ui.get_scroll_seq() + 1);
    }

    /// 列表滚动到指定句子行（组头 64/36 / 句子 28 + 间距 2）；无该句子时滚到组头
    fn scroll_to_row(&mut self, gid: usize, sid: usize) {
        let mut y = 0.0f32;
        let mut h = 28.0f32;
        let mut target = None;
        for row in &self.rows {
            if row.kind == 0 && row.gid as usize == gid {
                h = row_h(row);
                target = Some(y);
            } else if row.kind == 1 && row.gid as usize == gid && row.sidx as usize == sid {
                h = 28.0;
                target = Some(y);
                break;
            }
            y += row_h(row) + 2.0;
        }
        if let Some(t) = target {
            self.ui.set_scroll_target_y(t);
            self.ui.set_scroll_target_h(h);
            self.bump_scroll();
        }
    }

    // ---- Ctrl+C / X / A / Shift+I / S：作用于“当前 Text” ----

    fn copy_current_audio(&mut self) {
        let Some(gid) = self.current_gid() else {
            self.set_status("未播放任何 Text");
            return;
        };
        let side = self.tabs.get(&gid).copied().unwrap_or(0);
        self.copy_audio(gid, side);
    }

    fn copy_current_text(&mut self) {
        let Some(gid) = self.current_gid() else {
            self.set_status("未播放任何 Text");
            return;
        };
        let side = self.tabs.get(&gid).copied().unwrap_or(0);
        self.copy_text(gid, side);
    }

    fn export_current(&mut self) {
        let Some(gid) = self.current_gid() else {
            self.set_status("未播放任何 Text");
            return;
        };
        let side = self.tabs.get(&gid).copied().unwrap_or(0);
        self.export_group(gid, side);
    }

    fn select_all_current(&mut self) {
        let Some(gid) = self.current_gid() else {
            self.set_status("未播放任何 Text");
            return;
        };
        let side = self.tabs.get(&gid).copied().unwrap_or(0);
        let n = self.gid_sentence_count(gid, side);
        if n == 0 {
            self.set_status(format!("Text{} {}段 尚未转写", self.disp_num(gid), if side == 1 { "B" } else { "A" }));
            return;
        }
        let all_checked = (0..n).all(|i| self.checked.contains(&(gid, side, i)));
        for i in 0..n {
            if all_checked {
                self.checked.remove(&(gid, side, i));
            } else {
                self.checked.insert((gid, side, i));
            }
        }
        self.rebuild_rows();
        self.set_status(if all_checked {
            format!("已取消全选 Text{}", self.disp_num(gid))
        } else {
            format!("已全选 Text{}", self.disp_num(gid))
        });
    }

    /// 取消全选当前 Text 当前段
    fn select_none_current(&mut self) {
        let Some(gid) = self.current_gid() else {
            self.set_status("未播放任何 Text");
            return;
        };
        let side = self.tabs.get(&gid).copied().unwrap_or(0);
        let n = self.gid_sentence_count(gid, side);
        if n == 0 {
            self.set_status(format!("Text{} {}段 尚未转写", self.disp_num(gid), if side == 1 { "B" } else { "A" }));
            return;
        }
        for i in 0..n {
            self.checked.remove(&(gid, side, i));
        }
        self.rebuild_rows();
        self.set_status(format!("已取消全选 Text{}", self.disp_num(gid)));
    }

    fn invert_current(&mut self) {
        let Some(gid) = self.current_gid() else {
            self.set_status("未播放任何 Text");
            return;
        };
        let side = self.tabs.get(&gid).copied().unwrap_or(0);
        let n = self.gid_sentence_count(gid, side);
        if n == 0 {
            self.set_status(format!("Text{} {}段 尚未转写", self.disp_num(gid), if side == 1 { "B" } else { "A" }));
            return;
        }
        for i in 0..n {
            if self.checked.contains(&(gid, side, i)) {
                self.checked.remove(&(gid, side, i));
            } else {
                self.checked.insert((gid, side, i));
            }
        }
        self.rebuild_rows();
        self.set_status(format!("已反选 Text{}", self.disp_num(gid)));
    }

    fn play_sentence(&mut self, s: f64, e: f64) {
        if self.total_duration <= 0.0 {
            return;
        }
        // 点击正在播放的同一句：不要从头重复，保持当前播放
        if self.player.is_playing() && self.slice_range == Some((s, e)) {
            return;
        }
        self.player.seek((s / self.total_duration) as f32);
        self.slice_range = Some((s, e));
        self.selected = None;
        self.rebuild_rows();
        self.player.play();
    }

    // ============================================================
    // 进度条
    // ============================================================

    fn bar_drag(&mut self, ratio: f32, phase: i32) {
        match phase {
            0 => {
                self.dragging = true;
                self.seek_ratio(ratio);
            }
            1 => {
                if self.dragging {
                    self.seek_ratio(ratio);
                }
            }
            _ => {
                self.dragging = false;
            }
        }
    }

    fn seek_ratio(&mut self, ratio: f32) {
        if !self.whole_loaded {
            return;
        }
        self.player.seek(ratio);
        self.slice_range = None;
        self.just_seeked = true;
    }

    /// 点击句子行时间点：跳转到该句开始（which=0）/结束（which=1）时间，高亮并聚焦该句
    fn seek_time(&mut self, uidx: usize, which: i32) {
        if !self.whole_loaded {
            return;
        }
        let Some(row) = self.rows.get(uidx).cloned() else { return };
        let t = if which == 0 {
            row.a0 as f64
        } else {
            row.a1 as f64
        };
        if self.total_duration <= 0.0 {
            return;
        }
        self.player.seek((t / self.total_duration) as f32);
        self.slice_range = None;
        if row.kind == 1 {
            self.playing_sentence = Some((row.gid as usize, row.side as u8, row.sidx as usize));
        } else {
            // 组头 / 转写中占位行：整段区间
            self.playing_sentence = Some((row.gid as usize, row.side as u8, 0));
        }
        self.last_gid = Some(row.gid as usize);
        self.just_seeked = true;
        self.scroll_to_gid(row.gid as usize);
        self.rebuild_rows();
    }

    fn bar_click(&mut self, ratio: f32) {
        // 取消吸附：点击进度条只跳转播放位置，不吸附到最近的识别色带
        self.seek_ratio(ratio);
    }

    fn bar_dbl(&mut self, ratio: f32) {
        if !self.whole_loaded {
            return;
        }
        let t = ratio as f64 * self.total_duration;
        if let Some((gid, side, s, e)) = self.nearest_band(t, self.total_duration * 0.02) {
            self.selected = Some((gid, side));
            self.rebuild_rows();
            self.copy_segment_wav(gid, side, s, e);
        }
    }

    /// 在时间轴上找离 t 最近的重复片段起点（含 A/B 两端与 C 段），radius 秒内才命中
    fn nearest_band(&self, t: f64, radius: f64) -> Option<(usize, u8, f64, f64)> {
        let mut cands: Vec<(f64, f64, usize, u8)> = self
            .groups
            .iter()
            .flat_map(|g| {
                [
                    (g.a.0, g.a.1, g.group_id, 0u8),
                    (g.b.0, g.b.1, g.group_id, 1u8),
                ]
            })
            .collect();
        for &(gid, s, e) in &self.c_items {
            cands.push((s, e, gid, 0u8));
        }
        cands
            .into_iter()
            .min_by(|x, y| (x.0 - t).abs().partial_cmp(&(y.0 - t).abs()).unwrap())
            .filter(|b| (b.0 - t).abs() <= radius)
            .map(|(s, e, gid, side)| (gid, side, s, e))
    }

    /// 点击落在某段 A/B/C 识别区域（±eps 容差）内时命中，返回 (gid, side, 起点, 终点)；
    /// 多个候选重叠时取中点距点击位置最近的一段（A/B 相邻时选直觉上更近的）
    fn band_hit(&self, t: f64, eps: f64) -> Option<(usize, u8, f64, f64)> {
        let mut cands: Vec<(f64, f64, usize, u8)> = self
            .groups
            .iter()
            .flat_map(|g| {
                [
                    (g.a.0, g.a.1, g.group_id, 0u8),
                    (g.b.0, g.b.1, g.group_id, 1u8),
                ]
            })
            .collect();
        for &(gid, s, e) in &self.c_items {
            cands.push((s, e, gid, 0u8));
        }
        cands
            .into_iter()
            .map(|(s, e, gid, side)| (s, e, gid, side, (s + e) / 2.0))
            .filter(|(s, e, _, _, _)| t >= *s - eps && t <= *e + eps)
            .min_by(|x, y| (x.4 - t).abs().partial_cmp(&(y.4 - t).abs()).unwrap())
            .map(|(s, e, gid, side, _)| (gid, side, s, e))
    }

    /// 点击波形图：落在识别色带上则选中该段并从段开头播放（循环播放该段）
    fn wave_click(&mut self, ratio: f32) {
        if !self.whole_loaded {
            return;
        }
        let t = ratio as f64 * self.total_duration;
        let Some((gid, side, s, e)) = self.band_hit(t, self.total_duration * 0.02) else {
            return;
        };
        self.selected = Some((gid, side));
        self.rebuild_rows();
        self.player.seek((s / self.total_duration.max(1e-9)) as f32);
        self.slice_range = Some((s, e));
        self.player.play();
        self.last_gid = Some(gid);
        // 波段播放不参与“连续播放”续读
        self.playing_sentence = None;
        let is_c = self.c_items.iter().any(|&(g2, _, _)| g2 == gid);
        let tag = if is_c { "C" } else if side == 0 { "A" } else { "B" };
        self.set_status(format!("播放 T{} {tag}（{}–{}）", self.disp_num(gid), fmt_ts(s), fmt_ts(e)));
    }

    /// 频谱控件宽度变化：按像素宽度重新生成波形列（从基础峰值池化/插值）
    fn wave_resized(&mut self, width_px: usize) {
        if self.peaks_base.is_empty() {
            return;
        }
        let cols = width_px.clamp(64, 3200);
        if (cols as i32 - self.peaks_len as i32).abs() < 8 {
            return;
        }
        self.peaks_len = cols;
        let peaks = pool_peaks(&self.peaks_base, cols);
        self.ui
            .set_wave_peaks(ModelRc::from(Rc::new(VecModel::from(peaks))));
    }

    // ============================================================
    // 列表交互
    // ============================================================

    fn select_side(&mut self, gid: usize, side: u8) {
        self.selected = Some((gid, side));
        self.rebuild_rows();
    }

    fn toggle_checked(&mut self, uidx: usize) {
        let Some(row) = self.rows.get(uidx) else { return };
        let gid = row.gid as usize;
        let side = row.side as u8;
        let key = if row.kind == 1 {
            (gid, side, row.sidx as usize)
        } else {
            (gid, row.tab as u8, usize::MAX)
        };
        if self.checked.contains(&key) {
            self.checked.remove(&key);
        } else {
            self.checked.insert(key);
        }
        self.rebuild_rows();
    }

    fn play_sentence_row(&mut self, uidx: usize) {
        let Some(row) = self.rows.get(uidx) else { return };
        self.last_gid = Some(row.gid as usize);
        if row.kind == 1 {
            self.playing_sentence = Some((row.gid as usize, row.side as u8, row.sidx as usize));
        } else {
            self.playing_sentence = None;
        }
        self.play_sentence(row.a0 as f64, row.a1 as f64);
    }

    fn copy_text(&mut self, gid: usize, side: u8) {
        let segs = if side == 1 {
            self.asr_results_b.get(&gid)
        } else {
            self.asr_results.get(&gid)
        };
        if let Some(segs) = segs {
            let text: Vec<String> = segs
                .iter()
                .enumerate()
                .filter(|(i, _)| self.checked.contains(&(gid, side, *i)))
                .map(|(_, seg)| seg.text.clone())
                .collect();
            if !text.is_empty() {
                match clipboard::set_text(&text.join("\r\n")) {
                    Ok(()) => self.set_status(format!("已复制 Text{} 选中文本", self.disp_num(gid))),
                    Err(e) => self.set_status(format!("复制文本失败: {e:#}")),
                }
            } else {
                self.set_status(format!("Text{} 未勾选任何句子", self.disp_num(gid)));
            }
        }
    }

    /// 某一组内被勾选句子按"连续段"分组：相邻勾选(idx 连续)合成 [首,尾]
    fn checked_runs_in_group(&self, gid: usize, side: u8) -> Vec<(f64, f64)> {
        let span = if let Some(g) = self.groups.iter().find(|g| g.group_id == gid) {
            if side == 1 { g.b } else { g.a }
        } else if let Some(&(_, s, e)) = self.c_items.iter().find(|&&(g2, _, _)| g2 == gid) {
            (s, e)
        } else {
            return Vec::new();
        };
        if self.checked.contains(&(gid, side, usize::MAX)) {
            return vec![span];
        }
        let segs = if side == 1 {
            self.asr_results_b.get(&gid)
        } else {
            self.asr_results.get(&gid)
        };
        let Some(segs) = segs else { return vec![span] };
        let mut runs: Vec<(f64, f64)> = Vec::new();
        let mut cur_start: Option<f64> = None;
        let mut cur_end = 0.0f64;
        for (i, seg) in segs.iter().enumerate() {
            if !self.checked.contains(&(gid, side, i)) {
                if let Some(s0) = cur_start.take() {
                    runs.push((s0, cur_end));
                }
                continue;
            }
            let s = span.0 + seg.start;
            let e = span.0 + seg.end;
            match cur_start {
                None => {
                    cur_start = Some(s);
                    cur_end = e;
                }
                Some(_) => {
                    cur_end = e;
                }
            }
        }
        if let Some(s0) = cur_start.take() {
            runs.push((s0, cur_end));
        }
        runs
    }

    fn copy_segment_wav(&mut self, gid: usize, _side: u8, s: f64, e: f64) {
        if self.audio.is_none() {
            self.set_status("音频未加载");
            return;
        }
        let is_c = self.c_items.iter().any(|&(g2, _, _)| g2 == gid);
        let tag = if is_c { "C" } else if _side == 0 { "A" } else { "B" };
        let dnum = self.disp_num(gid);
        self.set_status(format!("正在复制 T{dnum} 片段…"));
        let name = format!("echodup_T{dnum}_{tag}_{:.1}-{:.1}.wav", s, e);
        self.spawn_clip(vec![(s, e)], name, None);
    }

    fn copy_audio(&mut self, gid: usize, side: u8) {
        if self.audio.is_none() {
            self.set_status("音频未加载");
            return;
        }
        let runs = self.checked_runs_in_group(gid, side);
        if runs.is_empty() {
            self.set_status("未勾选任何句子");
            return;
        }
        let dnum = self.disp_num(gid);
        self.set_status(format!("正在复制 T{dnum} …"));
        self.spawn_clip(runs, format!("echodup_text{dnum}_copy.wav"), None);
    }

    fn export_group(&mut self, gid: usize, side: u8) {
        let runs = self.checked_runs_in_group(gid, side);
        if runs.is_empty() {
            self.set_status("未勾选任何句子");
            return;
        }
        let name = format!("echodup_text{}_sel.wav", self.disp_num(gid));
        self.pending_export = Some(ExportJob {
            kind: ExportKind::Group(gid),
            name: name.clone(),
            ranges: runs,
        });
        self.request_save_path(&name);
    }

    fn export_checked(&mut self) {
        let Some(_audio) = &self.audio else { return };
        let mut items: Vec<(f64, f64)> = self
            .groups
            .iter()
            .flat_map(|g| {
                let a_segs = self
                    .asr_results
                    .get(&g.group_id)
                    .map(|segs| {
                        segs.iter()
                            .enumerate()
                            .filter(|(i, _)| self.checked.contains(&(g.group_id, 0, *i)))
                            .map(move |(_, seg)| (g.a.0 + seg.start, g.a.0 + seg.end))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let b_segs = self
                    .asr_results_b
                    .get(&g.group_id)
                    .map(|segs| {
                        segs.iter()
                            .enumerate()
                            .filter(|(i, _)| self.checked.contains(&(g.group_id, 1, *i)))
                            .map(move |(_, seg)| (g.b.0 + seg.start, g.b.0 + seg.end))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                a_segs.into_iter().chain(b_segs.into_iter()).collect::<Vec<_>>()
            })
            .chain(self.c_items.iter().flat_map(|&(gid, s0, _)| {
                self.asr_results
                    .get(&gid)
                    .map(|segs| {
                        segs.iter()
                            .enumerate()
                            .filter(|(i, _)| self.checked.contains(&(gid, 0, *i)))
                            .map(move |(_, seg)| (s0 + seg.start, s0 + seg.end))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            }))
            .collect();
        items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        if items.is_empty() {
            self.set_status("未勾选任何句子");
            return;
        }
        let name = format!("echodup_selected_{}ju.wav", items.len());
        self.pending_export = Some(ExportJob {
            kind: ExportKind::Checked,
            name: name.clone(),
            ranges: items,
        });
        self.request_save_path(&name);
    }

    /// 弹保存对话框（后台线程），选定后走 poll_save -> spawn_export
    fn request_save_path(&mut self, name: &str) {
        let (tx, rx) = channel();
        self.save_rx = Some(rx);
        let name = name.to_string();
        std::thread::spawn(move || {
            let p = rfd::FileDialog::new()
                .set_file_name(&name)
                .add_filter("WAV", &["wav"])
                .save_file();
            let _ = tx.send(p);
        });
    }

    /// 启动 复制/导出 子进程（out_path 为 None = 复制到剪贴板）
    fn spawn_clip(&mut self, ranges: Vec<(f64, f64)>, name: String, out_path: Option<PathBuf>) {
        let Some(cache_wav) = self.analysis_files.as_ref().map(|f| f.cache_wav.clone()) else {
            self.set_status("缺少音频缓存");
            return;
        };
        let role: &'static str = if out_path.is_some() { "export" } else { "copy" };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let payload = self.temp_dir.join(format!("{role}_{nanos}.json"));
        let req = serde_json::json!({
            "cache_wav": cache_wav.to_string_lossy(),
            "ranges": ranges,
            "gap_sec": 0.15,
            "speed": self.ui.get_speed(),
            "name": name,
            "out_path": out_path.map(|p| p.to_string_lossy().to_string()),
        });
        let Ok(bytes) = serde_json::to_vec(&req) else {
            self.set_status("内部错误：载荷序列化失败");
            return;
        };
        if let Err(e) = std::fs::write(&payload, bytes) {
            self.set_status(format!("写载荷失败: {e}"));
            return;
        }
        match Worker::spawn(role, &payload) {
            Ok(w) => self.copy_workers.push(w),
            Err(e) => self.set_status(format!("启动进程失败: {e}")),
        }
    }

    fn spawn_export(&mut self, job: ExportJob, out_path: PathBuf) {
        if self.audio.is_none() {
            self.set_status("音频未加载");
            return;
        }
        self.set_status(format!("正在导出 {} …", job.name));
        self.spawn_clip(job.ranges, job.name, Some(out_path));
    }

    // ============================================================
    // 行模型
    // ============================================================

    fn rebuild_rows(&mut self) {
        let mut rows: Vec<RowData> = Vec::new();
        let mut uidx = 0usize;
        // 按出现顺序渲染：AB 组与 C 段按起点时间合并排序，列表顺序与编号（Text 1..N）一致
        let mut entries: Vec<(f64, usize, u8)> = self
            .groups
            .iter()
            .map(|g| (g.a.0, g.group_id, 0u8))
            .chain(self.c_items.iter().map(|&(gid, s, _)| (s, gid, 1u8)))
            .collect();
        entries.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        for &(_, gid, is_c) in &entries {
            if is_c == 0 {
                // ---- AB 重复组 ----
                let Some(g) = self.groups.iter().find(|g| g.group_id == gid) else {
                    continue;
                };
                let tab = self.tabs.get(&gid).copied().unwrap_or(0);
                let a_ready = self.asr_results.contains_key(&gid);
                let b_ready = self.asr_results_b.contains_key(&gid);
                rows.push(RowData {
                    uidx: uidx as i32,
                    kind: 0,
                    gid: gid as i32,
                    sidx: -1,
                    a0: g.a.0 as f32,
                    a1: g.a.1 as f32,
                    b0: g.b.0 as f32,
                    b1: g.b.1 as f32,
                    conf: g.confidence,
                    text: SharedString::default(),
                    checked: false,
                    sel_a: self.selected == Some((gid, 0)),
                    sel_b: self.selected == Some((gid, 1)),
                    is_c: false,
                    disp: self.disp_num(gid) as i32,
                    tab: tab as i32,
                    side: 0,
                    a_ready: a_ready,
                    b_ready: b_ready,
                });
                uidx += 1;
                // 当前 tab 段（A 或 B）的句子
                let (span0, span1, segs) = if tab == 1 {
                    (g.b.0, g.b.1, self.asr_results_b.get(&gid))
                } else {
                    (g.a.0, g.a.1, self.asr_results.get(&gid))
                };
                if let Some(segs) = segs {
                    for (i, seg) in segs.iter().enumerate() {
                        rows.push(RowData {
                            uidx: uidx as i32,
                            kind: 1,
                            gid: gid as i32,
                            sidx: i as i32,
                            a0: (span0 + seg.start) as f32,
                            a1: (span0 + seg.end) as f32,
                            b0: g.b.0 as f32,
                            b1: g.b.1 as f32,
                            conf: 0.0,
                            text: seg.text.clone().into(),
                            checked: self.checked.contains(&(gid, tab, i)),
                            sel_a: false,
                            sel_b: false,
                            is_c: false,
                            disp: self.disp_num(gid) as i32,
                            tab: tab as i32,
                            side: tab as i32,
                            a_ready: a_ready,
                            b_ready: b_ready,
                        });
                        uidx += 1;
                    }
                } else {
                    rows.push(RowData {
                        uidx: uidx as i32,
                        kind: 2,
                        gid: gid as i32,
                        sidx: -1,
                        a0: span0 as f32,
                        a1: span1 as f32,
                        b0: g.b.0 as f32,
                        b1: g.b.1 as f32,
                        conf: 0.0,
                        text: SharedString::default(),
                        checked: self.checked.contains(&(gid, tab, usize::MAX)),
                        sel_a: false,
                        sel_b: false,
                        is_c: false,
                        disp: self.disp_num(gid) as i32,
                        tab: tab as i32,
                        side: tab as i32,
                        a_ready: a_ready,
                        b_ready: b_ready,
                    });
                    uidx += 1;
                }
            } else {
                // ---- C 段（不重复段）：组头 + 句子行（与 AB 组同构，整段直接 ASR） ----
                let Some(&(_, s, e)) = self.c_items.iter().find(|&&(g2, _, _)| g2 == gid) else {
                    continue;
                };
                let c_ready = self.asr_results.contains_key(&gid);
                rows.push(RowData {
                    uidx: uidx as i32,
                    kind: 0,
                    gid: gid as i32,
                    sidx: -1,
                    a0: s as f32,
                    a1: e as f32,
                    b0: 0.0,
                    b1: 0.0,
                    conf: 0.0,
                    text: SharedString::default(),
                    checked: false,
                    sel_a: self.selected == Some((gid, 0)),
                    sel_b: false,
                    is_c: true,
                    disp: self.disp_num(gid) as i32,
                    tab: 0,
                    side: 0,
                    a_ready: c_ready,
                    b_ready: false,
                });
                uidx += 1;
                if let Some(segs) = self.asr_results.get(&gid) {
                    for (i, seg) in segs.iter().enumerate() {
                        rows.push(RowData {
                            uidx: uidx as i32,
                            kind: 1,
                            gid: gid as i32,
                            sidx: i as i32,
                            a0: (s + seg.start) as f32,
                            a1: (s + seg.end) as f32,
                            b0: 0.0,
                            b1: 0.0,
                            conf: 0.0,
                            text: seg.text.clone().into(),
                            checked: self.checked.contains(&(gid, 0, i)),
                            sel_a: false,
                            sel_b: false,
                            is_c: false,
                            disp: self.disp_num(gid) as i32,
                            tab: 0,
                            side: 0,
                            a_ready: c_ready,
                            b_ready: false,
                        });
                        uidx += 1;
                    }
                } else {
                    rows.push(RowData {
                        uidx: uidx as i32,
                        kind: 2,
                        gid: gid as i32,
                        sidx: -1,
                        a0: s as f32,
                        a1: e as f32,
                        b0: 0.0,
                        b1: 0.0,
                        conf: 0.0,
                        text: SharedString::default(),
                        checked: self.checked.contains(&(gid, 0, usize::MAX)),
                        sel_a: false,
                        sel_b: false,
                        is_c: false,
                        disp: self.disp_num(gid) as i32,
                        tab: 0,
                        side: 0,
                        a_ready: c_ready,
                        b_ready: false,
                    });
                    uidx += 1;
                }
            }
        }
        self.rows = rows.clone();
        self.ui
            .set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
        self.ui.set_checked_count(self.checked.len() as i32);
    }

    // ============================================================
    // 清理
    // ============================================================

    fn shutdown(&mut self) {
        if let Some(mut w) = self.analyze.take() {
            w.kill();
        }
        if let Some(mut w) = self.asr_worker.take() {
            w.kill();
        }
        for w in self.copy_workers.iter_mut() {
            w.kill();
        }
        self.copy_workers.clear();
        // 注销文件拖拽目标
        drop::revoke(&mut self.drop_target);
        if let Some(f) = &self.analysis_files {
            let _ = std::fs::remove_file(&f.cache_wav);
            let _ = std::fs::remove_file(&f.result_json);
            let _ = std::fs::remove_file(&f.payload);
        }
    }
}

// ============================================================
// 波形渲染（纯 CPU，输出 RGBA）
// ============================================================

/// 波形峰值数组（1600 列≈每像素一列，归一化 0..1），供 Slint 原生绘制（颜色随主题）
fn wave_peaks(mono: &[f32], cols: usize) -> Vec<f32> {
    let n = mono.len();
    if n == 0 {
        return vec![0.0; cols];
    }
    let step = ((n as f64) / cols as f64).max(1.0) as usize;
    let mut out = Vec::with_capacity(cols);
    for x in 0..cols {
        let s = x as usize * step;
        let e = ((x as usize + 1) * step).min(n);
        let mut peak = 0.0f32;
        for &v in &mono[s..e] {
            peak = peak.max(v.abs());
        }
        out.push(peak);
    }
    let max = out.iter().cloned().fold(0.0f32, f32::max).max(1e-6);
    for v in out.iter_mut() {
        *v = (*v / max).min(1.0);
    }
    out
}

/// 从基础峰值池化/插值到目标列数（cols <= base 取 max 池化，否则重复插值）
fn pool_peaks(base: &[f32], cols: usize) -> Vec<f32> {
    let b = base.len();
    if b == 0 || cols == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(cols);
    if cols <= b {
        let seg = b as f32 / cols as f32;
        for c in 0..cols {
            let s = (c as f32 * seg).floor() as usize;
            let e = (((c as f32 + 1.0) * seg).ceil() as usize).min(b).max(s + 1);
            let mut m = 0.0f32;
            for &v in &base[s..e] {
                m = m.max(v);
            }
            out.push(m);
        }
    } else {
        let seg = cols as f32 / b as f32;
        for c in 0..cols {
            let src = ((c as f32 + 0.5) / seg) as usize;
            out.push(base[src.min(b - 1)]);
        }
    }
    out
}

// ============================================================
// 工具
// ============================================================

/// 行高：AB 组头 64px / C 组头 36px / 句子与占位 28px（与 slint 渲染一致）
fn row_h(row: &RowData) -> f32 {
    if row.kind == 0 {
        if row.is_c {
            36.0
        } else {
            64.0
        }
    } else {
        28.0
    }
}

/// 秒 → “分:秒”显示（0.1s 精度；79.1 -> 1:19.1，600 -> 10:00）
fn fmt_ts(v: f64) -> String {
    let v = v.max(0.0);
    let tenths = (v * 10.0).round() as u64;
    let m = tenths / 600;
    let rem = tenths % 600;
    let whole = rem / 10;
    let frac = rem % 10;
    if frac > 0 {
        format!("{m}:{whole:02}.{frac}")
    } else {
        format!("{m}:{whole:02}")
    }
}

/// 倍速显示：去掉浮点累计误差（如 1.3000002 -> 1.3）
fn format_speed(v: f32) -> String {
    format!("{}", (v * 100.0).round() / 100.0)
}

/// 从原文取 Text N 块（模块级，供后台线程使用；N 为出现顺序编号，与原文标号一致）
fn block_for(transcript: &str, n: usize) -> String {
    if transcript.trim().is_empty() {
        return String::new();
    }
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for (i, line) in transcript.split('\n').enumerate() {
        let t = line.trim().trim_end_matches(':');
        let t = t.trim();
        let lower = t.to_lowercase();
        if let Some(rest) = lower.strip_prefix("text") {
            if let Ok(num) = rest.trim().parse::<usize>() {
                blocks.push((num, i));
            }
        }
    }
    if blocks.is_empty() {
        return transcript.trim().to_string();
    }
    let pos = match blocks.binary_search_by_key(&n, |(num, _)| *num) {
        Ok(p) => p,
        Err(_) => return String::new(),
    };
    let lines: Vec<&str> = transcript.split('\n').collect();
    let start = blocks[pos].1 + 1;
    let end = blocks.get(pos + 1).map(|(_, l)| *l).unwrap_or(lines.len());
    lines[start..end].join("\n").trim().to_string()
}

// ============================================================
// 入口
// ============================================================

pub fn run(initial: Option<PathBuf>) {
    let ui = match AppWindow::new() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("创建窗口失败: {e}");
            return;
        }
    };
    // 无边框窗口（slint_borderless_windows）：移除系统标题栏，窗口控制走 Rust WindowFrame
    use slint_borderless_windows::TitlebarSetup;
    let frame = ui.as_weak().setup_borderless().ok();
    if let Some(frame) = &frame {
        let f = frame.clone();
        ui.on_sys_minimize(move || f.minimize());
        let f = frame.clone();
        let ui_max = ui.as_weak();
        ui.on_sys_maximize(move || {
            f.toggle_maximized();
            if let Some(u) = ui_max.upgrade() {
                u.set_maximized(u.window().is_maximized());
            }
        });
        let f = frame.clone();
        ui.on_sys_close(move || f.close());
        let f = frame.clone();
        ui.on_titlebar_drag(move || f.drag());
        let f = frame.clone();
        let ui_dbl = ui.as_weak();
        ui.on_titlebar_double(move || {
            f.toggle_maximized();
            if let Some(u) = ui_dbl.upgrade() {
                u.set_maximized(u.window().is_maximized());
            }
        });
    }

    fit_window_to_work_area(&ui);
    // 启动后设置前台窗口：避免无边框窗口首次点击被窗口激活消费（首次点击菜单按钮无效）
    std::thread::spawn(|| {
        use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetWindowThreadProcessId, SetForegroundWindow,
        };
        std::thread::sleep(std::time::Duration::from_millis(400));
        let pid = std::process::id() as isize;
        unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let mut wpid: u32 = 0;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut wpid)); }
            if wpid as isize == lparam.0 {
                unsafe { SetForegroundWindow(hwnd); }
                BOOL(0)
            } else {
                BOOL(1)
            }
        }
        unsafe {
            EnumWindows(Some(enum_proc), LPARAM(pid));
        }
    });
    let app = Rc::new(RefCell::new(App::new(ui.clone_strong(), initial)));
    App::wire_up(app.clone());

    if let Err(e) = ui.run() {
        eprintln!("窗口事件循环异常: {e}");
    }
    app.borrow_mut().shutdown();
}

/// 启动时按屏幕工作区自适应窗口大小（按 DPI 放大渲染）。
/// GetSystemMetrics 返回 96-DPI 单位的工作区（≈逻辑尺寸）。
/// 关键：探测 winit 的物理换算系数 c（初始窗口物理宽 / 设计逻辑宽 1200）。
/// 本环境 winit 建窗≈1x（c≈1），正常机器 winit≈scale（c≈s）；用
/// 请求尺寸 = 目标逻辑视口 × s / c 可同时兼容两种，保证视口恰好=工作区。
/// 特例：验证构建强制 SLINT_SCALE_FACTOR=1.0（s==1）时，winit 仍按系统 DPI
/// 放大 Logical 请求（物理=逻辑×DPI），因此直接请求 Physical，避免逻辑视口被撑大。
#[cfg(windows)]
fn fit_window_to_work_area(ui: &AppWindow) {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SYSTEM_METRICS_INDEX};
    // SM_CXWORKAREA = 0x30, SM_CYWORKAREA = 0x31（0.58 绑定缺这两个常量，按标准值构造）
    let wa_w = unsafe { GetSystemMetrics(SYSTEM_METRICS_INDEX(0x0030)) } as f32;
    let wa_h = unsafe { GetSystemMetrics(SYSTEM_METRICS_INDEX(0x0031)) } as f32;
    let s = ui.window().scale_factor().max(1.0);
    let target_w = wa_w.min(1200.0);
    let target_h = wa_h.min(800.0);
    if s <= 1.01 {
        ui.window().set_size(slint::WindowSize::Physical(slint::PhysicalSize {
            width: target_w as u32,
            height: target_h as u32,
        }));
    } else {
        let c = (ui.window().size().width as f32 / 1200.0).clamp(1.0, s);
        let w = target_w * s / c;
        let h = target_h * s / c;
        ui.window().set_size(slint::WindowSize::Logical(slint::LogicalSize { width: w, height: h }));
    }
}
