//! Native GUI: windows and a drawable canvas, built only on the standard
//! library plus system APIs (Win32 on Windows, X11 elsewhere).
//!
//! Windows are recorded as an ordered list of canvas commands and replayed on
//! every repaint, so state stays in plain VM globals and needs no host
//! process. Key and click callbacks run on the VM's own thread between native
//! messages, which lets them read and write top-level globals directly.
use super::*;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::Ordering;

// -----------------------------------------------------------------
// Canvas recording
// -----------------------------------------------------------------

/// One recorded canvas operation. The canvas is an ordered list of these,
/// replayed onto a freshly-filled background on every repaint.
#[derive(Clone)]
enum PaintCmd {
    Text { x: i32, y: i32, text: String, color: i64 },
    Rect { x: i32, y: i32, w: i32, h: i32, color: i64, filled: bool },
}

/// A callback registered on a window: the function to invoke plus the seed
/// values (closure captures) that become its leading parameters.
#[derive(Clone)]
struct Callback {
    fidx: u32,
    seed: Vec<Value>,
}

/// One registered window: title, size, background, recorded canvas and
/// callbacks.
struct WinState {
    title: String,
    w: i32,
    h: i32,
    bg: i64,
    paints: Vec<PaintCmd>,
    on_key: Option<Callback>,
    on_click: Option<Callback>,
    /// Native handle (`HWND` on Windows, `Window` on X11) as a `usize`.
    native: usize,
    /// Set once the native window is destroyed.
    closed: bool,
}

/// Every window created by the program, keyed by the id returned by
/// `window(...)`.
static WINDOWS: Mutex<Vec<WinState>> = Mutex::new(Vec::new());

/// The VM currently blocked inside `event_loop`. Native callbacks run the
/// registered function back on this VM so they share the program's top-level
/// globals. Set and read only by the thread that owns the message loop.
static ACTIVE_VM: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// The VM's suspended state, saved and restored around a callback invocation.
struct Suspended {
    pc: usize,
    code: Vec<u8>,
    frame_obj: u32,
    fname: String,
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    pending: Option<Pending>,
}

/// Lock the window table, recovering from a poisoned mutex.
fn windows() -> std::sync::MutexGuard<'static, Vec<WinState>> {
    WINDOWS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The number of still-open windows.
fn windows_open() -> usize {
    windows().iter().filter(|w| !w.closed).count()
}

/// Find the window id that owns native handle `native`.
fn wid_of_native(native: usize) -> Option<usize> {
    windows().iter().position(|w| w.native == native)
}

/// Mark window `wid` as closed so `event_loop` can exit.
fn mark_closed(wid: usize) {
    if let Some(w) = windows().get_mut(wid) {
        w.closed = true;
    }
}

/// Repaint window `wid` from its recorded canvas into device context `dc`.
///
/// On Windows `dc` is the DC returned by `BeginPaint`, whose origin is the
/// top-left corner of the *client* area — painting through the window DC
/// instead would cover the title bar and trigger an endless `WM_NCPAINT`
/// flood.
fn paint_wid(wid: usize, dc: *mut std::ffi::c_void) {
    let (native, w, h, bg, paints) = match windows().get(wid) {
        Some(w) => (w.native, w.w, w.h, w.bg, w.paints.clone()),
        None => return,
    };
    platform::paint(native, dc, w, h, bg, &paints);
}

// -----------------------------------------------------------------
// Callback execution
// -----------------------------------------------------------------

