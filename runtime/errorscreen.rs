//! PointerSes Error Screen — surfaces errors visually so ordinary users can
//! see and report them (log output scrolls past and is easy to miss).
//!
//! * Windows: pops up a native message box (Win32 `MessageBoxW`, loaded
//!   dynamically so nothing extra is linked), titled "PointerSes Error".
//! * Everywhere else (Linux, Android Termux, redirected output): renders a
//!   bordered ANSI error screen on stderr.
//!
//! This file is fully self-contained (standard library only) so the *same*
//! source is embedded into the native executables produced by `pssc` — a
//! packaged program that fails at runtime shows the identical error screen.
//!
//! Set `PSS_NO_ERROR_SCREEN=1` to disable the popup (CI / scripts); the plain
//! message is still printed to stderr so an error is never swallowed.

/// Set this environment variable to `1` to disable the popup screen.
pub const DISABLE_ENV: &str = "PSS_NO_ERROR_SCREEN";

/// Whether the error-screen popup is enabled for this process.
pub fn popup_enabled() -> bool {
    std::env::var_os(DISABLE_ENV).is_none()
}

/// Show the error screen for `msg`. Never panics and never returns an error:
/// if the popup cannot be shown, the message still goes to stderr.
pub fn show_error(msg: &str) {
    if !popup_enabled() {
        eprintln!("{msg}");
        return;
    }
    #[cfg(windows)]
    {
        if message_box_w("PointerSes Error", msg) {
            return; // native dialog acknowledged by the user
        }
        // No interactive desktop (CI / service): fall through to ANSI screen.
    }
    eprint!("{}", render_ansi(msg));
}

// ---------------------------------------------------------------------------
// Windows native popup (dynamically loaded, no extra link dependency)
// ---------------------------------------------------------------------------

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "LoadLibraryW"]
    fn load_library_w(lp_lib_file_name: *const u16) -> *mut std::ffi::c_void;
    // Must match the declaration in codegen::llvm_embedded (same crate),
    // otherwise `clashing_extern_declarations` fires.
    #[link_name = "GetProcAddress"]
    fn get_proc_address(
        h_module: *mut std::ffi::c_void,
        lp_proc_name: *const std::ffi::c_char,
    ) -> *mut std::ffi::c_void;
}

#[cfg(windows)]
type MessageBoxProc =
    unsafe extern "system" fn(*const std::ffi::c_void, *const u16, *const u16, u32) -> i32;

/// Show a native message box. Returns `true` if the box was actually shown
/// (i.e. we are on an interactive desktop). Returns `false` on any failure so
/// the caller can fall back to the ANSI screen.
#[cfg(windows)]
fn message_box_w(caption: &str, text: &str) -> bool {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    let cap: Vec<u16> = OsStr::new(caption).encode_wide().chain(Some(0)).collect();
    let txt: Vec<u16> = OsStr::new(text).encode_wide().chain(Some(0)).collect();
    let dll: Vec<u16> = OsStr::new("user32.dll").encode_wide().chain(Some(0)).collect();
    let proc_name: Vec<u8> = b"MessageBoxW\0".to_vec();

    // MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST
    const FLAGS: u32 = 0x0000 | 0x0010 | 0x0001_0000 | 0x0004_0000;

    unsafe {
        let lib = load_library_w(dll.as_ptr());
        if lib.is_null() {
            return false;
        }
        let proc = get_proc_address(lib, proc_name.as_ptr().cast());
        if proc.is_null() {
            return false;
        }
        let msgbox: MessageBoxProc = std::mem::transmute(proc);
        msgbox(std::ptr::null(), txt.as_ptr(), cap.as_ptr(), FLAGS) != 0
    }
}

// ---------------------------------------------------------------------------
// ANSI terminal error screen
// ---------------------------------------------------------------------------

