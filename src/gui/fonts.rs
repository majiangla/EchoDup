//! 枚举 Windows 系统已安装字体（GDI），供「查看 > 字体」菜单动态列出。
//! 使用 EnumFontFamiliesExW 枚举全部字族，按字面名去重。

use std::sync::Mutex;
use windows::Win32::Foundation::LPARAM;
use windows::Win32::Graphics::Gdi::{
    EnumFontFamiliesExW, FONTENUMPROCW, GetDC, LOGFONTW, ReleaseDC, TEXTMETRICW, DEFAULT_CHARSET,
};

static FONT_BUF: Mutex<Option<Vec<String>>> = Mutex::new(None);

unsafe extern "system" fn font_enum_proc(
    lplogfont: *const LOGFONTW,
    _lptm: *const TEXTMETRICW,
    _fonttype: u32,
    _lparam: LPARAM,
) -> i32 {
    let mut guard = match FONT_BUF.lock() {
        Ok(g) => g,
        Err(_) => return 1,
    };
    let vec = guard.get_or_insert_with(Vec::new);
    if !lplogfont.is_null() {
        let lf = &*lplogfont;
        let mut len = 0usize;
        while len < lf.lfFaceName.len() && lf.lfFaceName[len] != 0 {
            len += 1;
        }
        let name = String::from_utf16_lossy(&lf.lfFaceName[..len]);
        if !name.is_empty() && !vec.iter().any(|x| x == &name) {
            vec.push(name);
        }
    }
    1
}

/// 返回系统全部字体族名（去重，顺序为 GDI 枚举顺序，大致按字母序）。
pub fn enum_system_fonts() -> Vec<String> {
    unsafe {
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return Vec::new();
        }
        let mut lf: LOGFONTW = Default::default();
        lf.lfCharSet = DEFAULT_CHARSET; // 枚举所有字符集（靠去重收敛）
        let _ = EnumFontFamiliesExW(hdc, &lf, Some(font_enum_proc), LPARAM(0), 0);
        let _ = ReleaseDC(None, hdc);
    }
    match FONT_BUF.lock() {
        Ok(mut g) => g.take().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}
