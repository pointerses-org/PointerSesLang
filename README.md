# Pointerses

Pointerses 是一门**系统向的小型编程语言**，内置显式指针系统、区域演算（region
calculus）内存模型、严格静态类型推断、并发注解、常驻守护进程与可选 LLVM 后端，
并自带原生 GUI（Win32 / X11 窗口与画布）与 TUI 终端界面。
参考编译器用 Rust 编写，**零外部 crate 依赖**（仅标准库），可完全离线构建。

## 核心特性

- **指针系统**——代数路径指针（`&v` / `&mut v` / `*p`），隐式解引用、按 VType 大小
  的指针偏移代数，编译期非法偏移诊断。
- **区域演算内存模型**——`Stack` / `Scoped` / `Heap` 三级区域，堆对象用非阻塞引用计数
  （`AtomicU32`），指针生命周期由区域演算校验，防止悬垂。
- **泛型 / 模块 / trait**——泛型结构体与函数（类型擦除、免单态化）、`module`/`import`
  命名空间、`trait`/`impl` 静态分派 + `dyn Trait` / `T: Trait` 运行时动态分派。
- **异常与插值**——`throw`/`try`/`catch`/`finally`（含跨函数展开与 finally 全覆盖语义）、
  字符串插值 `$ident` / `${expr}`。
- **集合**——数组、`List[T]`、`Map[K, V]`（引用计数堆对象）。
- **并发注解**——`@Auto`（M:N 协程）、`@Manual(fixed=N)`（固定线程池），真实 OS 线程执行。
- **原生执行**——`pssc --file-exe` 产出独立原生可执行文件（内嵌完整 VM 运行时），
  支持跨架构 x64 / x32 / arm64。
- **工程配置**——`.pspc` 声明工程名、版本、git 依赖与 **watchdog**（宿主级进程自动重启）。
- **错误屏**——项目报错时弹出可视错误屏（原生对话框 / ANSI 终端屏），普通用户可直接
  看到并上报错误；`pss task pointerses-error-screen -e "..."` 可手动调用。
- **原生 GUI**——语言内建窗口与画布（Windows 走 Win32、Linux 走 X11，纯 std + 系统 API），
  支持绘图、按键回调与鼠标点击回调，画布状态保存在 VM 顶层全局变量中，回调在 VM 主线程
  上执行，可直接读写顶层变量。
- **TUI 终端界面**——纯 std 直写 ANSI 转义序列：清屏、光标定位、256 色、`key()` 阻塞读键
  （方向键给 `up`/`down`/`left`/`right` 名字）、`terminal_cols()` / `terminal_rows()`
  自适应布局。

## 快速开始

```powershell
cargo build --release

# 运行综合示例（单文件，演示全部特性）
pss run examples/main.psp --no-daemon

# 终端界面（TUI）与窗口（GUI）示例
pss run examples/tui.psp --no-daemon
pss run examples/gui.psp  --no-daemon

# 直接执行单行代码（类似 python -c）
pss -c 'println("hello from -c")'

# 打包成原生可执行文件
pssc examples/main.psp --file-exe -o main.exe
```

## GUI 图形界面

语言内建窗口与画布，无需宿主 Rust 程序，也无需任何外部图形库（Windows 用 Win32，
Linux 用 X11）。内置函数：

| 函数 | 说明 |
| --- | --- |
| `window(title, w, h) -> int` | 创建窗口，返回窗口 id |
| `window_close(id)` | 关闭窗口 |
| `window_title(id, title)` | 修改窗口标题 |
| `clear_canvas(id)` | 清空画布 |
| `fill_rect(id, x, y, w, h, c)` | 画实心矩形 |
| `draw_rect(id, x, y, w, h, c)` | 画空心矩形 |
| `draw_text(id, x, y, text)` | 在 `(x, y)` 画文本 |
| `rgb(r, g, b) -> int` | 构造 `0xRRGGBB` 颜色值 |
| `on_key(id, func(k))` | 注册按键回调，`k` 为按键名 |
| `on_click(id, func(x, y))` | 注册鼠标点击回调，坐标为客户端坐标 |
| `event_loop()` | 进入事件循环，阻塞直到所有窗口关闭 |

`on_key` 的按键名：`space`、`enter`、`tab`、`backspace`、`escape`、`delete`、
`left` / `up` / `right` / `down`，以及字母与数字的原字符（已按键盘布局与 Shift 状态
解码）。方向键、`escape`、`delete` 没有字符码，从 `WM_KEYDOWN` 上报；其余字符从
`WM_CHAR` 上报（输入法组合出的字符走 `WM_IME_CHAR`，同样上报）。因此中文输入法处于
组合状态时，字母会被输入法接管——切换到英文输入模式即按原样接收。

示例：

```
// examples/gui.psp     点击移动方块、空格变色
// examples/gui_smoke.psp  窗口生命周期自检
// examples/cb_test.psp    按键 / 点击回调 + 顶层全局变量共享
pss run examples/gui.psp --no-daemon
```

## TUI 终端界面

纯 std 直接写 ANSI 转义序列，无需宿主 Rust 程序。内置函数：

| 函数 | 说明 |
| --- | --- |
| `clear_screen()` | 清屏并复位光标 |
| `cursor(x, y)` | 移动光标（1 基） |
| `color(fg, bg)` / `reset_color()` | ANSI 256 色前景/背景，或恢复默认 |
| `hide_cursor()` / `show_cursor()` | 隐藏 / 显示光标 |
| `key() -> str` | 阻塞读一个按键（方向键给 `up`/`down`/`left`/`right` 等名字） |
| `key_available() -> bool` | 是否有按键等待 |
| `terminal_cols()` / `terminal_rows()` | 终端列数 / 行数（检测不到时回退 80×24） |

`key()` 在输入流结束时返回 `"eof"`——用 `while true` 循环读按键时要自己处理这个分支，
否则会空转。

```
// examples/tui.psp    居中状态屏，↑/↓ 改计数，q 退出
pss run examples/tui.psp --no-daemon
```

## 文档

详细的开发文档见 **[docs.md](docs.md)**：工具链用法、语言参考（含 GUI / TUI 内建）、
编译管线、内存模型、工程配置、watchdog、FFI、并发与仓库布局等。

## 测试

```powershell
cargo test
```