/// Render a bordered, red-accented error screen as ANSI text.
pub fn render_ansi(msg: &str) -> String {
    const W: usize = 72;
    let mut out = String::new();
    // Clear screen, home cursor, red on black.
    out.push_str("\x1b[2J\x1b[H\x1b[1;31m");
    out.push_str(&format!(
        "┌{}┐\n│{}│\n",
        "─".repeat(W),
        center("PointerSes Error Screen", W)
    ));
    out.push_str(&format!("├{}┤\n", "─".repeat(W)));
    for line in wrap(msg, W - 4) {
        out.push_str(&format!("│  {}  │\n", pad(&line, W - 4)));
    }
    out.push_str(&format!("├{}┤\n", "─".repeat(W)));
    out.push_str("│  pss task pointerses-error-screen -e \"<错误内容>\"   │\n");
    out.push_str(&format!("│{}│\n", pad("", W - 4)));
    out.push_str(&format!("│  {}", pad("请复制以上错误内容，用于报告问题。", W - 4)));
    out.push_str("  │\n");
    out.push_str(&format!("└{}┘\n\x1b[0m", "─".repeat(W)));
    out
}

/// Center `s` within `w` columns (box-drawing chars are single-width here).
fn center(s: &str, w: usize) -> String {
    let trimmed = s.trim();
    let len = trimmed.chars().count();
    let pad_total = w.saturating_sub(len);
    let left = pad_total / 2;
    let right = pad_total - left;
    format!("{}{}{}", " ".repeat(left), trimmed, " ".repeat(right))
}

/// Pad `s` to exactly `w` columns (truncate if longer).
fn pad(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len >= w {
        s.chars().take(w).collect()
    } else {
        format!("{}{}", s, " ".repeat(w - len))
    }
}

/// Wrap `s` into lines of at most `w` columns (word-aware, CJK-safe).
fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for word in s.split_whitespace() {
        let wlen = word.chars().count();
        if !cur.is_empty() && cur_len + 1 + wlen > w {
            lines.push(cur);
            cur = String::new();
            cur_len = 0;
        }
        if wlen > w {
            // A single overlong token: hard-break it.
            for (i, ch) in word.chars().enumerate() {
                if i % w == 0 && i > 0 {
                    lines.push(cur);
                    cur = String::new();
                }
                cur.push(ch);
            }
            cur_len = cur.chars().count();
            continue;
        }
        if !cur.is_empty() {
            cur.push(' ');
            cur_len += 1;
        }
        cur.push_str(word);
        cur_len += wlen;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_respects_width() {
        let lines = wrap("a b c d e f g h i j", 10);
        assert!(lines.iter().all(|l| l.chars().count() <= 10));
        assert_eq!(lines.join(" ").replace(" ", ""), "abcdefghij");
    }

    #[test]
    fn wrap_long_single_word_breaks() {
        let lines = wrap("abcdefghijklmnop", 6);
        assert!(lines.iter().all(|l| l.chars().count() <= 6));
        assert_eq!(lines.iter().map(|l| l.chars().count()).sum::<usize>(), 16);
    }

    #[test]
    fn wrap_cjk_words() {
        let lines = wrap("这是一段比较长的中文错误描述信息用于测试换行行为", 12);
        assert!(lines.iter().all(|l| l.chars().count() <= 12));
        let joined: String = lines.join("");
        assert_eq!(joined, "这是一段比较长的中文错误描述信息用于测试换行行为");
    }

    #[test]
    fn pad_centers_exactly() {
        assert_eq!(pad("x", 3), "x  ");
        assert_eq!(pad("abc", 3), "abc");
        assert_eq!(pad("abcd", 3), "abc");
        assert_eq!(center("hi", 8), "   hi   ");
        assert_eq!(center("longer", 4), "longer"); // center pads but never truncates
    }

    #[test]
    fn render_ansi_contains_title_and_message() {
        let out = render_ansi("boom at line 3");
        assert!(out.contains("PointerSes Error Screen"));
        assert!(out.contains("boom at line 3"));
        assert!(out.starts_with("\x1b[2J"));
        assert!(out.ends_with("\x1b[0m"));
    }
}