/// Register `vm` as the active callback target.
fn set_active_vm(vm: &mut Vm<'_>) {
    ACTIVE_VM.store(vm as *mut Vm<'_> as *mut std::ffi::c_void, Ordering::SeqCst);
}

/// Recover the active VM, or `None` if none is blocked in `event_loop`.
///
/// `ACTIVE_VM` is only set while its owner is blocked in `event_loop`, and
/// native callbacks fire only on that same thread while it is blocked, so the
/// returned reference is the sole live handle to the VM.
unsafe fn active_vm() -> Option<&'static mut Vm<'static>> {
    let p = ACTIVE_VM.load(Ordering::SeqCst);
    if p.is_null() {
        return None;
    }
    Some(&mut *(p as *mut Vm<'static>))
}

/// The event payload passed to a callback as trailing parameters.
enum Payload {
    None,
    Key(String),
    Click { x: i64, y: i64 },
}

/// Run a registered callback on the active VM, sharing the program's globals.
///
/// The callback is entered as a fresh top-level frame — exactly the way
/// `run()` enters `main` — so its trailing `OP_HALT`/`OP_RETURN` ends cleanly
/// without unwinding a non-existent caller. The VM's suspended state (it is
/// blocked inside `event_loop`) is saved before the call and restored after.
fn run_callback(fidx: u32, seed: Vec<Value>, payload: Payload) {
    let Some(vm) = (unsafe { active_vm() }) else {
        return;
    };
    let f = match vm.prog.funcs.get(fidx as usize) {
        Some(f) => f.clone(),
        None => return,
    };
    // Materialise the payload inside the VM's heap before suspending it.
    let extra: Vec<Value> = match payload {
        Payload::None => Vec::new(),
        Payload::Key(s) => vec![Value::Obj(vm.new_str_obj(s.into_bytes()))],
        Payload::Click { x, y } => vec![Value::I64(x), Value::I64(y)],
    };
    let saved = Suspended {
        pc: vm.pc,
        code: std::mem::take(&mut vm.code),
        frame_obj: vm.frame_obj,
        fname: std::mem::take(&mut vm.fname),
        stack: std::mem::take(&mut vm.stack),
        frames: std::mem::take(&mut vm.frames),
        pending: vm.pending.take(),
    };
    let mut locals = vec![Value::Null; f.nlocals as usize];
    for (i, v) in seed.into_iter().chain(extra).enumerate() {
        if i < locals.len() {
            locals[i] = v;
        }
    }
    let fobj = vm.alloc(HeapObj::new(ObjKind::Frame, &f.name, locals));
    vm.frame_obj = fobj;
    vm.code = f.code;
    vm.pc = 0;
    vm.fname = f.name.clone();
    vm.stack.clear();
    vm.frames.clear();
    vm.pending = None;
    if let Err(e) = vm.execute() {
        eprintln!("pointerses callback error: {e}");
    }
    vm.pc = saved.pc;
    vm.code = saved.code;
    vm.frame_obj = saved.frame_obj;
    vm.fname = saved.fname;
    vm.stack = saved.stack;
    vm.frames = saved.frames;
    vm.pending = saved.pending;
}

/// Fire the key callback registered on window `wid` (if any).
fn fire_key(wid: usize, key: &str) {
    let cb = windows()
        .get(wid)
        .and_then(|w| w.on_key.as_ref().cloned());
    if let Some(cb) = cb {
        run_callback(cb.fidx, cb.seed, Payload::Key(key.to_string()));
    }
}

/// Fire the click callback registered on window `wid` (if any).
fn fire_click(wid: usize, x: i64, y: i64) {
    let cb = windows()
        .get(wid)
        .and_then(|w| w.on_click.as_ref().cloned());
    if let Some(cb) = cb {
        run_callback(cb.fidx, cb.seed, Payload::Click { x, y });
    }
}

// -----------------------------------------------------------------
// Dispatch
// -----------------------------------------------------------------

/// Convert a string argument to a Rust `String`.
fn str_arg(heap: &[HeapObj], v: Option<&Value>) -> String {
    match v {
        Some(x) => String::from_utf8_lossy(&to_string_bytes(heap, x)).into_owned(),
        None => String::new(),
    }
}

/// Dispatch a GUI builtin: window management, canvas drawing, callbacks and
/// the blocking event loop. `rgb(r, g, b)` packs a 0xRRGGBB colour.
pub fn call_gui(vm: &mut Vm, name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "rgb" => {
            let r = num(&args.get(0), &vm.heap)?.clamp(0, 255);
            let g = num(&args.get(1), &vm.heap)?.clamp(0, 255);
            let b = num(&args.get(2), &vm.heap)?.clamp(0, 255);
            Ok(Value::I64((r << 16) | (g << 8) | b))
        }
        "window" => {
            let title = str_arg(&vm.heap, args.get(0));
            let w = num(&args.get(1), &vm.heap)?.max(1) as i32;
            let h = num(&args.get(2), &vm.heap)?.max(1) as i32;
            let native = platform::create(&title, w, h)?;
            let id = {
                let mut cs = windows();
                let id = cs.len();
                cs.push(WinState {
                    title,
                    w,
                    h,
                    bg: 0xffffffff,
                    paints: Vec::new(),
                    on_key: None,
                    on_click: None,
                    native,
                    closed: false,
                });
                id
            };
            Ok(Value::I64(id as i64))
        }
        "window_close" => {
            let wid = num(&args.get(0), &vm.heap)? as usize;
            let native = match windows().get(wid) {
                Some(w) => w.native,
                None => return Err(format!("unknown window id {wid}")),
            };
            platform::close(native);
            Ok(Value::Null)
        }
        "window_title" => {
            let wid = num(&args.get(0), &vm.heap)? as usize;
            let title = str_arg(&vm.heap, args.get(1));
            let native = match windows().get_mut(wid) {
                Some(w) => {
                    w.title = title.clone();
                    w.native
                }
                None => return Err(format!("unknown window id {wid}")),
            };
            platform::set_title(native, &title);
            Ok(Value::Null)
        }
        "clear_canvas" => {
            let wid = num(&args.get(0), &vm.heap)? as usize;
            let native = match windows().get_mut(wid) {
                Some(w) => {
                    w.paints.clear();
                    w.native
                }
                None => return Err(format!("unknown window id {wid}")),
            };
            platform::invalidate(native);
            Ok(Value::Null)
        }
        "draw_text" => {
            let wid = num(&args.get(0), &vm.heap)? as usize;
            let x = num(&args.get(1), &vm.heap)? as i32;
            let y = num(&args.get(2), &vm.heap)? as i32;
            let text = str_arg(&vm.heap, args.get(3));
            let color = num(&args.get(4), &vm.heap).unwrap_or(0xff000000);
            let cmd = PaintCmd::Text { x, y, text, color };
            let native = match windows().get_mut(wid) {
                Some(w) => {
                    w.paints.push(cmd);
                    w.native
                }
                None => return Err(format!("unknown window id {wid}")),
            };
            platform::invalidate(native);
            Ok(Value::Null)
        }
        "draw_rect" | "fill_rect" => {
            let wid = num(&args.get(0), &vm.heap)? as usize;
            let x = num(&args.get(1), &vm.heap)? as i32;
            let y = num(&args.get(2), &vm.heap)? as i32;
            let rw = num(&args.get(3), &vm.heap)? as i32;
            let rh = num(&args.get(4), &vm.heap)? as i32;
            let color = num(&args.get(5), &vm.heap).unwrap_or(0xff000000);
            let cmd = PaintCmd::Rect {
                x,
                y,
                w: rw,
                h: rh,
                color,
                filled: name == "fill_rect",
            };
            let native = match windows().get_mut(wid) {
                Some(w) => {
                    w.paints.push(cmd);
                    w.native
                }
                None => return Err(format!("unknown window id {wid}")),
            };
            platform::invalidate(native);
            Ok(Value::Null)
        }
        "on_key" | "on_click" => {
            let wid = num(&args.get(0), &vm.heap)? as usize;
            let (fidx, seed) = match args.get(1) {
                Some(Value::Obj(id)) => {
                    let o = &vm.heap[*id as usize];
                    let fidx = o
                        .closure_fn
                        .ok_or_else(|| format!("`{name}` expects a function handler"))?;
                    (fidx, o.fields.clone())
                }
                _ => return Err(format!("`{name}` expects a function handler")),
            };
            let cb = Callback { fidx, seed };
            let _native = match windows().get_mut(wid) {
                Some(w) => {
                    if name == "on_key" {
                        w.on_key = Some(cb);
                    } else {
                        w.on_click = Some(cb);
                    }
                    w.native
                }
                None => return Err(format!("unknown window id {wid}")),
            };
            Ok(Value::Null)
        }
        "event_loop" => {
            set_active_vm(vm);
            let r = platform::event_loop();
            ACTIVE_VM.store(std::ptr::null_mut(), Ordering::SeqCst);
            r.map(|_| Value::Null)
        }
        _ => Err(format!("unknown GUI call `{name}`")),
    }
}

#[cfg(windows)]
mod platform {
    pub use super::win32::{
        close, create, event_loop, invalidate, paint, set_title,
    };
}

#[cfg(not(windows))]
mod platform {
    pub use super::x11::{close, create, event_loop, invalidate, paint, set_title};
}

// -----------------------------------------------------------------
// Win32 backend
// -----------------------------------------------------------------
#[cfg(windows)]
mod win32 {
    use super::*;
    use std::ffi::{c_char, c_void};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleA(h: *const c_char) -> *mut c_void;
        fn GetLastError() -> u32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn RegisterClassA(cls: *const WndClassA) -> u16;
        fn CreateWindowExA(
            exstyle: u32,
            cls: *const c_char,
            title: *const c_char,
            style: u32,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            parent: *mut c_void,
            menu: *mut c_void,
            inst: *mut c_void,
            param: *mut c_void,
        ) -> *mut c_void;
        fn DefWindowProcA(hwnd: *mut c_void, msg: u32, wp: usize, lp: isize) -> isize;
        fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
        fn UpdateWindow(hwnd: *mut c_void) -> i32;
        fn GetMessageA(msg: *mut Msg, hwnd: *mut c_void, min: u32, max: u32) -> i32;
        fn TranslateMessage(msg: *const Msg) -> i32;
        fn DispatchMessageA(msg: *const Msg) -> isize;
        fn PostMessageA(hwnd: *mut c_void, msg: u32, wp: usize, lp: isize) -> i32;
        fn PostQuitMessage(code: i32);
        fn SetWindowTextA(hwnd: *mut c_void, s: *const c_char) -> i32;
        fn InvalidateRect(hwnd: *mut c_void, rect: *const c_void, erase: i32) -> i32;
        fn GetDC(hwnd: *mut c_void) -> *mut c_void;
        fn ReleaseDC(hwnd: *mut c_void, dc: *mut c_void) -> i32;
        fn EndPaint(hwnd: *mut c_void, ps: *const PaintStruct) -> i32;
        fn BeginPaint(hwnd: *mut c_void, ps: *mut PaintStruct) -> *mut c_void;
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn CreateSolidBrush(color: u32) -> *mut c_void;
        fn DeleteObject(obj: *mut c_void) -> i32;
        fn GetStockObject(objid: u32) -> *mut c_void;
        fn CreatePen(style: i32, width: i32, color: u32) -> *mut c_void;
        fn SelectObject(dc: *mut c_void, obj: *mut c_void) -> usize;
        fn Rectangle(dc: *mut c_void, left: i32, top: i32, right: i32, bottom: i32) -> i32;
        fn CreateFontA(
            height: i32,
            width: i32,
            rotation: i32,
            direction: i32,
            weight: u32,
            italic: i32,
            underline: i32,
            strike: i32,
            charset: u32,
            outprec: u32,
            clip: u32,
            quality: u32,
            pitch: u32,
            face: *const c_char,
        ) -> *mut c_void;
        fn ExtTextOutA(
            dc: *mut c_void,
            x: i32,
            y: i32,
            flags: u32,
            rect: *const c_void,
            text: *const c_char,
            len: u32,
            rects: *mut c_void,
        ) -> i32;
        fn SetTextColor(dc: *mut c_void, color: u32) -> u32;
        fn SetBkMode(dc: *mut c_void, mode: i32) -> i32;
        fn SetTextAlign(dc: *mut c_void, align: u32) -> u32;
    }

    /// The `WNDPROC` signature, stored pointer-sized in `WNDCLASSA.lpfnWndProc`.
    type WndProc =
        unsafe extern "system" fn(hwnd: *mut c_void, msg: u32, wp: usize, lp: isize) -> isize;

    // Field names follow the Windows `WNDCLASSA` spelling; the layout is what
    // matters, not the casing.
    #[repr(C)]
    #[allow(non_snake_case)]
    struct WndClassA {
        style: u32,
        lpfnWndProc: WndProc,
        cbClsExtra: i32,
        cbWndExtra: i32,
        hInstance: *mut c_void,
        hIcon: *mut c_void,
        hCursor: *mut c_void,
        hbrBackground: *mut c_void,
        lpszMenuName: *const c_char,
        lpszClassName: *const c_char,
    }

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct Msg {
        hwnd: *mut c_void,
        message: u32,
        wp: usize,
        lp: isize,
        time: u32,
        pt: Point,
    }

    #[repr(C)]
    struct PaintStruct {
        rect: Rect,
        erase: i32,
        restore: i32,
        update: i32,
        reserved: [u8; 32],
    }

    const WM_DESTROY: u32 = 0x0002;
    const WM_CLOSE: u32 = 0x0010;
    const WM_PAINT: u32 = 0x000f;
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_CHAR: u32 = 0x0102;
    const WM_IME_CHAR: u32 = 0x010f;
    const SW_SHOW: i32 = 5;
    const WS_OVERLAPPEDWINDOW: u32 = 0x00cf0000;
    const TRANSPARENT: i32 = 1;
    const TA_LEFT: u32 = 0x0001;
    /// Stock object ids (`GetStockObject` arguments).
    const NULL_BRUSH: u32 = 5;
    const NULL_PEN: u32 = 8;
    const CLASS_NAME: &[u8] = b"PointerSesWindow\0";
    const FONT_FACE: &[u8] = b"Arial\0";

    /// Win32 `COLORREF` is 0x00BBGGRR; the program's colour is 0xRRGGBB.
    /// Only `B` needs to move (byte 0 -> byte 2); `R` and `G` are already
    /// in place.
    fn colorref(c: i64) -> u32 {
        let c = (c as u32) & 0x00ffff_ffff;
        ((c >> 16) & 0xff) | (c & 0xff00) | ((c & 0xff) << 16)
    }

    /// A NUL-terminated copy of `s`; the caller must keep it alive.
    fn cstr(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    pub fn create(title: &str, w: i32, h: i32) -> Result<usize, String> {
        unsafe {
            let inst = GetModuleHandleA(std::ptr::null());
            let bg = CreateSolidBrush(0x00ffffff);
            let cls = WndClassA {
                style: 0,
                lpfnWndProc: wnd_proc,
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: inst,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: bg,
                lpszMenuName: std::ptr::null(),
                lpszClassName: CLASS_NAME.as_ptr() as *const c_char,
            };
            let atom = RegisterClassA(&cls);
            if atom == 0 {
                return Err(format!("RegisterClassA failed (error {})", GetLastError()));
            }
            let t = cstr(title);
            let hwnd = CreateWindowExA(
                0,
                CLASS_NAME.as_ptr() as *const c_char,
                t.as_ptr() as *const c_char,
                WS_OVERLAPPEDWINDOW,
                -1,
                -1,
                w,
                h,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                inst,
                std::ptr::null_mut(),
            );
            if hwnd.is_null() {
                return Err(format!(
                    "CreateWindowExA failed (error {})",
                    GetLastError()
                ));
            }
            let _ = ShowWindow(hwnd, SW_SHOW);
            Ok(hwnd as usize)
        }
    }

    pub fn close(native: usize) {
        unsafe {
            let _ = PostMessageA(native as *mut c_void, WM_CLOSE, 0, 0);
        }
    }

    pub fn set_title(native: usize, title: &str) {
        let t = cstr(title);
        unsafe {
            let _ = SetWindowTextA(native as *mut c_void, t.as_ptr() as *const c_char);
        }
    }

    /// Mark the window dirty; the next paint replays the recorded canvas.
    pub fn invalidate(native: usize) {
        unsafe {
            let _ = InvalidateRect(native as *mut c_void, std::ptr::null_mut(), 1);
        }
    }

    pub fn paint(
        _native: usize,
        dc: *mut c_void,
        w: i32,
        h: i32,
        bg: i64,
        paints: &[PaintCmd],
    ) {
        if dc.is_null() {
            return;
        }
        unsafe {
            let pen = GetStockObject(NULL_PEN);
            let brush = CreateSolidBrush(colorref(bg));
            if !brush.is_null() {
                let _ = SelectObject(dc, brush);
                let _ = SelectObject(dc, pen);
                let _ = Rectangle(dc, 0, 0, w, h);
                DeleteObject(brush);
            }
            for p in paints {
                match p {
                    PaintCmd::Text { x, y, text, color } => {
                        let font = CreateFontA(
                            -16,
                            0,
                            0,
                            0,
                            400,
                            0,
                            0,
                            0,
                            1,
                            0,
                            0,
                            0,
                            0,
                            FONT_FACE.as_ptr() as *const c_char,
                        );
                        let _ = SelectObject(dc, font);
                        SetTextColor(dc, colorref(*color));
                        SetBkMode(dc, TRANSPARENT);
                        SetTextAlign(dc, TA_LEFT);
                        let b = text.as_bytes();
                        let _ = ExtTextOutA(
                            dc,
                            *x,
                            *y,
                            0,
                            std::ptr::null_mut(),
                            b.as_ptr() as *const c_char,
                            b.len() as u32,
                            std::ptr::null_mut(),
                        );
                        if !font.is_null() {
                            DeleteObject(font);
                        }
                    }
                    PaintCmd::Rect { x, y, w: rw, h: rh, color, filled } => {
                        if *filled {
                            // Solid fill: the current brush paints, a null pen
                            // suppresses the outline `Rectangle` would add.
                            let pen = GetStockObject(NULL_PEN);
                            let b = CreateSolidBrush(colorref(*color));
                            if !b.is_null() {
                                let _ = SelectObject(dc, b);
                                let _ = SelectObject(dc, pen);
                                let _ = Rectangle(dc, *x, *y, *x + *rw, *y + *rh);
                                DeleteObject(b);
                            }
                        } else {
                            // Outline only: a null brush keeps the interior clear.
                            let brush = GetStockObject(NULL_BRUSH);
                            let pen = CreatePen(0, 2, colorref(*color));
                            if !pen.is_null() {
                                let _ = SelectObject(dc, brush);
                                let _ = SelectObject(dc, pen);
                                let _ = Rectangle(dc, *x, *y, *x + *rw, *y + *rh);
                                DeleteObject(pen);
                            }
                        }
                    }
                }
            }
        }
    }

    /// A human-readable name for a `WM_CHAR` / `WM_IME_CHAR` character code.
    ///
    /// Character keys are decoded from the character code because it already
    /// reflects the keyboard layout and the shift state. Characters routed
    /// through an input method editor arrive as `WM_IME_CHAR` instead of
    /// `WM_CHAR`, so both are reported by the same arm.
    fn char_name(c: u32) -> String {
        match c {
            0x20 => "space".into(),
            0x0d => "enter".into(),
            0x09 => "tab".into(),
            0x08 => "backspace".into(),
            _ if c >= 32 && c < 127 => (c as u8 as char).to_string(),
            // Anything else — for example the `0x1B` code Windows delivers
            // for `escape` — has already been reported by `WM_KEYDOWN`, so
            // reporting it here too would double-fire.
            _ => String::new(),
        }
    }

    /// A key with no character code, reported from `WM_KEYDOWN`.
    ///
    /// These keys never produce a `WM_CHAR`, so decoding them from the
    /// character stream would silently drop them.
    fn keydown_name(c: u32) -> Option<&'static str> {
        match c {
            0x25 => Some("left"),
            0x26 => Some("up"),
            0x27 => Some("right"),
            0x28 => Some("down"),
            0x1b => Some("escape"),
            0x2d => Some("delete"),
            _ => None,
        }
    }

    /// The window procedure: paint, mouse clicks, keys and close.
    extern "system" fn wnd_proc(hwnd: *mut c_void, msg: u32, wp: usize, lp: isize) -> isize {
        match msg {
            WM_PAINT => {
                // `BeginPaint` marks the invalidated region as painted and
                // returns a DC whose origin is the client area; without it
                // Windows re-issues `WM_PAINT` forever and floods the queue.
                let mut ps: PaintStruct = unsafe { std::mem::zeroed() };
                let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
                if !hdc.is_null() {
                    if let Some(wid) = wid_of_native(hwnd as usize) {
                        paint_wid(wid, hdc);
                    }
                }
                let _ = unsafe { EndPaint(hwnd, &ps) };
                0
            }
            WM_LBUTTONDOWN => {
                let x = (lp as u32 & 0xffff) as i32;
                let y = ((lp as u32 >> 16) & 0xffff) as i32;
                if let Some(wid) = wid_of_native(hwnd as usize) {
                    fire_click(wid, x as i64, y as i64);
                }
                0
            }
            WM_CHAR | WM_IME_CHAR => {
                let name = char_name(wp as u32);
                if !name.is_empty() {
                    if let Some(wid) = wid_of_native(hwnd as usize) {
                        fire_key(wid, &name);
                    }
                }
                0
            }
            WM_KEYDOWN => {
                if let Some(name) = keydown_name(wp as u32) {
                    if let Some(wid) = wid_of_native(hwnd as usize) {
                        fire_key(wid, name);
                    }
                }
                unsafe { DefWindowProcA(hwnd, msg, wp, lp) }
            }
            WM_DESTROY => {
                if let Some(wid) = wid_of_native(hwnd as usize) {
                    mark_closed(wid);
                }
                unsafe {
                    PostQuitMessage(0);
                }
                0
            }
            _ => unsafe { DefWindowProcA(hwnd, msg, wp, lp) },
        }
    }

    /// Pump messages until the last window is closed (`WM_QUIT`).
    pub fn event_loop() -> Result<(), String> {
        if windows_open() == 0 {
            return Ok(());
        }
        let mut msg: Msg = unsafe { std::mem::zeroed() };
        loop {
            let r = unsafe { GetMessageA(&mut msg, std::ptr::null_mut(), 0, 0) };
            if r == -1 {
                return Err("Win32 GetMessage failed".into());
            }
            if r == 0 {
                break; // `WM_QUIT` posted by the last `WM_DESTROY`.
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageA(&msg);
            }
        }
        Ok(())
    }
}

// -----------------------------------------------------------------
// X11 backend
// -----------------------------------------------------------------
#[cfg(not(windows))]
mod x11 {
    use super::*;
    use std::ffi::{c_char, c_void};

    /// `XEvent` is a union; the buffer is conservatively oversized.
    const XEVT: usize = 132;

    /// The shared X display connection (opened by the first `create`).
    static DISPLAY: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    #[link(name = "X11")]
    extern "C" {
        fn XOpenDisplay(display: *const c_char) -> *mut c_void;
        fn XDefaultRootWindow(d: *mut c_void) -> usize;
        fn XCreateSimpleWindow(
            d: *mut c_void,
            parent: usize,
            x: i32,
            y: i32,
            w: u32,
            h: u32,
            border: u32,
            border_color: usize,
            background: usize,
        ) -> usize;
        fn XStoreName(d: *mut c_void, w: usize, name: *const c_char);
        fn XMapWindow(d: *mut c_void, w: usize);
        fn XFlush(d: *mut c_void);
        fn XNextEvent(d: *mut c_void, e: *mut c_void);
        fn XCreateGC(d: *mut c_void, w: usize, valuemask: u32, values: *mut c_void) -> usize;
        fn XFreeGC(d: *mut c_void, gc: usize);
        fn XSetForeground(d: *mut c_void, gc: usize, color: usize);
        fn XFillRectangle(
            d: *mut c_void,
            w: usize,
            gc: usize,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
        );
        fn XDrawRectangle(
            d: *mut c_void,
            w: usize,
            gc: usize,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
        );
        fn XDrawString(
            d: *mut c_void,
            w: usize,
            gc: usize,
            x: i32,
            y: i32,
            s: *const c_char,
            len: i32,
        );
        fn XFilterEvent(ev: *mut c_void, w: usize) -> i32;
        fn XDestroyWindow(d: *mut c_void, w: usize);
        fn XSelectInput(d: *mut c_void, w: usize, mask: usize);
        fn XInternAtom(d: *mut c_void, name: *const c_char, only_exists: i32) -> usize;
        fn XSetWMProtocols(d: *mut c_void, w: usize, atoms: *mut usize, count: i32);
        fn XLookupString(
            ev: *const c_void,
            buf: *mut c_char,
            n: i32,
            keysym: *mut u32,
            status: *mut c_void,
        ) -> i32;
        fn XSetInputFocus(d: *mut c_void, w: usize, order: i32, time: u32);
        fn XClearArea(
            d: *mut c_void,
            w: usize,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
            erase: i32,
        );
    }

    /// A raw `XEvent` buffer with field access by byte offset.
    struct XEv {
        buf: [u8; XEVT],
    }

    impl XEv {
        fn zeroed() -> Self {
            XEv { buf: [0u8; XEVT] }
        }
        fn ptr(&mut self) -> *mut c_void {
            self.buf.as_mut_ptr() as *mut c_void
        }
        fn ty(&self) -> i32 {
            i32::from_ne_bytes(self.buf[0..4].try_into().unwrap())
        }
        fn i32_at(&self, off: usize) -> i32 {
            i32::from_ne_bytes(self.buf[off..off + 4].try_into().unwrap())
        }
        fn usize_at(&self, off: usize) -> usize {
            usize::from_ne_bytes(self.buf[off..off + 8].try_into().unwrap())
        }
    }

    /// An X11 pixel value for a TrueColor display: 0xRRGGBB.
    fn pixel(c: i64) -> usize {
        (c as usize) & 0x00ffff_ffff
    }

    /// The current display connection, or null.
    fn display() -> *mut c_void {
        DISPLAY.load(Ordering::SeqCst)
    }

    pub fn create(title: &str, w: i32, h: i32) -> Result<usize, String> {
        let disp = if display().is_null() {
            let d = unsafe { XOpenDisplay(std::ptr::null()) };
            if d.is_null() {
                return Err("cannot open X display".into());
            }
            DISPLAY.store(d, Ordering::SeqCst);
            d
        } else {
            display()
        };
        let root = unsafe { XDefaultRootWindow(disp) };
        let win = unsafe {
            XCreateSimpleWindow(
                disp,
                root,
                0,
                0,
                w.max(1) as u32,
                h.max(1) as u32,
                1,
                0,
                u32::MAX as usize,
            )
        };
        if win == 0 {
            return Err("XCreateSimpleWindow failed".into());
        }
        let t = {
            let mut v = title.as_bytes().to_vec();
            v.push(0);
            v
        };
        unsafe {
            XStoreName(disp, win, t.as_ptr() as *const c_char);
            // Expose | KeyPress | KeyRelease | ButtonPress | Substructure | Structure
            let mask = (1 << 12) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 17) | (1 << 16);
            XSelectInput(disp, win, mask);
            let mut wm = XInternAtom(
                disp,
                b"WM_DELETE_WINDOW\0".as_ptr() as *const c_char,
                0,
            );
            XSetWMProtocols(disp, win, &mut wm as *mut usize, 1);
            XSetInputFocus(disp, win, 1, 0);
            XMapWindow(disp, win);
            XFlush(disp);
        }
        Ok(win)
    }

    pub fn close(native: usize) {
        if let Some(wid) = wid_of_native(native) {
            mark_closed(wid);
        }
        let disp = display();
        if disp.is_null() {
            return;
        }
        unsafe {
            XDestroyWindow(disp, native);
            XFlush(disp);
        }
    }

    pub fn set_title(native: usize, title: &str) {
        let disp = display();
        if disp.is_null() {
            return;
        }
        let mut t = title.as_bytes().to_vec();
        t.push(0);
        unsafe {
            XStoreName(disp, native, t.as_ptr() as *const c_char);
            XFlush(disp);
        }
    }

    /// Request a repaint; the `Expose` handler redraws the recorded canvas.
    pub fn invalidate(native: usize) {
        let disp = display();
        if disp.is_null() {
            return;
        }
        unsafe {
            XClearArea(disp, native, 0, 0, u32::MAX, u32::MAX, 0);
            XFlush(disp);
        }
    }

    pub fn paint(
        native: usize,
        _dc: *mut c_void,
        w: i32,
        h: i32,
        bg: i64,
        paints: &[PaintCmd],
    ) {
        let disp = display();
        if disp.is_null() {
            return;
        }
        let gc = unsafe { XCreateGC(disp, native, 0, std::ptr::null_mut()) };
        if gc == 0 {
            return;
        }
        unsafe {
            XSetForeground(disp, gc, pixel(bg));
            XFillRectangle(disp, native, gc, 0, 0, w.max(1) as u32, h.max(1) as u32);
            for p in paints {
                match p {
                    PaintCmd::Text { x, y, text, color } => {
                        XSetForeground(disp, gc, pixel(*color));
                        let b = text.as_bytes();
                        XDrawString(
                            disp,
                            native,
                            gc,
                            *x,
                            *y,
                            b.as_ptr() as *const c_char,
                            b.len() as i32,
                        );
                    }
                    PaintCmd::Rect { x, y, w: rw, h: rh, color, filled } => {
                        XSetForeground(disp, gc, pixel(*color));
                        if *filled {
                            XFillRectangle(disp, native, gc, *x, *y, (*rw).max(1) as u32, (*rh).max(1) as u32);
                        } else {
                            XDrawRectangle(disp, native, gc, *x, *y, (*rw).max(1) as u32, (*rh).max(1) as u32);
                        }
                    }
                }
            }
            XFlush(disp);
            XFreeGC(disp, gc);
        }
    }

    /// A named key, from a raw `XKeyEvent` buffer.
    fn key_name(ev: &XEv) -> String {
        let kc = ev.i32_at(76) as u32;
        match kc {
            65 => return "space".into(),   // XK_space
            36 => return "enter".into(),   // XK_Return
            23 => return "tab".into(),     // XK_Tab
            9 => return "backspace".into(),
            113 => return "left".into(),
            116 => return "up".into(),
            114 => return "right".into(),
            117 => return "down".into(),
            _ => {}
        }
        let disp = display();
        if disp.is_null() {
            return format!("key{kc}");
        }
        // `c_char` is `i8` on x86_64-linux but `u8` on aarch64-linux, so the
        // buffer must be typed as `c_char` rather than `i8` to match the Xlib
        // signature on both targets.
        let mut buf = [0 as c_char; 4];
        let mut keysym: u32 = 0;
        let mut status: i32 = 0;
        let n = unsafe {
            XLookupString(
                ev.buf.as_ptr() as *const c_void,
                buf.as_mut_ptr(),
                4,
                &mut keysym,
                &mut status as *mut i32 as *mut c_void,
            )
        };
        if n > 0 && buf[0] >= 32 && buf[0] < 127 {
            return (buf[0] as u8 as char).to_string();
        }
        format!("key{kc}")
    }

    /// Pump events until every window is closed.
    pub fn event_loop() -> Result<(), String> {
        let disp = display();
        if disp.is_null() {
            return Err("cannot open X display".into());
        }
        let mut ev = XEv::zeroed();
        loop {
            unsafe {
                XNextEvent(disp, ev.ptr());
            }
            let ty = ev.ty();
            let wt = ev.usize_at(24);
            // Let the window manager drop events it has consumed.
            if unsafe { XFilterEvent(ev.ptr(), wt) } == 1 {
                if windows_open() == 0 {
                    break;
                }
                continue;
            }
            if let Some(wid) = wid_of_native(wt) {
                match ty {
                    2 => {
                        // KeyPress
                        let key = key_name(&ev);
                        fire_key(wid, &key);
                    }
                    4 => {
                        // ButtonPress
                        let x = ev.i32_at(56) as i64;
                        let y = ev.i32_at(60) as i64;
                        fire_click(wid, x, y);
                    }
                    12 => {
                        // Expose
                        paint_wid(wid, std::ptr::null_mut());
                    }
                    17 => {
                        // DestroyNotify (the destroyed window is at offset 32).
                        let dw = ev.usize_at(32);
                        if let Some(dwid) = wid_of_native(dw) {
                            mark_closed(dwid);
                        }
                    }
                    18 => {
                        // ClientMessage: `WM_DELETE_WINDOW` from the title bar.
                        mark_closed(wid);
                        unsafe {
                            XDestroyWindow(disp, wt);
                            XFlush(disp);
                        }
                    }
                    33 => {
                        // ConfigureNotify: track the new canvas size.
                        let nw = ev.usize_at(56) as i32;
                        let nh = ev.usize_at(60) as i32;
                        let mut cs = windows();
                        if let Some(w) = cs.get_mut(wid) {
                            w.w = nw;
                            w.h = nh;
                        }
                    }
                    _ => {}
                }
            }
            if windows_open() == 0 {
                break;
            }
        }
        Ok(())
    }
}
