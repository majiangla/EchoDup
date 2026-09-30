//! 重复组类型定义。
//!
//! 旧 Wang 音频指纹检测管线（detect::detect + refine）已由
//! [`crate::core::slice_detect`]（红蓝紫 A/B/C 分段）取代：
//! - A/B = 重复内容对（ASR 只需转写 A）；
//! - C = 不重复段（直接 ASR）。
//! 此处仅保留 GUI / 导出共用的 [`RepeatGroup`] 数据结构。

/// 一对 A/B 重复段（A=先出现，B=后出现，内容重复）。
#[derive(Debug, Clone)]
pub struct RepeatGroup {
    pub group_id: usize,
    pub a: (f64, f64),
    pub b: (f64, f64),
    /// Haitsma 指纹相似度（0.60+ 才认可）。
    pub confidence: f32,
    pub match_type: &'static str,
    /// B 起点相对 A 起点的偏移（秒）。
    pub offset_sec: f64,
}
