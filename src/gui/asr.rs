use once_cell::sync::OnceCell;
use std::path::PathBuf;
use std::sync::Mutex;

/// 一条 ASR 句子段：起止秒（相对该切片）+ 文本
#[derive(Debug, Clone)]
pub struct AsrSegment {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

// 模型文件在编译时嵌入 exe
const CT2_CONFIG: &[u8] = include_bytes!("../../models/ct2-tiny/config.json");
const CT2_MODEL_BIN: &[u8] = include_bytes!("../../models/ct2-tiny/model.bin");
const CT2_TOKENIZER: &[u8] = include_bytes!("../../models/ct2-tiny/tokenizer.json");
const CT2_VOCAB: &[u8] = include_bytes!("../../models/ct2-tiny/vocabulary.txt");
const CT2_PREPROC: &[u8] =
    include_bytes!("../../models/ct2-tiny/preprocessor_config.json");

static CTX: OnceCell<Mutex<ct2rs::Whisper>> = OnceCell::new();

fn cache_dir() -> PathBuf {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        PathBuf::from(local).join("echodup").join("cache")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".cache").join("echodup")
    } else {
        std::env::temp_dir().join("echodup-cache")
    }
}

fn write_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    if path.exists() {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() == data.len() as u64 {
                return Ok(());
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, data)
}

/// 解压嵌入的模型到缓存目录，返回模型目录路径
fn model_dir() -> Result<PathBuf, String> {
    let dir = cache_dir().join("models").join("ct2-tiny");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建模型目录失败: {e}"))?;
    write_file(&dir.join("config.json"), CT2_CONFIG).map_err(|e| format!("解压 config: {e}"))?;
    write_file(&dir.join("model.bin"), CT2_MODEL_BIN).map_err(|e| format!("解压 model.bin: {e}"))?;
    write_file(&dir.join("tokenizer.json"), CT2_TOKENIZER).map_err(|e| format!("解压 tokenizer: {e}"))?;
    write_file(&dir.join("vocabulary.txt"), CT2_VOCAB).map_err(|e| format!("解压 vocab: {e}"))?;
    write_file(&dir.join("preprocessor_config.json"), CT2_PREPROC).map_err(|e| format!("解压 preproc: {e}"))?;
    Ok(dir)
}

fn ctx() -> Result<&'static Mutex<ct2rs::Whisper>, String> {
    if let Some(c) = CTX.get() {
        return Ok(c);
    }
    let dir = model_dir()?;
    let threads = (num_cpus::get() - 1).max(1);
    let config = ct2rs::Config {
        compute_type: ct2rs::ComputeType::INT8,
        num_threads_per_replica: threads,
        ..Default::default()
    };
    eprintln!("[asr] loading ct2 model: {} (int8, {} threads)", dir.display(), threads);
    let w = ct2rs::Whisper::new(&dir, config).map_err(|e| format!("加载模型: {e}"))?;
    let _ = CTX.set(Mutex::new(w));
    Ok(CTX.get().unwrap())
}

/// 对一段 16kHz 单声道 f32 音频转写，返回句子段（时间相对该切片）。
/// 模型只加载一次，逐段复用。
pub fn transcribe(samples: &[f32]) -> Result<Vec<AsrSegment>, String> {
    let guard = ctx()?;
    let w = guard.lock().map_err(|e| format!("lock: {e}"))?;

    let opts = ct2rs::WhisperOptions {
        beam_size: 1,
        sampling_topk: 1,
        sampling_temperature: 0.0,
        max_length: 448,
        suppress_blank: true,
        suppress_tokens: vec![-1],
        ..Default::default()
    };

    let segments = w
        .generate_segments(samples, Some("en"), &opts)
        .map_err(|e| format!("转写: {e}"))?;

    // 收集所有词及其真实起止时间（相对切片）
    let mut words: Vec<(f32, f32, String)> = Vec::new();
    for seg in segments {
        if let Some(segs_words) = seg.words {
            for word in segs_words {
                words.push((word.start, word.end, word.word.trim().to_string()));
            }
        }
    }
    if words.is_empty() {
        return Ok(Vec::new());
    }

    // 按词分组为句子：遇到以 . ! ? 结尾的词就断句
    let mut out: Vec<AsrSegment> = Vec::new();
    let mut buf_text: Vec<String> = Vec::new();
    let mut buf_start = words[0].0 as f64;
    let mut buf_end = words[0].0 as f64;
    let ends_sentence = |t: &str| t.ends_with('.') || t.ends_with('!') || t.ends_with('?');

    for (ws, we, wtxt) in &words {
        if wtxt.is_empty() {
            continue;
        }
        if buf_text.is_empty() {
            buf_start = *ws as f64;
        }
        buf_end = *we as f64;
        buf_text.push(wtxt.clone());
        if ends_sentence(wtxt) {
            let text = buf_text.join(" ");
            out.push(AsrSegment { start: buf_start, end: buf_end, text });
            buf_text.clear();
        }
    }
    if !buf_text.is_empty() {
        let text = buf_text.join(" ");
        out.push(AsrSegment { start: buf_start, end: buf_end, text });
    }

    // 强制相邻句子首尾相接：上一句的 end = 下一句的 start，不留空隙
    for i in 1..out.len() {
        let prev_end = out[i - 1].end;
        out[i].start = prev_end;
    }
    Ok(out)
}

