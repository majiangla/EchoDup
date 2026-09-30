use serde::{Deserialize, Serialize};
use std::path::Path;

/// EchoDup 配置：红蓝紫（A/B/C）分段检测参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 静音阈值（dB，-80..0），默认 -30。
    pub threshold_db: f64,
    /// 最小静音时长（秒），默认 1.0。
    pub min_silence: f64,
    /// 最短声音时长（秒），默认 1.0。
    pub min_sound: f64,
    /// 边界容差（"夹住"判定距离，秒），默认 2.0。
    pub zone_tolerance: f64,
    /// 解码目标采样率。
    pub target_sample_rate: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            threshold_db: -30.0,
            min_silence: 1.0,
            min_sound: 1.0,
            zone_tolerance: 2.0,
            target_sample_rate: 16000,
        }
    }
}

impl Config {
    pub fn load_or_default(exe_dir: &Path) -> Self {
        let candidates = [
            exe_dir.join("config.toml"),
            dirs_appdata().join("echo_dup").join("config.toml"),
        ];
        for p in candidates.iter() {
            if p.exists() {
                if let Ok(s) = std::fs::read_to_string(p) {
                    if let Ok(c) = toml::from_str::<Config>(&s) {
                        return c;
                    }
                }
            }
        }
        Config::default()
    }
}

fn dirs_appdata() -> std::path::PathBuf {
    std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}
