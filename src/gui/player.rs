use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use timestretch::engine::{
    Engine, EngineConfig, EngineController, EngineProfile, SourceProducer,
};

/// 实时 timestretch 播放器（timestretch 0.15 engine，WideKeylock 变速不变调）。
///
/// 三件套架构（官方实时引擎设计）：
/// - [`EngineController`]（控制线程/UI）：`set_tempo_rate()` 即时改倍速（下一块边界生效、
///   内部平滑渐变，不再有整段预拉伸的卡顿与“进度条突然满”）；`source_position()` 读进度。
/// - [`SourceProducer`]（喂音频线程——App 每 30ms tick 调 `feed()`）：把原始单声道样本推入
///   无锁 ring，供处理器消费；ring 跑空时输出静音并计数，绝不阻塞、绝不失败。
/// - [`EngineProcessor`]（cpal 回调内）：`process(&mut out)` 无分配、无锁，填满设备帧。
///
/// profile 选 WideKeylock：全频谱 keylock（0.25–2.0 全程变速不变调）、0 延迟，第一个输出
/// 帧就是源帧 0 —— 听力重复场景最合适。
///
/// 进度语义：engine 的 `source_position()` 是相对最近一次 reset 的 fed-source 坐标；
/// Player 用 `anchor_frame`（load/seek/回绕时的绝对轨道帧）与之相加得到绝对进度。
const MONO_CAP: usize = 8192; // 设备单次回调帧数上限（预分配，防回调内分配）
const FEED_CHUNK: usize = 8192; // 每次 feed 最多推入的源帧数
const BLOCK_REF: usize = 1024; // demand_hint 的参考回调块大小

/// engine 硬支持范围（timestretch MIN_TEMPO_RATE..=MAX_TEMPO_RATE）
const SPEED_MIN: f64 = 0.25;
const SPEED_MAX: f64 = 4.0;

struct Shared {
    playing: AtomicBool,
    looping: AtomicBool,
    /// 回调检测到即 `processor.reset()`（load / seek / stop / 循环回绕后置位）
    reset_pending: AtomicBool,
}

pub struct Player {
    stream: Option<cpal::Stream>,
    controller: Option<EngineController>,
    source: Option<SourceProducer>,
    shared: Arc<Shared>,
    /// 单声道原始样本（已按需重采样到设备采样率）
    src: Option<Arc<Vec<f32>>>,
    /// 实际播放采样率（= 设备采样率，engine 构建采样率）
    src_sr: u32,
    /// 已推入 ring 的源帧游标
    feed_pos: usize,
    /// 当前绝对轨道锚定帧（load=0 / seek=目标 / 回绕=0）
    anchor_frame: u64,
    /// 当前生效倍速（clamp 到 engine 范围）
    speed: f64,
    device_channels: usize,
}

