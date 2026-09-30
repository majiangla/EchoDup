/// 原文对齐：把用户粘贴的原文按字符级最长公共子序列对齐到 ASR 时间。
/// 参考 es_audio_player/asr_aligner.py（difflib 字符级对齐 + 缺失时间插值）。

/// 一条对齐后的句子（时间相对该 A 段切片，秒）
#[derive(Debug, Clone)]
pub struct AlignedSent {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

fn norm(c: char) -> Option<char> {
    let c = c.to_ascii_lowercase();
    if c.is_ascii_alphanumeric() { Some(c) } else { None }
}

/// 分行模式：0=按句号（。！？.!?…）、1=按换行（每行一句）、2=按原文 M:/W: 说话人标记
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitMode {
    ByPunct = 0,
    ByLine = 1,
    BySpeaker = 2,
}

impl SplitMode {
    pub fn from_repr(v: u8) -> SplitMode {
        match v {
            0 => SplitMode::ByPunct,
            2 => SplitMode::BySpeaker,
            _ => SplitMode::ByLine,
        }
    }
}

/// 把原文按指定模式切成句：对齐后显示的一行 = 一句
fn split_sents(text: &str, mode: SplitMode) -> Vec<String> {
    match mode {
        SplitMode::ByLine => split_by_lines(text),
        SplitMode::ByPunct => split_by_punct(text),
        SplitMode::BySpeaker => split_by_speaker(text),
    }
}

/// 按换行切成行：一行 = 一句（行内标点不拆，空行跳过）
fn split_by_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let line = line.trim().trim_end_matches('\r');
        if !line.is_empty() {
            out.push(line.to_string());
        }
    }
    out
}

/// 按句号分行：忽略换行，遇句末标点（。！？.!?…）切句
fn split_by_punct(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        cur.push(c);
        if "。！？.!?…".contains(c) {
            let seg = cur.trim();
            if !seg.is_empty() {
                out.push(seg.to_string());
            }
            cur.clear();
        }
    }
    let seg = cur.trim();
    if !seg.is_empty() {
        out.push(seg.to_string());
    }
    out
}

/// 按原文 M:/W: 分行：出现 M:/W:/F:/K:/C:（含全角冒号）且其前非字母时切句。
/// 找不到说话人标记时回退为按换行分行。
fn split_by_speaker(text: &str) -> Vec<String> {
    let markers = ["M:", "W:", "F:", "K:", "C:", "M：", "W：", "F：", "K：", "C："];
    let lower = text.to_lowercase();
    let mut starts = Vec::new();
    for m in &markers {
        let ml = m.to_lowercase();
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(&ml) {
            let abs = from + rel;
            let prev_ok = abs == 0
                || !lower[..abs]
                    .chars()
                    .next_back()
                    .unwrap_or(' ')
                    .is_ascii_alphabetic();
            if prev_ok {
                starts.push(abs);
            }
            from = abs + ml.len();
        }
    }
    starts.sort_unstable();
    starts.dedup();
    if starts.is_empty() {
        return split_by_lines(text);
    }
    let mut out = Vec::new();
    for (i, &pos) in starts.iter().enumerate() {
        let end = if i + 1 < starts.len() { starts[i + 1] } else { text.len() };
        let seg = text[pos..end].trim().replace('\n', " ").replace('\r', " ");
        if !seg.is_empty() {
            out.push(seg);
        }
    }
    out
}

/// 把一组 ASR 段转成 (规范字符, 时间) 流：段内按字符数均分时间
fn asr_char_stream(segs: &[(f64, f64, String)]) -> (Vec<char>, Vec<(f64, f64)>) {
    let mut chars = Vec::new();
    let mut times = Vec::new();
    for &(s, e, ref text) in segs {
        let n = text.chars().count().max(1) as f64;
        let mut i = 0.0;
        for c in text.chars() {
            if let Some(nc) = norm(c) {
                let cs = s + (e - s) * i / n;
                let ce = s + (e - s) * (i + 1.0) / n;
                chars.push(nc);
                times.push((cs, ce));
            }
            i += 1.0;
        }
    }
    (chars, times)
}

/// 原文规范字符流，并记录每个规范字符对应原文的句子索引
fn orig_char_stream(text: &str, mode: SplitMode) -> (Vec<char>, Vec<usize>, Vec<String>) {
    let sents = split_sents(text, mode);
    let mut chars = Vec::new();
    let mut sent_of = Vec::new();
    for (si, sent) in sents.iter().enumerate() {
        for c in sent.chars() {
            if let Some(nc) = norm(c) {
                chars.push(nc);
                sent_of.push(si);
            }
        }
    }
    (chars, sent_of, sents)
}

