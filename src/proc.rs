//! GUI 侧的子进程管理：把耗时的 解析/ASR/复制/导出 交给 `--worker` 子进程，
//! 通过 stdout 的 JSON 行协议异步接收进度与结果，UI 线程只轮询通道，永不阻塞。
//!
//! 协议与 worker 角色定义见 crate::worker。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};

/// 来自 worker 子进程的一条消息
#[derive(Debug)]
pub enum ProcMsg {
    /// 一行 JSON 进度/结果
    Json(String),
    /// 进程已退出（携带退出码）；一次性报告
    Exit(i32),
}

/// 一个已启动的 worker 子进程
pub struct Worker {
    pub role: &'static str,
    pub child: Child,
    rx: Receiver<ProcMsg>,
    reported_exit: bool,
}

fn exe_path() -> std::path::PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("echo_dup"))
}

impl Worker {
    /// 启动 `--worker <role> <payload_json>`。payload_json 由调用方先写好。
    pub fn spawn(role: &'static str, payload: &Path) -> std::io::Result<Worker> {
        let mut cmd = Command::new(exe_path());
        cmd.arg("--worker")
            .arg(role)
            .arg(payload)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .stdin(Stdio::null());
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("stdout piped");
        let (tx, rx) = channel();

        // 读线程：逐行转发 JSON，读到 EOF 即结束（进程状态由主线程 try_wait 报告）
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        let t = l.trim();
                        if !t.is_empty() && tx.send(ProcMsg::Json(t.to_string())).is_err() {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Worker { role, child, rx, reported_exit: false })
    }

    /// 拉取当前所有消息：JSON 行 + （若已退出）退出消息
    pub fn drain(&mut self) -> Vec<ProcMsg> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            out.push(m);
        }
        if !self.reported_exit {
            if let Some(status) = self.child.try_wait().ok().flatten() {
                self.reported_exit = true;
                out.push(ProcMsg::Exit(status.code().unwrap_or(-1)));
            }
        }
        out
    }

    /// 强制结束子进程（窗口关闭时调用）
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 写一个临时 JSON 载荷文件，返回路径
pub fn write_payload(
    dir: &Path,
    role: &str,
    v: &serde_json::Value,
) -> std::io::Result<std::path::PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = dir.join(format!("echodup_{role}_{nanos}.json"));
    std::fs::write(&p, serde_json::to_vec(v).unwrap())?;
    Ok(p)
}