impl Player {
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            playing: AtomicBool::new(false),
            looping: AtomicBool::new(false),
            reset_pending: AtomicBool::new(false),
        });

        let host = cpal::default_host();
        let device = host.default_output_device();
        let mut device_sr = 44100u32;
        let mut device_channels = 2usize;
        let mut controller = None;
        let mut source = None;

        let stream = device.and_then(|dev| {
            let cfg = dev.default_output_config().ok()?;
            device_sr = cfg.sample_rate().0;
            device_channels = cfg.channels() as usize;
            let channels = cfg.channels() as usize;

            let handles = Engine::build(EngineConfig {
                sample_rate: device_sr,
                channels: 1,
                profile: EngineProfile::WideKeylock,
                initial_tempo_rate: 1.0,
                max_block_frames: 1024,
                source_capacity_frames: 32_768,
                pre_analysis: None,
            })
            .ok()?;

            controller = Some(handles.controller);
            source = Some(handles.source);

            let shared_cb = shared.clone();
            let mut processor = handles.processor;
            let mut mono = vec![0.0f32; MONO_CAP];

            let stream = dev
                .build_output_stream(
                    &cfg.config(),
                    move |data: &mut [f32], _| {
                        // reset 请求无条件执行（暂停/停止态下的 seek/stop 也要归位进度）
                        if shared_cb.reset_pending.swap(false, Ordering::Relaxed) {
                            processor.reset();
                        }
                        if !shared_cb.playing.load(Ordering::Relaxed) {
                            data.fill(0.0);
                            return;
                        }
                        let frames = (data.len() / channels.max(1)).min(MONO_CAP);
                        processor.process(&mut mono[..frames]);
                        for (i, v) in mono[..frames].iter().enumerate() {
                            let base = i * channels;
                            for c in 0..channels {
                                data[base + c] = *v;
                            }
                        }
                    },
                    |e| eprintln!("cpal error: {e}"),
                    None,
                )
                .ok()?;
            stream.play().ok()?;
            Some(stream)
        });

        Self {
            stream,
            controller,
            source,
            shared,
            src: None,
            src_sr: device_sr,
            feed_pos: 0,
            anchor_frame: 0,
            speed: 1.0,
            device_channels,
        }
    }

    /// 装载音频（单声道）。采样率与设备不一致时线性重采样到设备采样率，
    /// 保证播放速度/音高正确；随后复位到文件头。
    pub fn load(&mut self, samples: &[f32], sample_rate: u32, speed: f32) {
        let device_sr = self.src_sr;
        let resampled = if sample_rate != device_sr {
            resample_linear(samples, sample_rate, device_sr)
        } else {
            samples.to_vec()
        };
        self.src = Some(Arc::new(resampled));
        self.feed_pos = 0;
        self.anchor_frame = 0;
        self.speed = clamp_speed(speed as f64);
        self.shared.playing.store(false, Ordering::Relaxed);
        self.shared.reset_pending.store(true, Ordering::Relaxed);
        if let Some(s) = &mut self.source {
            s.set_track_position(0);
        }
        if let Some(c) = &self.controller {
            c.set_tempo_rate(self.speed);
        }
        self.feed();
    }

    /// 切换倍速：只写控制邮箱，下一块边界即生效（0.25–4.0，engine 硬范围）。
    pub fn set_speed(&mut self, speed: f32) {
        let s = clamp_speed(speed as f64);
        if (self.speed - s).abs() < 1e-6 {
            return;
        }
        self.speed = s;
        if let Some(c) = &self.controller {
            c.set_tempo_rate(s);
        }
    }

    pub fn play(&mut self) {
        let len = self.src.as_ref().map(|v| v.len()).unwrap_or(0);
        if len == 0 {
            return;
        }
        if self.at_end() {
            // 已放完：从头播
            self.feed_pos = 0;
            self.anchor_frame = 0;
            self.shared.reset_pending.store(true, Ordering::Relaxed);
            if let Some(s) = &mut self.source {
                s.set_track_position(0);
            }
        }
        self.shared.playing.store(true, Ordering::Relaxed);
        self.feed();
        if let Some(s) = &self.stream {
            let _ = s.play();
        }
    }

    pub fn pause(&mut self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    pub fn stop(&mut self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        self.feed_pos = 0;
        self.anchor_frame = 0;
        self.shared.reset_pending.store(true, Ordering::Relaxed);
        if let Some(s) = &mut self.source {
            s.set_track_position(0);
        }
        self.feed();
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn set_loop(&mut self, v: bool) {
        self.shared.looping.store(v, Ordering::Relaxed);
    }

    /// 进度 = 绝对内容位置（原始音频比例）
    pub fn progress(&self) -> f32 {
        let len = self.src.as_ref().map(|v| v.len()).unwrap_or(0);
        if len == 0 {
            return 0.0;
        }
        let pos = self.abs_pos();
        (pos / len as f64).clamp(0.0, 1.0) as f32
    }

    /// 跳到内容位置（原始音频比例）。复位处理器并以目标帧为锚点，进度立即归位。
    /// 注意：不要调用 source.set_track_position(target)——那会把 target 写入 engine 的
    /// track anchor，source_position() 已包含该偏移，再叠加 anchor_frame 会双重记账，
    /// 导致进度变成 2×target（点击后找不到对应句子）。reset 后 source_position 从 0 起，
    /// anchor_frame 单独负责绝对位置即可。
    pub fn seek(&mut self, progress: f32) {
        let Some(src) = &self.src else { return };
        let len = src.len();
        if len == 0 {
            return;
        }
        let target = ((progress.clamp(0.0, 1.0) as f64) * len as f64) as u64;
        self.feed_pos = target as usize;
        self.anchor_frame = target;
        self.shared.reset_pending.store(true, Ordering::Relaxed);
        self.feed();
    }

    /// 当前内容时间（原始音频秒）
    pub fn current_time_sec(&self) -> f64 {
        self.abs_pos() / self.src_sr.max(1) as f64
    }

    /// 总时长（原始音频秒）
    pub fn duration_sec(&self) -> f64 {
        let len = self.src.as_ref().map(|v| v.len()).unwrap_or(0);
        len as f64 / self.src_sr.max(1) as f64
    }

    pub fn has_data(&self) -> bool {
        self.src.as_ref().map(|v| !v.is_empty()).unwrap_or(false)
    }

    /// 喂音频：App 每 30ms tick 调用一次。维持 ring 缓冲 ≥ demand_hint，
    /// 播到末尾（非循环）则停播；循环时回绕（reset 归位进度）。
    pub fn feed(&mut self) {
        let Some(src) = &self.src else { return };
        let Some(source) = &mut self.source else { return };
        let hint = source.demand_hint(BLOCK_REF, 4.0);
        let mut rounds = 0usize;
        while rounds < 4 {
            while source.occupied_frames() < hint && self.feed_pos < src.len() {
                let end = (self.feed_pos + FEED_CHUNK).min(src.len());
                let pushed = source.push(&src[self.feed_pos..end]);
                self.feed_pos += pushed;
                if pushed == 0 {
                    break;
                }
            }
            if self.feed_pos >= src.len() {
                if self.shared.looping.load(Ordering::Relaxed) && rounds < 3 {
                    self.feed_pos = 0;
                    self.anchor_frame = 0;
                    self.shared.reset_pending.store(true, Ordering::Relaxed);
                    source.set_track_position(0);
                    rounds += 1;
                    continue;
                }
                if !self.shared.looping.load(Ordering::Relaxed) {
                    self.shared.playing.store(false, Ordering::Relaxed);
                }
                break;
            }
            break;
        }
    }

    fn at_end(&self) -> bool {
        let len = self.src.as_ref().map(|v| v.len()).unwrap_or(0);
        if len == 0 {
            return true;
        }
        self.abs_pos() >= len as f64 - 1.0
    }

    /// 绝对内容位置（源样本帧）= 锚定帧 + engine 发布的 fed-source 坐标
    fn abs_pos(&self) -> f64 {
        let rel = self
            .controller
            .as_ref()
            .map(|c| c.source_position())
            .unwrap_or(0.0);
        self.anchor_frame as f64 + rel
    }
}

/// 整段时间拉伸（worker 导出/复制倍速用；与实时播放无关）。
/// speed = 倍速（2.0 = 2 倍快）；stretch ratio = 1/speed（>1 变慢、<1 变快）。
pub fn do_stretch(input: &[f32], sample_rate: u32, speed: f32) -> Vec<f32> {
    if input.is_empty() || speed <= 0.0 {
        return input.to_vec();
    }
    let ratio = (1.0 / speed as f64).clamp(0.01, 100.0);
    if (ratio - 1.0).abs() <= f64::EPSILON {
        return input.to_vec();
    }
    let params = timestretch::StretchParams::new(ratio)
        .with_sample_rate(sample_rate)
        .with_channels(1);
    timestretch::stretch(input, &params).unwrap_or_else(|_| input.to_vec())
}

fn clamp_speed(speed: f64) -> f64 {
    speed.clamp(SPEED_MIN, SPEED_MAX)
}

/// 线性插值重采样（from → to）。to/from 为采样率。
fn resample_linear(src: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || src.is_empty() {
        return src.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = ((src.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 / ratio;
        let i0 = pos.floor() as usize;
        let frac = pos - i0 as f64;
        let i1 = (i0 + 1).min(src.len() - 1);
        let v = src[i0] as f64 * (1.0 - frac) + src[i1] as f64 * frac;
        out.push(v as f32);
    }
    out
}

#[cfg(test)]
mod seek_tests {
    use timestretch::engine::{Engine, EngineConfig, EngineProfile};

    fn build(sr: u32) -> (timestretch::engine::EngineController,
                           timestretch::engine::SourceProducer,
                           timestretch::engine::EngineProcessor) {
        let handles = Engine::build(EngineConfig {
            sample_rate: sr,
            channels: 1,
            profile: EngineProfile::WideKeylock,
            initial_tempo_rate: 1.0,
            max_block_frames: 1024,
            source_capacity_frames: 131_072,
            pre_analysis: None,
        })
        .expect("engine build");
        (handles.controller, handles.source, handles.processor)
    }

    #[test]
    fn seek_position_restarts_after_reset() {
        let sr = 44_100u32;
        let (ctrl, mut prod, mut proc) = build(sr);
        let total = (sr * 4) as usize;
        let src: Vec<f32> = (0..total).map(|i| ((i % 97) as f32 / 97.0 - 0.5) * 0.4).collect();
        let mut out = vec![0.0f32; 512];

        // 阶段 1：喂前 1 秒并消费
        let mut fed = 0usize;
        for _ in 0..160 {
            if fed < sr as usize {
                let end = (fed + 4096).min(total);
                fed += prod.push(&src[fed..end]);
            }
            proc.process(&mut out);
        }
        let p1 = ctrl.source_position();
        eprintln!("p1 after ~1s feed+process: {p1}");
        assert!(p1 > 0.0 && p1 < sr as f64 * 1.5, "p1 out of range: {p1}");

        // 阶段 2：模拟 app seek 到 1.5s（target=66150）：proc.reset() + 从 target 喂
        let target = 66_150u64; // 1.5s
        proc.reset();
        let mut fed2 = 0usize;
        for _ in 0..160 {
            if fed2 < sr as usize {
                let pos = target as usize + fed2;
                let end = (pos + 4096).min(total);
                fed2 += prod.push(&src[pos..end]);
            }
            proc.process(&mut out);
        }
        let p2 = ctrl.source_position();
        eprintln!("p2 after reset + seek feed: {p2}");
        // 期望：相对 reset 从 0 起（p2 ≈ fed2 附近，1s 内）
        assert!(p2 >= 0.0 && p2 < sr as f64 * 1.5, "p2 out of range: {p2}");
        let abs = target as f64 + p2;
        eprintln!("abs = target({target}) + p2({p2}) = {abs} (期望 ≈ 1.5s~2.5s 区间)");
        assert!(abs >= target as f64 && abs < (target as f64 + sr as f64 * 1.5));
    }
}