/// 主入口：原文 + 该 A 段的 ASR 段，返回对齐后的句子（mode 决定原文如何分行）
pub fn align(
    transcript: &str,
    asr_segs: &[(f64, f64, String)],
    mode: SplitMode,
) -> Vec<AlignedSent> {
    let (asr_chars, asr_times) = asr_char_stream(asr_segs);
    if asr_chars.is_empty() || transcript.trim().is_empty() {
        return Vec::new();
    }
    let (orig_chars, sent_of, sents) = orig_char_stream(transcript, mode);
    if orig_chars.is_empty() || sents.is_empty() {
        return Vec::new();
    }

    // DP LCS：找原文位置 -> asr 位置 的映射
    // LCS 表（滚动两行省内存）
    let n = orig_chars.len();
    let m = asr_chars.len();
    // 完整 LCS 表用于回溯（规模小，直接存）
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            dp[i + 1][j + 1] = if orig_chars[i] == asr_chars[j] {
                dp[i][j] + 1
            } else {
                dp[i][j + 1].max(dp[i + 1][j])
            };
        }
    }
    // 回溯得到匹配块：orig_idx -> asr_idx
    let mut matched = vec![-1i32; n];
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        if orig_chars[i - 1] == asr_chars[j - 1] {
            matched[i - 1] = (j - 1) as i32;
            i -= 1; j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }

    // 每句取第一个/最后一个匹配字符的 asr 时间
    let mut first: Vec<Option<f64>> = vec![None; sents.len()];
    let mut last: Vec<Option<f64>> = vec![None; sents.len()];
    for (oi, &ai) in matched.iter().enumerate() {
        if ai < 0 { continue; }
        let si = sent_of[oi];
        let ai = ai as usize;
        let t0 = asr_times[ai].0;
        let t1 = asr_times[ai].1;
        first[si] = Some(first[si].map_or(t0, |x: f64| x.min(t0)));
        last[si] = Some(last[si].map_or(t1, |x: f64| x.max(t1)));
    }

    // 缺失时间插值/外推
    let mut out: Vec<AlignedSent> = sents
        .iter()
        .enumerate()
        .map(|(si, text)| AlignedSent { text: text.clone(), start: first[si].unwrap_or(-1.0), end: last[si].unwrap_or(-1.0) })
        .collect();

    // 锚点
    let anchors: Vec<usize> = (0..out.len()).filter(|&i| out[i].start >= 0.0).collect();
    if anchors.is_empty() {
        return out;
    }
    for i in 0..out.len() {
        if out[i].start >= 0.0 { continue; }
        let prev = anchors.iter().copied().filter(|&a| a < i).last();
        let nxt = anchors.iter().copied().find(|&a| a > i);
        match (prev, nxt) {
            (Some(p), Some(nx)) => {
                let t0 = out[p].end;
                let t1 = out[nx].start;
                let k = i - p;
                let total = nx - p;
                out[i].start = t0 + (t1 - t0) * k as f64 / total as f64;
                out[i].end = t0 + (t1 - t0) * (k + 1) as f64 / total as f64;
            }
            (Some(p), None) => {
                let dur = (out[p].end - out[p].start).max(0.5);
                out[i].start = out[p].end;
                out[i].end = out[p].end + dur;
            }
            (None, Some(nx)) => {
                let dur = (out[nx].end - out[nx].start).max(0.5);
                out[i].start = (out[nx].start - dur).max(0.0);
                out[i].end = out[nx].start;
            }
            (None, None) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_sents_by_line_breaks_only() {
        // 按换行计：每行原文 = 一个显示行；行内标点不拆；空行跳过
        let text = "Hello everyone, welcome.\nI'm fine, thank you. And you?\n\nWhat time is it\nOK?";
        let lines = split_sents(text, SplitMode::ByLine);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "Hello everyone, welcome.");
        assert_eq!(lines[1], "I'm fine, thank you. And you?");
        assert_eq!(lines[2], "What time is it");
        assert_eq!(lines[3], "OK?");
    }

    #[test]
    fn split_sents_crlf_and_trim() {
        let lines = split_sents("  Line one\r\nLine two  \n", SplitMode::ByLine);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "Line one");
        assert_eq!(lines[1], "Line two");
    }

    #[test]
    fn split_sents_by_punct() {
        let lines = split_sents("Hello everyone. How are you? I'm fine!\nNew line here", SplitMode::ByPunct);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "Hello everyone.");
        assert_eq!(lines[1], "How are you?");
        assert_eq!(lines[2], "I'm fine!");
        assert_eq!(lines[3], "New line here");
    }

    #[test]
    fn split_sents_by_speaker() {
        let lines = split_sents("M: Hello everyone.\nW: Welcome!\nNo marker line", SplitMode::BySpeaker);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "M: Hello everyone.");
        assert_eq!(lines[1], "W: Welcome! No marker line");
    }

    #[test]
    fn align_output_rows_match_original_lines() {
        // 原文 3 行（第 2 行无句末标点），ASR 只有 2 段；对齐后每行一个显示行
        let block = "Hello everyone.\nHow are you today\nNice to meet you!";
        let asr = vec![
            (0.0, 1.2, "hello everyone".to_string()),
            (1.3, 3.0, "nice to meet you".to_string()),
        ];
        let out = align(&block, &asr, SplitMode::ByLine);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].text, "Hello everyone.");
        assert_eq!(out[1].text, "How are you today");
        assert_eq!(out[2].text, "Nice to meet you!");
        // 有匹配的句子应有真实时间，未匹配的经插值后也非 -1
        assert!(out[0].start >= 0.0 && out[0].end >= 0.0);
        assert!(out[1].start >= 0.0 && out[1].end >= 0.0);
        assert!(out[2].start >= 0.0 && out[2].end >= 0.0);
    }
}
