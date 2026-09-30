//! Windows OLE 文件拖拽支持：把拖入窗口的文件（音频 / 文本）交给 UI 线程处理。
//!
//! Slint 1.18 的 winit 后端不转发 winit 的 `DroppedFile` 事件，因此直接在
//! Windows 上注册标准 OLE `IDropTarget`（Explorer 的文件拖拽都走 OLE），
//! 在 `Drop` 回调里解析 `CF_HDROP` 中的文件路径，经 mpsc 通道发给 UI 线程，
//! 与“打开文件”对话框共用同一处理链路（按扩展名分发音频 / 文本）。
//!
//! 注意：windows 0.58 的 `#[implement]` 属性宏会把实现 trait 绑定到其生成的
//! `X_Impl` 包装类型上（vtbl 的 `Identity` 是该包装类型），因此这里的
//! `IDropTarget_Impl` 必须实现于 `FileDropTarget_Impl`，字段经 `self.this` 访问。

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use windows::core::{implement, Result};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINTL};
use windows::Win32::System::Com::{IDataObject, FORMATETC, DVASPECT_CONTENT, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop, RevokeDragDrop,
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible};

/// OLE 拖拽目标：持有一个把拖入路径发给 UI 线程的通道。
#[implement(IDropTarget)]
pub struct FileDropTarget {
    tx: Sender<Vec<PathBuf>>,
}

impl FileDropTarget {
    fn new(tx: Sender<Vec<PathBuf>>) -> Self {
        Self { tx }
    }
}

// windows-implement 0.58：trait 必须实现于宏生成的包装类型 `FileDropTarget_Impl`，
// 用户的字段放在 `this`（`FileDropTarget`）中。
impl IDropTarget_Impl for FileDropTarget_Impl {
    fn DragEnter(
        &self,
        _data: Option<&IDataObject>,
        _key: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        unsafe { *effect = DROPEFFECT_COPY; }
        Ok(())
    }

    fn DragOver(
        &self,
        _key: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        unsafe { *effect = DROPEFFECT_COPY; }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        Ok(())
    }

    fn Drop(
        &self,
        data: Option<&IDataObject>,
        _key: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        unsafe { *effect = DROPEFFECT_COPY; }
        if let Some(obj) = data {
            let fmt = FORMATETC {
                cfFormat: CF_HDROP.0,
                ptd: core::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            };
            unsafe {
                if let Ok(mut medium) = obj.GetData(&fmt) {
                    let hdrop = HDROP(medium.u.hGlobal.0);
                    let mut paths: Vec<PathBuf> = Vec::new();
                    let count = DragQueryFileW(hdrop, u32::MAX, None);
                    for i in 0..count {
                        let len = DragQueryFileW(hdrop, i, None);
                        if len == 0 {
                            continue;
                        }
                        let mut buf = vec![0u16; len as usize + 1];
                        DragQueryFileW(hdrop, i, Some(&mut buf));
                        paths.push(PathBuf::from(String::from_utf16_lossy(
                            &buf[..len as usize],
                        )));
                    }
                    let _ = self.this.tx.send(paths);
                    let _ = ReleaseStgMedium(&mut medium);
                }
            }
        }
        Ok(())
    }
}

/// 在本进程内查找主窗口句柄。
///
/// winit 会创建一个小型可见辅助窗口（class "Winit Thread Event Target"，约 12x12），
/// 按 PID+可见性枚举可能先撞上它，导致 OLE 注册到错误的窗口、拖拽无效。
/// 因此枚举全部可见顶层窗口，取面积最大者（Slint 主窗口）。
fn find_main_hwnd() -> Option<HWND> {
    struct Ctx {
        best: Option<HWND>,
        best_area: i64,
    }
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lparam.0 as *mut Ctx) };
        let mut pid: u32 = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid == unsafe { GetCurrentProcessId() } && unsafe { IsWindowVisible(hwnd).as_bool() } {
            let mut r = windows::Win32::Foundation::RECT::default();
            unsafe {
                GetWindowRect(hwnd, &mut r);
            }
            let w = (r.right - r.left) as i64;
            let h = (r.bottom - r.top) as i64;
            let area = w * h;
            if area > ctx.best_area {
                ctx.best_area = area;
                ctx.best = Some(hwnd);
            }
        }
        BOOL(1) // 继续枚举全部，取最大
    }
    let mut ctx = Ctx { best: None, best_area: 0 };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.best
}

/// 已注册的拖拽目标及其窗口句柄（主线程持有）。
pub struct DropReg {
    target: IDropTarget,
    hwnd: HWND,
}

/// 确保拖拽目标已注册到当前主窗口。
///
/// 背景：winit/Slint 的窗口句柄要到 ui.run() 显示流程后才真正可用，过早注册会
/// 注册到一个无效/占位句柄上导致拖拽失效。此函数在 UI 轮询中反复调用：
/// 窗口句柄出现后完成注册；若句柄变化（窗口重建）则注销旧句柄并重新注册。
pub fn ensure(cur: &mut Option<DropReg>, tx: &Sender<Vec<PathBuf>>) {
    let Some(hwnd) = find_main_hwnd() else { return };
    if let Some(r) = cur {
        if r.hwnd == hwnd {
            return;
        }
        // 窗口句柄变化：注销旧句柄再重注册
        unsafe {
            let _ = RevokeDragDrop(r.hwnd);
        }
        *cur = None;
    }
    unsafe {
        let _ = OleInitialize(None);
    }
    let target = FileDropTarget::new(tx.clone());
    let com: IDropTarget = target.into();
    unsafe {
        match RegisterDragDrop(hwnd, &com) {
            Ok(()) => {}
            Err(e) if e.code() == windows::core::HRESULT(-2147221247) => {
                // 0x80040101 = DRAGDROP_E_ALREADYREGISTERED：主窗口已被 winit 注册了
                // 拖放目标（winit 会吃掉 DroppedFile 事件但 Slint 不转发，导致拖拽无效）。
                // 注销 winit 的 handler 后重新注册接管。
                #[cfg(debug_assertions)]
                eprintln!("[drop] main window already registered by winit; revoke and retake hwnd={:#x}", hwnd.0 as usize);
                let _ = RevokeDragDrop(hwnd);
                if RegisterDragDrop(hwnd, &com).is_err() {
                    #[cfg(debug_assertions)]
                    eprintln!("[drop] RegisterDragDrop FAILED after revoke");
                    return;
                }
            }
            Err(e) => {
                #[cfg(debug_assertions)]
                eprintln!("[drop] RegisterDragDrop FAILED hwnd={:#x} err={e:?}", hwnd.0 as usize);
                return;
            }
        }
    }
    *cur = Some(DropReg { target: com, hwnd });
    #[cfg(debug_assertions)]
    eprintln!("[drop] OLE drag target registered hwnd={:#x}", hwnd.0 as usize);
}

/// 注销拖拽目标（窗口销毁前调用）。
pub fn revoke(cur: &mut Option<DropReg>) {
    if let Some(r) = cur.take() {
        unsafe {
            let _ = RevokeDragDrop(r.hwnd);
        }
    }
}
