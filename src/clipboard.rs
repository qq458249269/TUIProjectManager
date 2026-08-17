//! Windows 剪贴板「文件路径」读取 + Ctrl+V 文件粘贴拦截。
//!
//! 背景：egui-winit 收到 Ctrl+V 只会做文本粘贴——剪贴板里只有文件（Explorer
//! 复制/剪切，无 CF_UNICODETEXT）时它把按键整体吞掉、什么都不产生。所以这里
//! 用 winit 的 `with_msg_hook` 在窗口过程之前拦 WM_KEYDOWN：检测到
//! 「Ctrl+V 且剪贴板确实有文件」就置位标志并吞掉消息；UI 帧消费标志后把
//! 文件路径填进焦点终端。剪贴板文件格式：
//!   - Explorer 复制 → FileGroupDescriptorW（文件名在定长 FILEDESCRIPTORW 里）；
//!   - 剪切 / 拖放 → CF_HDROP（DROPFILES + 双空结尾路径串）。

use std::sync::atomic::{AtomicBool, Ordering};

/// 消息钩子发现「Ctrl+V 且剪贴板含文件」时置位，下一帧由焦点终端消费。
pub static FILES_PASTE_REQUESTED: AtomicBool = AtomicBool::new(false);

const CF_HDROP: u32 = 0x000F;
const WM_KEYDOWN: u32 = 0x0100;
const VK_V: u32 = 0x56;
const VK_CONTROL: u32 = 0x11;

#[link(name = "user32")]
unsafe extern "system" {
    fn OpenClipboard(hwnd: *const std::ffi::c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn GetClipboardData(fmt: u32) -> *const std::ffi::c_void;
    fn IsClipboardFormatAvailable(fmt: u32) -> i32;
    fn RegisterClipboardFormatW(name: *const u16) -> u32;
    fn GetKeyState(vkey: i32) -> i16;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalLock(h: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    fn GlobalUnlock(h: *const std::ffi::c_void) -> i32;
    fn GlobalSize(h: *const std::ffi::c_void) -> usize;
}

/// winit `with_msg_hook` 回调：返回 true = 吞掉这条消息（不再派发给窗口过程）。
/// 只拦 WM_KEYDOWN + V + Ctrl：剪贴板确有文件时才置位并吞掉（阻止 egui-winit
/// 做无效的空文本粘贴），其余情况一律放行走正常文本粘贴。
pub fn msg_hook(msg: *const std::ffi::c_void) -> bool {
    if msg.is_null() {
        return false;
    }
    unsafe {
        // MSG 布局：hwnd + message(UINT) + wParam + lParam + time + pt。
        // x64：hwnd(0..8) message(8..12) wParam(12..20)；x86：hwnd(0..4) message(4..8) wParam(8..12)。
        #[cfg(target_pointer_width = "64")]
        let (m_off, w_off) = (8usize, 12usize);
        #[cfg(target_pointer_width = "32")]
        let (m_off, w_off) = (4usize, 8usize);
        let base = msg as *const u8;
        let message = *(base.add(m_off) as *const u32);
        let wparam = *(base.add(w_off) as *const u32);
        if (message == WM_KEYDOWN)
            && (wparam & 0xFFFF) == VK_V
            && (GetKeyState(VK_CONTROL as i32) < 0) // 高位=按下
            && !clipboard_files().is_empty()
        {
            FILES_PASTE_REQUESTED.store(true, Ordering::Relaxed);
            return true;
        }
    }
    false
}

/// 读取剪贴板里的文件路径（内核为 UTF-16/ANSI 混合结构，统一转成 String）。
/// 返回空 Vec = 剪贴板里没有文件（可能只有文本 — 文本粘贴走 egui 原有路径）。
pub fn clipboard_files() -> Vec<String> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        // 剪切/拖放来源：CF_HDROP。
        if IsClipboardFormatAvailable(CF_HDROP) != 0 {
            out = read_global(GetClipboardData(CF_HDROP), parse_hdrop);
        }
        // Explorer 复制来源：FileGroupDescriptorW（注册格式，名字定死）。
        if out.is_empty() {
            let fmt = RegisterClipboardFormatW(wide("FileGroupDescriptorW").as_ptr());
            if fmt != 0 && IsClipboardFormatAvailable(fmt) != 0 {
                out = read_global(GetClipboardData(fmt), parse_descriptor);
            }
        }
        CloseClipboard();
        out
    }
}

/// GlobalLock → 解析 → GlobalUnlock 的通用外壳。
unsafe fn read_global(
    handle: *const std::ffi::c_void,
    parse: fn(&[u8]) -> Vec<String>,
) -> Vec<String> {
    if handle.is_null() {
        return Vec::new();
    }
    let ptr = unsafe { GlobalLock(handle) };
    if ptr.is_null() {
        return Vec::new();
    }
    let size = unsafe { GlobalSize(handle) };
    let out = if size > 0 {
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, size) };
        parse(bytes)
    } else {
        Vec::new()
    };
    unsafe { GlobalUnlock(handle) };
    out
}

