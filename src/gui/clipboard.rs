use anyhow::Result;
use hound::{SampleFormat, WavSpec, WavReader, WavWriter};
use std::path::PathBuf;
use windows::Win32::Foundation::{HANDLE, HWND, BOOL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Shell::DROPFILES;

const CF_HDROP: u32 = 15;
const CF_UNICODETEXT: u32 = 13;

/// 读取 16bit PCM wav，返回 (f32 单声道采样, 采样率)。用于 worker 读缓存 wav。
pub fn read_wav_mono(path: &std::path::Path) -> Result<(Vec<f32>, u32)> {
    let mut r = WavReader::open(path)?;
    let sr = r.spec().sample_rate;
    let ch = r.spec().channels as usize;
    let bits = r.spec().bits_per_sample;
    let samples: Vec<f32> = if bits == 16 {
        r.samples::<i16>()
            .filter_map(|s| s.ok())
            .map(|v| v as f32 / 32768.0)
            .collect()
    } else if bits == 32 && r.spec().sample_format == SampleFormat::Float {
        r.samples::<f32>().filter_map(|s| s.ok()).collect()
    } else {
        // 其他位深按 i32 归一化
        r.samples::<i32>()
            .filter_map(|s| s.ok())
            .map(|v| (v as f64 / 2147483648.0) as f32)
            .collect()
    };
    let mono: Vec<f32> = if ch == 1 {
        samples
    } else {
        samples
            .chunks(ch)
            .map(|f| f.iter().sum::<f32>() / ch as f32)
            .collect()
    };
    Ok((mono, sr))
}

/// 纯文本复制（复制文本按钮），瞬时完成留在 GUI 进程
pub fn set_text(text: &str) -> Result<()> {
    unsafe {
        let mut opened = false;
        for _ in 0..20 {
            if OpenClipboard(HWND(std::ptr::null_mut())).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !opened {
            anyhow::bail!("OpenClipboard failed");
        }
        let _ = EmptyClipboard();

        let wide: Vec<u16> = text
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let bytes = wide.len() * 2;
        let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
        let base = GlobalLock(hmem) as *mut u8;
        std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, base, bytes);
        let _ = GlobalUnlock(hmem);
        let set_res = SetClipboardData(CF_UNICODETEXT, HANDLE(hmem.0));
        let _ = CloseClipboard();
        if let Err(e) = set_res {
            anyhow::bail!("SetClipboardData failed: {e}");
        }
    }
    Ok(())
}

pub fn copy_wav(samples: &[f32], sample_rate: u32, channels: u16, filename: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("echodup");
    std::fs::create_dir_all(&dir)?;
    // 唯一文件名，避免上一次剪贴板仍占用同名文件导致写入失败
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let stem = std::path::Path::new(filename)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
    let path = dir.join(format!("{stem}_{nanos}.wav"));
    write_wav_file(&path, samples, sample_rate, channels)?;
    unsafe {
        set_clipboard_file(&path)?;
    }
    Ok(path)
}

/// 直接把 f32 采样写成 16bit PCM WAV 文件
pub fn write_wav_file(path: &std::path::Path, samples: &[f32], sample_rate: u32, channels: u16) -> Result<()> {
    let spec = WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut w = WavWriter::create(path, spec)?;
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        w.write_sample(v)?;
    }
    w.finalize()?;
    Ok(())
}

unsafe fn set_clipboard_file(path: &std::path::Path) -> Result<()> {
    // 重试：剪贴板可能被其他程序短暂占用
    let mut opened = false;
    for _ in 0..20 {
        if OpenClipboard(HWND(std::ptr::null_mut())).is_ok() {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if !opened {
        anyhow::bail!("OpenClipboard failed");
    }
    let _ = EmptyClipboard();

    let path_str = path.to_string_lossy().to_string();
    let wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();

    let header = std::mem::size_of::<DROPFILES>();
    let total = header + wide.len() * 2 + 2;

    let hmem = GlobalAlloc(GMEM_MOVEABLE, total)?;
    let base = GlobalLock(hmem) as *mut u8;

    let df = base as *mut DROPFILES;
    (*df).pFiles = header as u32;
    (*df).pt.x = 0;
    (*df).pt.y = 0;
    (*df).fNC = BOOL(0);
    (*df).fWide = BOOL(1);

    let dst = base.add(header) as *mut u16;
    for (i, &c) in wide.iter().enumerate() {
        *dst.add(i) = c;
    }
    *dst.add(wide.len()) = 0;

    let _ = GlobalUnlock(hmem);
    let set_res = SetClipboardData(CF_HDROP, HANDLE(hmem.0));
    let _ = CloseClipboard();
    if let Err(e) = set_res {
        anyhow::bail!("SetClipboardData failed: {e}");
    }
    Ok(())
}