/// 解析 DROPFILES：pFiles 偏移 0 的 DWORD 指向路径区起始，fWide 决定 UTF-16/ANSI，
/// 路径以连续 NUL 结尾（双空=列表结束）。
fn parse_hdrop(bytes: &[u8]) -> Vec<String> {
    if bytes.len() < 20 {
        return Vec::new();
    }
    let u32s = unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u32, bytes.len() / 4) };
    let start = u32s[0] as usize; // DROPFILES::pFiles
    let f_wide = u32s[4] != 0; // DROPFILES::fWide
    if start >= bytes.len() {
        return Vec::new();
    }
    let tail = &bytes[start..];
    if f_wide {
        split_nul_wide(tail)
    } else {
        split_nul_ansi(tail)
    }
}

/// 解析 FileGroupDescriptorW：首 DWORD 是记录数，每条 FILEDESCRIPTORW 定长 592
/// 字节、cFileName（宽字符）从记录内偏移 72 开始（MAX_PATH=260）。
fn parse_descriptor(bytes: &[u8]) -> Vec<String> {
    if bytes.len() < 4 {
        return Vec::new();
    }
    const RECORD: usize = 592;
    const NAME_OFF: usize = 72;
    const NAME_LEN: usize = 260;
    let count = unsafe { *(bytes.as_ptr() as *const u32) } as usize;
    let mut out = Vec::new();
    for i in 0..count.min((bytes.len() - 4) / RECORD) {
        let rec = &bytes[4 + i * RECORD..];
        let mut name = String::new();
        // cFileName 在记录内的偏移 = 72 字节，宽字符定长 260。
        let name_bytes = &rec[NAME_OFF..(NAME_OFF + NAME_LEN * 2).min(rec.len())];
        let u16s: Vec<u16> = name_bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        for &u in &u16s {
            if u == 0 {
                break;
            }
            name.push(char::from_u32(u as u32).unwrap_or('\u{FFFD}'));
        }
        if !name.is_empty() {
            out.push(name);
        }
    }
    out
}

/// 按 NUL 切分宽字符路径串（双空 = 列表结束）。
fn split_nul_wide(tail: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for chunk in tail.chunks_exact(2) {
        let u = u16::from_le_bytes([chunk[0], chunk[1]]);
        if u == 0 {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            } else if !out.is_empty() {
                break; // 双空 = 结束
            }
        } else {
            cur.push(char::from_u32(u as u32).unwrap_or('\u{FFFD}'));
        }
    }
    out
}

/// 按 NUL 切分 ANSI 路径串（双空 = 列表结束）。
fn split_nul_ansi(tail: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for &b in tail {
        if b == 0 {
            if !cur.is_empty() {
                out.push(String::from_utf8_lossy(&cur).into_owned());
                cur.clear();
            } else if !out.is_empty() {
                break; // 双空 = 结束
            }
        } else {
            cur.push(b);
        }
    }
    out
}

/// "abc" → "abc\0" 的 UTF-16 编码（win32 宽字符串）。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 DROPFILES（19 字节头部按 pFiles 起始）+ UTF-16 双空路径列表。
    #[test]
    fn hdrop_wide_paths() {
        let mut buf = vec![0u8; 20];
        let wide_part: Vec<u8> = "C:\\a\\b.txt\0D:\\文件夹\\c.txt\0\0"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        buf.extend_from_slice(&wide_part);
        // pFiles=20, fWide=1
        buf[0..4].copy_from_slice(&20u32.to_le_bytes());
        buf[16..20].copy_from_slice(&1u32.to_le_bytes());
        let out = parse_hdrop(&buf);
        assert_eq!(out, vec!["C:\\a\\b.txt".to_string(), "D:\\文件夹\\c.txt".to_string()]);
    }

    /// FileGroupDescriptorW：214*count 的头（4 字节计数）+ 每条 592 字节、名字在 72 处。
    #[test]
    fn descriptor_paths() {
        let mut buf = vec![0u8; 4];
        let name_bytes: Vec<u8> = "E:\\报告.xlsx\0".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut rec = vec![0u8; 592];
        rec[72..72 + name_bytes.len()].copy_from_slice(&name_bytes);
        buf.extend_from_slice(&rec); // 1 条
        buf[0..4].copy_from_slice(&1u32.to_le_bytes());
        let out = parse_descriptor(&buf);
        assert_eq!(out, vec!["E:\\报告.xlsx".to_string()]);
    }

    /// 空剪贴板数据 → 空结果，不 panic。
    #[test]
    fn empty_buffers() {
        assert!(parse_hdrop(&[]).is_empty());
        assert!(parse_hdrop(&[0u8; 4]).is_empty());
        assert!(parse_descriptor(&[]).is_empty());
        assert!(parse_descriptor(&[5u8; 2]).is_empty());
    }
}