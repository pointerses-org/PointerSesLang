# Pointerses 开发文档

Pointerses 是一门系统向的小型编程语言：显式指针系统、区域演算（region calculus）
内存模型、严格静态类型推断、并发注解、常驻守护进程（快照快速启动）与可选 LLVM 后端，
并内置**泛型、模块 / import、trait / impl（静态分派 + dyn 动态分派）、异常处理
（try/catch/throw/finally）、字符串插值、原生 GUI（Win32 / X11 窗口与画布）与
TUI 终端界面**。参考编译器用 Rust 编写，**零外部 crate 依赖**（仅标准库），因此
`cargo build` 可完全离线工作。

本文档是面向开发的完整参考：工具链用法、语言规范（含 GUI / TUI 内建）、编译管线、
内存模型、并发、工程配置、watchdog、错误屏、FFI、打包格式、跨架构构建与仓库布局。

---

## 1. 总览与架构

工具链由四个命令组成（全部支持 `-v` / `--version`）：

| 命令 | 角色 | 产出 |
|---|---|---|
| `pss` | 运行器 | 运行 `.project` 包、直接编译运行 `.psp` 源码、`-c` 单行代码、`task` 内置任务、`daemon` 常驻服务 |
| `pssc` | 打包器 / 编译器驱动 | `.project` 包、Windows / Unix 原生可执行文件 |
| `pssp` | 反编译器 | `.project` → 还原源码 / 人类可读字节码列表 |
| `pssl` | 动态库打包器 | 整个工程 → `.psdl` 库 |

前端（`lexer` → `parser` → `semantic`）与后端（`codegen`）共享于同一个 lib crate
`pointerses`。程序既可在字节码 VM 中解释执行（默认 `native` 后端），也可打包成内嵌
完整 VM 运行时的独立原生可执行文件。

```
.psp 源码 ──► lexer ──► parser ──► semantic ──► codegen(bytecode/native)
                                                     │
                  .project 包 ◄── packaging ◄───────┤
                  原生 exe   ◄── rustc + 内嵌 VM ────┘
```

---

## 2. 构建与测试

```powershell
# 发布构建（生成 target\release\pss.exe / pssc.exe / pssp.exe / pssl.exe）
cargo build --release

# 更快的开发构建
cargo build            # target\debug\*

# 运行测试套件
cargo test
```

默认的 `native` 后端是**纯字节码 VM**，构建不需要任何外部工具链或网络。可选的 `llvm`
后端在 `--features llvm` 下编译，见「LLVM 后端」一节。

---

## 3. 工具链（CLI 参考）

### 3.1 `pss` —— 运行器

```
pss <file.project> [args...]        # 运行编译好的 .project 包（含架构校验）
pss <file.psp> [--no-daemon]        # 直接编译并运行 .psp 源码
pss run <file.psp> [--no-daemon]    # 同上（守护进程加速）
pss -c '<code>' [args...]           # 直接执行单行代码（类似 python -c）
pss task pointerses-error-screen -e "报错内容"  # 手动弹出错误屏
pss daemon [--port N]               # 常驻守护进程（内部使用）
pss -v | --version                  # 输出版本号
pss help
```

- `.project` / `.psdl` 包运行时先做**架构校验**：包的目标架构集合不含本机架构则拒绝执行。
- `.psp` 源码默认走守护进程快照的**快速启动路径**；`--no-daemon` 强制冷启动全量编译。
- watchdog 策略从同 basename 的 `.pspc` 读取（见第 9 节）。

### 3.2 `pssc` —— 打包器 / 编译器驱动

```
pssc <file.psp>                     # 打包成 .project（默认，由 pss 运行）
pssc <file.psp> --file-exe [-o out.exe]   # 打包成 Windows 原生可执行文件
pssc <file.psp> --file-x   [-o out]       # 打包成 Unix 原生可执行文件
pssc --project                [-o out]    # 打包当前目录为一个工程
pssc <file.psp> -b native|llvm           # 选择后端（默认 native）
pssc <file.psp> --emit-ir                 # 导出 LLVM IR（仅 llvm 后端）
pssc <file.psp> --snapshot | --selfhost   # 迁移自旧 `pss build` 的诊断
pssc <file.psp> --fram-x64 --fram-arm64   # 指定目标架构（可组合）
pssc <file.psp> --fram-all                # 全架构
pssc <file.psp> --no-api-check            # 构建 .project 时跳过调用存在性检查
pssc -v | --version                       # 输出版本号
```

- **`--no-api-check`**：构建 `.project`（字节码包，由 `pss` 运行）时**跳过构建期的
  "被调用函数是否存在"检查**——程序中可以调用既未定义、也未 `extern fn` 声明的名字，
  构建照常通过，调用被编译为 extern 调用，运行时由宿主 Rust 程序（`PSS_RUST_LIB`）
  提供该符号（见第 12 节）。**仅对 `.project` 打包有效**：与 `--file-exe` / `--file-x`
  一起使用会被拒绝（原生可执行文件没有宿主可提供 API）。也可以在 `.pspc` 里配置
  `no-api-check: true` 达到同样效果（见第 8 节）。

- **`--file-exe` / `--file-x`**：产出**真正的原生可执行文件**（不是自解压脚本）——把编译
  后的字节码嵌入一个小型 Rust 驱动，经 `rustc` 与完整 VM 运行时链接而成；`--file-x`
  产出的 Unix 可执行文件即使构建在 Windows 上也不带扩展名。
- **多架构**：`--fram-x64 --fram-x32 --fram-arm64` 可组合；一次调用会为每个目标架构
  各产出一个原生可执行文件，多架构时自动加 `_x64` / `_x32` / `_arm64` 后缀。
  跨架构用 `rustc --target` + zig `cc`（`tools/zigcc.rs` shim）链接，产出的是真正的
  COFF / ELF 二进制（见第 15 节）。
- **`--project`**：打包当前目录为一个工程；入口为 `main.psp`，否则目录里唯一的一个
  `.psp`，多个 `.psp` 且无 `main.psp` 时报错。
- **`--snapshot` / `--selfhost`**：来自旧 `pss build` 命令面的迁移诊断，分别写出
  `.snap.json` 快照与自举元数据。
- 原生可执行文件与 `.project` 包都会**内嵌 watchdog 配置**（若有）。

### 3.3 `pssp` —— 反编译器

```
pssp <file.project>                     # 打印字节码列表到 stdout
pssp <file.project> --out-psp <out.psp> # 还原原始 .psp 源码
pssp <file.project> --out-list <out.txt># 输出人类可读的字节码列表
pssp -v | --version
```

- 先打印一行摘要（源码名、目标架构、字节码字节数），再按输出选项写出结果。
- `--out-psp` 还原的是打包时保存的**原始源码文本**，可保真复原。

### 3.4 `pssl` —— 动态库打包器

```
pssl <dir> [-o out.psdl]    # 将整个工程打包成 .psdl 动态库
pssl -v | --version
```

- 入口解析规则与 `pssc --project` 相同（`main.psp` → 唯一 `.psp`）。
- 默认输出名取 `.pspc` 里的 `name`，否则取目录 basename。

### 3.5 版本与帮助

四个工具都支持 `-v` / `--version` 输出各自版本号。`pss` 额外支持 `pss help`。

---

## 4. 快速上手

仓库 `examples/` 下是**一个可直接编译运行的综合示例**：

- `examples/main.psp` —— 单文件，依次演示结构体、指针、闭包、集合、泛型、
  trait/impl、dyn trait 对象、错误处理、字符串插值、并发注解、FFI 共 11 组特性。
- `examples/main.pspc` —— 与 `main.psp` 同 basename 的工程配置（`name` / `version`，
  含 git `dependencies` 与 `watchdog` 块的注释示例）。
- `examples/native_rust/` —— FFI 配套的 Rust `cdylib` 示例（`rust_add` / `rust_mul`）。

```powershell
# 运行综合示例（单文件，不依赖任何辅助文件）
pss run examples/main.psp --no-daemon

# 打包成原生可执行文件
pssc examples/main.psp --file-exe -o main.exe

# 一次打出 x64 / x32 / arm64 三个原生 exe
pssc examples/main.psp --file-exe --fram-all

# 反编译还原
pssc examples/main.psp -o main.project
pssp main.project --out-psp main_restored.psp
```

`main.psp` 的预期输出（节选）：

```
1 struct: Alice (30)
2 ptr:    *p = 42
3 closure: add1(5) = 6
4 collections: arr=[1, 99, 3] [2 items] m={a: 1, b: 2}
5 generics: 42 / hi / pick=1 / right
6 static: Rex is 3 years old / Woof, I am Rex
7 dyn:    Tom says meow!! / bound: Rex is 3 years old
8 errors: caught: division by zero / finally: done
9 interp: n is 42, and 42 == 42
10 concurrent: 3^2 + (5+10) + (2+10) = 36
11 ffi:    add(2, 3) = 5
```

---

## 5. 语言参考

本文档从词法、类型、语句、表达式、注解、内置函数到声明逐项列出所有语言构造。

### 5.1 程序结构

一个 `.psp` 文件由**声明**（顶层）组成，顺序任意：

- `struct` 结构体定义（支持泛型 `struct Box[T]`）
- `fn` 函数定义（支持泛型 `fn id[T](x: T) -> T`）
- `trait` 特征定义（只声明方法签名）
- `impl` 实现块（`impl Trait for Type` 或 `impl Type`）
- `extern` 外部 FFI 声明
- `module 名称;` 命名空间声明（把本文件声明放进 `M::` 前缀）
- `import "path.psp";` 递归合并另一个源文件
- `@注解` 可放在声明之前

```pointerses
module Greet;
struct Point { x: int, y: int }
extern fn add(a: int, b: int) -> int

fn main() -> int {
  return 0
}
```

顶层不允许出现其它语句，否则报错。`main` 函数是可选的程序入口。

### 5.2 词法（Token）

**字面量**

| 类别 | 示例 |
|---|---|
| 整数 | `42`, `-7` |
| 浮点 | `3.14`, `-0.5` |
| 布尔 | `true`, `false` |
| 字符串 | `"hello"` |
| 插值字符串 | `"hi ${expr}"`, `"hi $name"` |
| 空值 | `null` |

**关键字**

```
fn let struct return if else while for true false null extern
mut region trait impl try catch finally throw module import
this
```

**运算符与标点**

```
{ } ( ) [ ] , . : :: ; =
+ - * / % & | !
< > == != <= >= && || ->  =>
@ $    // $ 用于字符串插值
```

**分隔符**：语句由**换行**、**分号 `;`** 或块结束 `}` 终止，均可省略分号：

```pointerses
let a = 1      // 换行即结束
let b = 2;     // 分号结束
```

> `=>`（胖箭头）当前为保留符号。`//` 为行注释，`/* ... */` 为块注释。

### 5.3 类型系统

**基本类型**

| 类型 | 说明 |
|---|---|
| `int` | 64 位整数（由 `Int` 字面量推断） |
| `float` | 浮点数 |
| `bool` | 布尔值 |
| `String` / `string` / `str` | 字符串 |
| `void` | 无返回值 |
| `null` | 空值 |

**复合 / 泛型类型**

| 类型 | 写法 | 说明 |
|---|---|---|
| 数组 | `[T]` | 定长数组（意图固定，存储为可变） |
| 列表 | `List[T]` | 可增长列表 |
| 映射 | `Map[K, V]` | 键值映射 |
| 函数类型 | `(A, B) -> R` | 函数 / 闭包签名 |
| 命名类型 | `Point` | 已定义的结构体名 |
| 泛型结构体 | `Box[T]` | 用户定义泛型结构体实例 |
| 类型变量 | `T` | 泛型函数 / impl 内的类型参数 |

**指针类型**

```pointerses
&int          // 不可变指针
&mut int      // 可变指针
&Point        // 指向结构体
```

指针是一等节点，携带**可变性**与**目标类型**。

### 5.4 语句（Statements）

**变量声明 `let`**

```pointerses
let name = expr          // 类型推断
let x: int = 42          // 显式类型
let list: List[int] = [] // 空列表需要类型注解
let p: &int = &x         // 指针变量
```

没有初始化器时默认赋值为 `null`（推断为对应类型）。

**表达式语句**

```pointerses
println("hi")
greet("")
x = x + 1
arr[1] = 99
```

**返回 `return`**

```pointerses
return          // void 函数
return expr     // 有返回值的函数
```

`return` 后紧跟换行 / 分号 / `}` 时视为无返回值返回。

**条件 `if / else if / else`**

```pointerses
if (cond) { ... }
else if (cond2) { ... }
else { ... }
```

`else` 后可接另一个 `if` 形成 `else if` 链。

**循环 `while` / `for`**

```pointerses
while (cond) { ... }

for (let i = 0; i < 5; i = i + 1) { ... }
```

`for` 的三个部分（初始化 / 条件 / 步进）均可省略。`break` / `continue` 可用于任何循环。

**错误处理 `try / catch / finally` 与 `throw`**

```pointerses
throw expr                    // 抛出任一值（字符串、数字、对象……）

try {
  risky()
} catch (e) {                 // e 绑定抛出的值
  println("caught: " + e)
} finally {
  cleanup()                   // 正常与捕获退出后都会执行
}
```

- `catch` 可省略（只保留 `finally`），`finally` 可省略。
- 异常在 VM 中以**处理器栈**实现：同函数 `throw` 直接进入 `catch`；未处理时**跨函数展开**，
  直到找到最近的调用者 `catch`；若全程无 `catch`，运行时报 `uncaught exception`。
- `finally` 在**任意**退出路径都执行：正常结束、`catch` 完成、`return` 和 `throw` 都会先
  经过 `finally` 再真正返回 / 继续抛出（嵌套 `finally` 按外层顺序依次执行）。`finally` 内
  的 `return` 会覆盖待返回的值（与 Java 一致）。

**注解指令 `@`**

语句级可放置单个注解（多用于 `@region` 类指令）。见第 5.8 节。

**块语句**

`{ ... }` 作为表达式求值时，其值等于**最后一个表达式**。

### 5.5 表达式（Expressions）

**字面量与引用**

```pointerses
42, 3.14, true, "text", null
变量名
```

**二元运算**（优先级从低到高）

1. `||`
2. `&&`
3. `==` `!=`
4. `<` `<=` `>` `>=`
5. `+` `-`
6. `*` `/` `%`

**一元运算**

```pointerses
-x        // 取负
!x        // 逻辑非
&x        // 取地址（不可变指针）
&mut x    // 取可变地址
*p        // 解引用
```

`&x` / `&mut x` / `*p` 构成**代数路径指针表达式**（一等指针节点）。

**赋值**

```pointerses
lhs = rhs
```

左值可以是标识符、字段、索引（`arr[i]`、`m["k"]`）。

**函数 / 闭包调用**

```pointerses
add(2, 3)
f(x)(y)          // 后置调用
```

**模块路径**

```pointerses
Greet::hello("world")             // 调用命名空间函数
Greet::Point { x: 3, y: 4 }       // 命名空间结构体字面量
```

`::` 把路径段拼接成完整名字（`M::name`）。同一文件内的短名 `name` 仍可用。

**泛型调用 / 字面量**

```pointerses
pick[int](1, 2, true)             // 显式类型实参调用
first([7, 8, 9], -1)              // 从实参推断类型参数
Box[int] { value: 42, tag: "int" } // 泛型结构体字面量
```

**字符串插值**

```pointerses
let name = "Pointerses"
let n = 42
println("hello $name!")            // $ident 直接插入
println("sum = ${10 + 32}")        // ${expr} 任意表达式
println("$name has ${n} features") // 混合文本与多段插值
```

插值表达式内部可嵌套字符串与括号（词法层平衡扫描）。

**属性访问与方法调用**

```pointerses
g.prefix                 // 字段 / 属性
obj.method(args)         // 方法
arr[0], m["a"]           // 索引
```

方法调用优先静态解析到 `impl` 定义的方法（见第 5.6 节）；否则回退到内置容器 / 字符串方法。

**结构体字面量**

```pointerses
Point { x: 1, y: 2 }
```

**数组 / 列表 / 映射字面量**

```pointerses
[1, 2, 3]                          // 推断为 List[int]
[]                                 // 需要类型注解
Map[String, int] { "a": 1, "b": 2 }
List[int] { 5, 7, 9 }              // 带类型的列表字面量
```

**闭包**

```pointerses
|x: int| -> int { return x * 2 }
|x, y| x + y                      // 单表达式体
|a: int, b: int| -> int { ... }
```

闭包捕获外层自由变量（由语义分析器填充 `captures`）。

**块表达式**

```pointerses
let r = { let t = 1; t + 2 }      // r 等于 3
```

**条件表达式 `if ... else`**

```pointerses
let m = if (x > 0) 1 else -1
```

`if` 作为表达式时返回分支值（分支为单一 primary 表达式）。

### 5.6 声明（Declarations）

**结构体 `struct`**

```pointerses
struct Greeter {
  prefix: String
  age: int
}
```

字段以 `名称: 类型` 列出，用逗号 / 换行 / 分号分隔。默认栈分配，支持属性访问。

**泛型结构体**（类型擦除：运行时只注册一份模板布局，字段按动态值存取）：

```pointerses
struct Box[T] {
  value: T
  tag: String
}

let ib = Box[int] { value: 42, tag: "int" }
let sb = Box[String] { value: "hi", tag: "str" }
println(ib.value + " / " + sb.value)
```

**函数 `fn`**

```pointerses
fn name(params) -> RetType { ... }
fn name(params) { ... }        // 无返回类型 => void
```

参数：`名称` 或 `名称: 类型`（可省略类型）。参数间用逗号分隔。

**泛型函数**（编译一次，调用时类型代换检查；可显式或推断类型实参）：

```pointerses
fn pick[T](first: T, second: T, prefer_first: bool) -> T {
  return prefer_first ? first : second
}

let a = pick[int](1, 2, true)        // 显式
let b = pick("x", "y", false)        // 推断 T = String
```

**特征 `trait` 与实现 `impl`**

```pointerses
trait Describe {
  fn describe() -> str        // 只声明签名
}

struct Dog { name: str, age: int }
struct Robot { model: str }

impl Describe for Dog {       // 实现 trait 方法
  fn describe() -> str { return this.name + " is " + this.age + " years old" }
}
impl Describe for Robot { ... }

impl Dog {                    // inherent impl（固有方法）
  fn bark() -> str { return "Woof, I am " + this.name }
  fn older() -> Dog { return Dog { name: this.name, age: this.age + 1 } }
}

let d = Dog { name: "Rex", age: 3 }
println(d.describe())         // 静态分派到 impl::Describe::Dog::describe
println(d.bark())
```

- `impl` 方法体内的 `this` 是**隐式接收者**（首参数），类型为接收者结构体。
- 方法调用在编译期**静态解析**为对应 `impl` 函数（`OP_CALL`），无需运行时查表。
- 泛型结构体也可有 `impl`（`impl Describe for Pair`），`this` 的字段按模板实例擦除。

**泛型约束（bound）与 trait 对象（dyn）**

```pointerses
fn describe_all[T: Describe](x: T) -> str { return x.describe() }   // T 有 bound
fn loud(d: dyn Describe) -> str { return d.describe() + "!!" }       // trait 对象
```

- 类型参数可声明 trait 约束 `T: Trait`：函数体内只能调用该 trait 声明的成员。
- `dyn Trait` 是 trait 对象类型，可传入任意 `impl Trait` 的结构体实例。
- 二者都通过**运行时动态分派**实现：编译器把调用编译成 `OP_METHOD_DYN`，VM 按接收者的
  类型标签在运行时 dispatch 表中查找对应的 impl 函数（找不到时报错）。
- 静态分派仍优先：具体类型已知的 `obj.method()` 在编译期解析，零运行时开销。

**模块 `module` 与导入 `import`**

```pointerses
// lib.psp
module Greet;
fn hello(name: str) -> str { return "Hello, " + name + "!" }
struct Point { x: int, y: int }

// main.psp
import "lib.psp"              // 递归合并（相对路径，去重）
fn main() -> int {
  let m = Greet::hello("world")
  let p = Greet::Point { x: 3, y: 4 }   // 或短名 Point { ... }
  println(m)
  return 0
}
```

- `module M;` 声明命名空间：本文件所有声明同时注册为 `M::name` 与短名 `name`。
- `import` 递归解析、合并被引文件；重复导入（菱形依赖）只合并一份。
- **短名作用域**：在模块文件内部使用短名 `hello()` / `Point { }` 会优先解析到**本模块**
  的同名成员（语义分析与代码生成都遵循）；跨模块同名互不干扰。只有**无模块**的文件
  （如主文件）直接使用短名时才取最后合并者——此时建议用全限定名 `A::hello`、`B::hello`。

**外部声明 `extern`（FFI）**

```pointerses
extern fn add(a: int, b: int) -> int     // 默认 Arrow ABI
extern "arrow" fn f(...)                 // 显式 Arrow（零拷贝共享内存）
extern "c" fn g(...) -> int              // C ABI
extern fn h(a: int)                       // 无返回
```

外部函数通过 **Arrow 共享内存描述符** 跨边界传参（默认零拷贝）。见第 12 节。

### 5.7 内置函数

**全局函数**

| 名称 | 签名 | 说明 |
|---|---|---|
| `println` | `println(x)` | 打印一行 |
| `print` | `print(x)` | 打印不换行 |
| `thread_id` | `thread_id() -> int` | 当前线程 / 调度器 ID |
| `sleep` | `sleep(ms: int)` | 休眠指定毫秒 |

**原生 GUI（窗口与画布）**

Windows 走 Win32，Linux 走 X11，纯 std + 系统 API，不依赖任何图形库。

| 名称 | 签名 | 说明 |
|---|---|---|
| `window` | `window(title: str, w: int, h: int) -> int` | 创建窗口，返回窗口 id |
| `window_close` | `window_close(id: int)` | 关闭窗口 |
| `window_title` | `window_title(id: int, title: str)` | 修改窗口标题 |
| `clear_canvas` | `clear_canvas(id: int)` | 清空画布 |
| `fill_rect` | `fill_rect(id, x, y, w, h, c)` | 画实心矩形 |
| `draw_rect` | `draw_rect(id, x, y, w, h, c)` | 画空心矩形 |
| `draw_text` | `draw_text(id, x, y, text: str)` | 在 `(x, y)` 画文本 |
| `rgb` | `rgb(r: int, g: int, b: int) -> int` | 构造 `0xRRGGBB` 颜色 |
| `on_key` | `on_key(id: int, func(k: str) -> int)` | 注册按键回调 |
| `on_click` | `on_click(id: int, func(x: int, y: int) -> int)` | 注册鼠标点击回调（客户端坐标） |
| `event_loop` | `event_loop()` | 进入事件循环，阻塞直到所有窗口关闭 |

`on_key` 的按键名：`space`、`enter`、`tab`、`backspace`、`escape`、`delete`、
`left` / `up` / `right` / `down`，以及字母与数字的原字符（已按键盘布局与 Shift 状态
解码）。方向键、`escape`、`delete` 没有字符码，从 `WM_KEYDOWN` 上报；其余字符从
`WM_CHAR` 上报（输入法组合出的字符走 `WM_IME_CHAR`，同样上报）。

回调与画布的两条语义要点：

- 画布的绘图指令被**录制**在 VM 顶层全局变量所在的窗口记录里，`clear_canvas` /
  `fill_rect` / `draw_rect` / `draw_text` 只改录制表并标记重绘，实际的 GDI 绘制发生在
  下一条 `WM_PAINT` 上（`BeginPaint` 取客户端区 DC，`EndPaint` 结束）。
- 回调在 **VM 主线程**上执行（事件循环线程内联调用，不是另开线程），因此可以直接读写
  顶层全局变量——回调里的 `say(...)` / `redraw()` 与原线程共享同一份 VM 状态。

**TUI（终端界面）**

纯 std 直接写 ANSI 转义序列，无需宿主 Rust 程序。

| 名称 | 签名 | 说明 |
|---|---|---|
| `clear_screen` | `clear_screen()` | 清屏并复位光标 |
| `cursor` | `cursor(x: int, y: int)` | 移动光标（1 基） |
| `color` | `color(fg: int, bg: int)` | 设置前景/背景色（ANSI 256 色或颜色名） |
| `reset_color` | `reset_color()` | 恢复默认颜色 |
| `hide_cursor` | `hide_cursor()` | 隐藏光标 |
| `show_cursor` | `show_cursor()` | 显示光标 |
| `key` | `key() -> str` | 阻塞读取一个按键（方向键给名字） |
| `key_available` | `key_available() -> bool` | 是否有按键等待 |
| `terminal_cols` | `terminal_cols() -> int` | 终端列数（检测不到时回退 80） |
| `terminal_rows` | `terminal_rows() -> int` | 终端行数（检测不到时回退 24） |

`key()` 返回：方向键 `up` / `down` / `left` / `right`，`enter`、`space`、`tab`、
`backspace`、`esc`，可打印字符返回原字符，其他字节返回 `key(N)`，输入流结束时返回
`eof`（注意：循环里用 `while true` 读 `key()` 时要在收到 `eof` 后自行退出，否则会
空转）。

**`List[T]` 方法**

| 方法 | 签名 | 说明 |
|---|---|---|
| `push` | `push(v: T)` | 尾部追加 |
| `pop` | `pop() -> T` | 弹出尾部 |
| `len` | `len() -> int` | 长度 |
| `get` | `get(i: int) -> T` | 取下标元素 |
| `set` | `set(i: int, v: T)` | 写下标元素 |

**`[T]` 数组方法**

| 方法 | 签名 | 说明 |
|---|---|---|
| `len` | `len() -> int` | 长度 |
| `get` | `get(i: int) -> T` | 取元素 |
| `set` | `set(i: int, v: T)` | 写元素 |

**`Map[K, V]` 方法**

| 方法 | 签名 | 说明 |
|---|---|---|
| `set` | `set(k: K, v: V)` | 写入键值 |
| `get` | `get(k: K) -> V` | 取值 |
| `has` | `has(k: K) -> bool` | 键是否存在 |
| `len` | `len() -> int` | 键数量 |
| `remove` | `remove(k: K) -> bool` | 删除键 |
| `keys` | `keys() -> List[K]` | 返回所有键 |

**`String` 方法**

| 方法 | 签名 | 说明 |
|---|---|---|
| `len` | `len() -> int` | 长度 |
| `trim` | `trim() -> String` | 去除首尾空白 |
| `to_upper` | `to_upper() -> String` | 转大写 |
| `to_lower` | `to_lower() -> String` | 转小写 |
| `substr` | `substr(a: int, b: int) -> String` | 取子串 |

### 5.8 注解（Annotations）

以 `@` 开头，可带命名或位置参数。合法放在**声明前**或**语句级**。

| 注解 | 目标 | 说明 |
|---|---|---|
| `@Auto` | 函数 | 在 M:N 协程调度器下调度执行 |
| `@Manual(fixed=N)` | 函数 | 在固定 N 个工作线程的线程池运行（默认 N=4） |
| `@region(X)` | 结构体 / 函数 | 指定区域，X ∈ `Stack` / `Heap` / `Scoped` |

```pointerses
@Auto
fn parallel_task(n: int) -> int { return n * n }

@Manual(fixed=2)
fn pool_task(i: int) -> int { return i + 10 }

@region(Heap)
struct HeapObj { data: int }
```

区域默认定级：`Named` 结构体与 `String` → Heap，其它 → Stack。由 `&` 创建的指针默认
属于所在作用域区域（Scoped）。

---

## 6. 内存模型与指针规则

- **默认栈分配**；`Heap` 绑定的对象进入堆区域。
- 指针分 `&`（不可变）与 `&mut`（可变）两种。
- 解引用用 `*p`；也支持隐式解引用（由语义分析按需处理）。
- 区域演算检查指针生命周期与作用域，防止悬垂指针。
- 闭包捕获来自闭包创建点之前的栈帧变量。
- 堆对象用**非阻塞引用计数**（`AtomicU32`），无外部 crate。

---

## 7. 并发

- `@Auto` 函数：M:N 协程调度（可跨线程 / 隔离 VM 实例）。
- `@Manual(fixed=N)` 函数：固定 N 线程池（默认 N=4）。
- 运行时辅助：`thread_id()`、`sleep(ms)`。
- 并发注解被编译进产物作为调度元数据，并在运行时**真实执行**（真实 OS 线程），
  可用 `thread_id()` 在 off-main 线程内打印布尔值证明。

---

## 8. 工程配置 `.pspc`

与 `.psp` 同 basename 的 `.pspc`（如 `main.psp` ↔ `main.pspc`）是该程序的**工程配置**，
工具链自动加载（`pss run`、`pssc` 打包都会读取）。格式为 **YAML 子集**：

```yaml
name: main
version: 0.1.0
dependencies:              # 可选：git 依赖（name: spec）
  utils: git+https://example.com/repo/utils.git#v1.0.0

no-api-check: true         # 可选：构建 .project 时跳过调用存在性检查

watchdog:                  # 可选：不配置此块则不启用 watchdog
  auto_restart: true       # 启用自动拉起
  max_auto_restarts: 3     # 最大重启次数
  auto_restarts_sleep_time: 20  # 重启间隔，单位：秒
```

- `name`：工程名（`pssl` 用它生成默认 `.psdl` 名）。
- `no-api-check`：`true` / `false`。等价于 `pssc --no-api-check`：构建 `.project` 时
  跳过"被调用函数是否存在"检查，允许调用由宿主 Rust 程序在运行时提供的 API
  （与 `--file-exe` / `--file-x` 组合会被拒绝，见第 3.2 与 12 节）。
- `dependencies`：git 依赖映射 `name: spec`，spec 格式
  `name: git+https://host/repo.git#ref` 或 `name: git+file:///path/to/repo.git#ref`；
  解析时自动 clone 到本地缓存并递归合并其模块 / 声明（见 `src/project/deps.rs`）。
- 参考实现：`examples/main.pspc`（git 依赖与 watchdog 均为注释示例，取消注释即启用）。

---

## 9. watchdog（进程级自动重启）

**watchdog 是宿主级、真实进程级处理**（非空壳）：`pss` 与 `pssc --file-exe` 打包的
原生可执行文件都把程序作为**子进程**运行；当子进程**异常终止**（`main` 返回非零、
未捕获异常、panic、abort、被信号杀死等任何非零退出）时，宿主自动重启它，最多
`max_auto_restarts` 次，每次间隔 `auto_restarts_sleep_time` 秒，直到成功（退出码 0）
或重试次数耗尽（以最后一次失败码退出）。

- 退出码为 0 视为成功，**不会**重启；配置了 `watchdog:` 但 `auto_restart: false`
  时只运行一次。
- 该策略对 `pss run <file.psp>`、`pss <file.project>`、`pssc --file-exe` 三种运行方式
  一致生效（watchdog 配置随 `.project` / 原生 exe 一并打包）。
- 宿主与子进程通过环境变量 `PSS_WATCHDOG_CHILD` 通信：子进程跳过自身 watchdog，
  避免无限递归；错误屏在 watchdog 子进程中也被跳过（防止弹窗阻塞重启循环）。

---

## 10. 错误屏（PointerSes Error Screen）

当一个 Pointerses 项目**报错**时，会自动弹出可视化的错误屏，把错误信息清楚地展示给
普通用户（日志输出不利于非技术用户报告错误）。

**触发时机**：`pss run <file.psp>` / `pss <file.project>` / `pss -c` 在**编译失败**或
**运行时错误**（VM `Err` 路径，如未定义变量、越界索引、未捕获异常）时弹出错误屏。
解释器 panic（如整数除零）不经过该路径。

**手动调用**（用户可自行触发，用于调试 / 测试 / 上报）：

```
pss task pointerses-error-screen -e "报错内容"
```

> 注意：错误内容要加**双引号**（`-e "..."`）；`-e` 之后的所有参数会用空格拼接，
> 所以即使不带引号的多词消息也能存活。

**显示形式**：

- Windows：动态加载 `user32!MessageBoxW` 弹出原生对话框（避免静态链接 user32）；
  失败时回退为 ANSI 终端屏。
- 其它平台：ANSI 终端错误屏（清屏 + 居中红框标题 + 自动换行的错误内容 + 使用提示）。

**禁用**：设置环境变量 `PSS_NO_ERROR_SCREEN=1` 可完全禁用弹窗（CI / 脚本场景）。
watchdog 受控的子进程也自动跳过弹窗。

**实现位置**：`runtime/errorscreen.rs`（纯 std，无外部依赖），以
`#[path]` 方式同时嵌入 lib（`src/lib.rs`）与原生驱动模板（`src/codegen/native.rs`），
因此 `pssc --file-exe` 打包出的原生可执行文件同样具备错误屏。

---

## 11. 编译管线

```
lexer ─► parser ─► semantic ─► codegen ─► bytecode ─► packaging / VM
```

| 阶段 | 模块 | 职责 |
|---|---|---|
| 词法 | `src/lexer.rs` | 源码 → token 流（含插值字符串的平衡扫描） |
| 语法 | `src/parser.rs` | token 流 → AST（`Program`：funcs / structs / traits / impls / externs / modules） |
| 工程 | `src/project/` | 读取 `.pspc`：name / version / dependencies / watchdog；git 依赖解析与合并 |
| 语义 | `src/semantic.rs` | 类型推断、作用域 / 模块解析、泛型约束、指针区域校验、闭包捕获收集 |
| 代码生成 | `src/codegen/bytecode.rs` + `native.rs` | 生成字节码；`native.rs` 产出内嵌 VM 的原生驱动 |
| 打包 | `src/packaging.rs` | 序列化 `.project` / `.psdl` 包（字节码 + 源码 + 元数据 + 架构 + watchdog） |
| 运行 | `src/vm/` | 字节码解释执行（含并发调度、处理器栈异常、动态分派表） |

`compiler.rs` 提供 `compile_file` / `compile_source` 两个入口：`pss -c` 走
`compile_source`（伪路径 `<command>`，`import` 相对当前目录解析），其余走
`compile_file`。

---

## 12. FFI（外部函数）

`extern fn` 声明外部函数，运行时通过 `PSS_RUST_LIB` 环境变量指向的 Rust `cdylib`
加载导出符号。默认 **Arrow ABI**：参数以零拷贝共享内存描述符（contiguous Arrow record
batch）传递，`extern "C" fn(desc: *const i64, len: i64) -> i64` 导出即被识别。

```pointerses
extern fn add(a: int, b: int) -> int

fn main() -> int {
  println(add(2, 3))    // 5
  return 0
}
```

配套示例：`examples/native_rust/`（Rust crate，导出 `rust_add` / `rust_mul`）。
构建并运行：

```powershell
# 在 examples/native_rust/ 内
cargo build --release

# 设置库路径后运行引用该库的程序
$env:PSS_RUST_LIB = "$PWD/examples/native_rust/target/release/pointerses_native.dll"
pss run <your_program.psp> --no-daemon
```

> 说明：`examples/main.psp` 中的 `extern fn add` 调用的是内置 `add`，无需外部库。
> 若要加载自己的 Rust 库，声明 `extern fn` 对应导出符号，并设置 `PSS_RUST_LIB` 即可。

### 12.1 宿主 API 与 `no-api-check`

上面是**显式声明** extern（构建期会核对调用存在）。另一种场景：Pointerses 程序作为
**API 库**被一个 Rust 宿主程序加载，宿主在运行时提供全部 API 符号。这时可以**不写
`extern fn` 声明**，而是用 `--no-api-check`（或 `.pspc` 的 `no-api-check: true`）跳过
构建期的存在性检查：任何既未定义、也未声明的函数调用都会被编译为 extern 调用，运行
时按名字在 `PSS_RUST_LIB` 指向的 `cdylib` 里解析（没有宿主库时运行期报
`unknown extern function ...`）。

```pointerses
// hostapi.psp —— 不声明 extern，直接调用宿主提供的 API
fn main() -> int {
  println(host_add(10, 32))   // 42
  println(host_mul(6, 7))     // 42
  return 0
}
```

```powershell
# 1) 构建 .project（跳过调用存在性检查）
pssc hostapi.psp --no-api-check        # 或 hostapi.pspc 里配 no-api-check: true

# 2) 宿主提供 API：cargo build --release 导出 host_add / host_mul 符号
$env:PSS_RUST_LIB = "$PWD/host/target/release/host.dll"
pss hostapi.project
```

> 该模式**只对 `.project` 打包有效**：`--no-api-check` 与 `--file-exe` / `--file-x`
> 一起使用会被拒绝。

---

## 13. 打包格式（`.project` / `.psdl`）

两种格式共享同一容器结构，仅 magic 不同：

| 常量 | 值 |
|---|---|
| `PROJECT_MAGIC` | `PSPROJ\0\0` |
| `PSDL_MAGIC` | `PSDL\0\0\0\0` |
| `VERSION` | 2 |

字段（小端序）：magic(8) → version(u32) → arch_mask(u32) → source_name(str) →
source(str) → bytecode(bytes) → meta_json(bytes) → watchdog(长度前缀；0 = 未配置，
否则 12 字节：auto_restart / max_auto_restarts / sleep_seconds 各 u32)。

**架构校验**：`pss` 执行包前用 `packaging::check_arch` 校验宿主架构是否在
`arch_mask` 内（x64=1 / x32=2 / arm64=4），不匹配则拒绝运行。`pssc --file-exe`
的多架构构建产出独立的原生二进制（每个架构一个），`--fram-all` 一次打出三个。

---

## 14. 守护进程与快照

`pss run`（不带 `--no-daemon`）先尝试常驻守护进程的内存快照**快速启动**：

- 守护进程默认端口 `daemon::DEFAULT_PORT`，可用 `pss daemon --port N` 手动启动。
- `daemon::try_fast_start(file)` 命中快照 → 直接执行字节码，省去冷启动全量编译。
- 未命中 → 冷编译后 `daemon::warm_cache` 预热快照，下次命中。
- `pss -c` 不走守护进程（没有文件可快照），适合一次性命令。

---

## 15. 跨架构构建、musl 静态与发布

**`--fram-*` 架构模型**（`src/arch.rs`）

| 标志 | 架构 | 位 |
|---|---|---|
| `--fram-x64` | x86_64 | 1 |
| `--fram-x32` | i686 / x86 | 2 |
| `--fram-arm64` | aarch64 | 4 |
| `--fram-all` | 全部 | 7 |

可组合（非互斥）；未指定时默认**宿主机架构**。架构以位掩码记录进包，运行时强制校验。

**交叉编译**：`pssc --file-exe --fram-*` 跨架构时用 `rustc --target` + `tools/zigcc.rs`
（`zig cc` shim）链接。zig 由 shim 自己找（同目录 `zig` / 环境变量 `ZIG` / PATH），无需
写进任何配置文件。目标三元组到 zig `-target` 的映射在 `.cargo/config.toml`，而 mingw
导入库的 `-L` 搜索路径由 `build.rs` 按 `rustc --print sysroot` 动态解析——因为 cargo 不
会展开 `rustflags` 里的 `${env:VAR}`，所以这个路径不可能在 config 里写成可移植形式。

**Linux 静态链接**：Linux x64 / arm64 发布包是 **musl 静态链接**的（zig 交叉链接 +
glibc 兼容 shim `tools/musl_shim_{aarch64,x86_64}.o`），不依赖动态链接器，因此也能在
**Android Termux（bionic）/ 精简容器**等没有 glibc 的系统上直接运行。i686-linux 保持
动态 glibc。`gui.rs` 的 `#[link(name="X11")]` 让每个 Linux 链接都带 `-lX11`，而 zig
不带 libX11；`tools/x11_stub/` 提供链接桩解决之（zig 0.16 的 `-l` 解析不认 `-L`，所以
zigcc 直接按链接类型注入桩）：静态 bin 注入 `libX11.o`（no-op 桩体烤进二进制，GUI 调用
优雅失败——`XOpenDisplay` 返回 0 → "cannot open X display"——CLI 全功能），动态 bin 与
cdylib 用 `libX11.so`（`SONAME libX11.so.6`，运行时绑真 libX11，GUI 在有 X11 的 Linux 上
正常）。即静态包是便携 CLI（无 GUI），要 GUI 用动态/cdylib 包。

**Termux 使用要点**：解压后 `chmod +x pss` 然后 `./pss`（**不要** `bash pss`）；
注意 `chmod -X`（大写）不会设置执行位。

**LLVM 后端**（可选，`-b llvm` / `--features llvm`）：内嵌完整 LLVM（`libs/LLVM-C.dll`
约 67 MB，仓库内自带；`build.rs` 在构建时把它复制到 `pss.exe` 旁边，运行时无需外部
LLVM / clang / MSVC）。DLL 缺失时按 `PSS_LLVM_DIR/<bin|lib>`、`PATH` 查找——源码里
没有任何硬编码的本机安装路径。`llvm` 后端**仅宿主**，不能跨架构（无 cross LLVM-C）；
跨架构请用默认 `native` 后端。`--emit-ir` 仅 `llvm` 后端可用（导出 `.ll` 文本）。

**发布打包**：`scripts/build-all.ps1` 构建 6 个目标（4 个工具 + cdylib），调用 zigcc
交叉链接，再由 `scripts/make-releases.ps1` 组装发布目录
`target\releases\PointerSes-0.1-beta-<os>_<arch>-{all,bin,dev}`；`LLVM-C.dll` 只放进
Windows 包。demo 打包使用 `examples\main.psp`（host 原生 + 有 zig 时多架构）。

---

## 16. 仓库布局

```
Pointerses/
├── Cargo.toml              # 零外部依赖；bin: pss/pssc/pssp/pssl; lib: rlib+cdylib
├── build.rs                # 捆绑 LLVM-C；按 target 动态输出 mingw 导入库的 -L 路径
├── .cargo/config.toml      # 6 个交叉目标的 linker + zig -target 映射（无本机路径）
├── README.md               # 简介（本文档为详细开发文档）
├── docs.md                 # 本文件
├── src/
│   ├── lib.rs              # 工具链共享库（模块汇总）
│   ├── bin/pss.rs          # 运行器
│   ├── bin/pssc.rs         # 打包器 / 编译器驱动
│   ├── bin/pssp.rs         # 反编译器
│   ├── bin/pssl.rs         # 动态库打包器
│   ├── cli.rs              # pss 子命令（run/daemon/task/-c/version/help）
│   ├── lexer.rs parser.rs semantic.rs compiler.rs
│   ├── codegen/            # bytecode.rs（字节码）、native.rs（原生驱动）、llvm 相关
│   ├── vm/                 # 解释器 / 并发调度 / 处理器栈
│   ├── packaging.rs        # .project / .psdl 序列化
│   ├── project/            # .pspc：yaml / deps / mangle
│   ├── watchdog.rs         # 宿主级自动重启
│   ├── daemon.rs           # 常驻守护进程与快照
│   ├── arch.rs             # 目标架构模型与 --fram-* 解析
│   ├── ffi.rs              # Arrow ABI 外部函数
│   ├── bootstrap.rs        # 自举元数据（meta → JSON）
│   └── concurrency.rs      # M:N 协程 / 线程池调度
├── runtime/
│   ├── runtime.rs          # VM 运行时（#[path] 嵌入 lib 与原生驱动）
│   ├── gui.rs              # 原生 GUI：窗口/画布，内含 Win32 与 X11 两个后端
│   └── errorscreen.rs      # 错误屏（纯 std）
├── libs/
│   └── LLVM-C.dll          # vendored LLVM-C（~67 MB）；build.rs 的唯一必需要求，
│                           #   构建时复制到 target/<profile>/，即 pss.exe 旁边
├── tools/
│   ├── zigcc.rs            # zig cc 交叉链接 shim
│   ├── musl_shim.c/.o      # glibc 兼容符号 shim（Linux 静态链接，-g0 -fno-ident 生成）
│   ├── dwarf_stub.s/.o     # i686 Windows 展开符号桩（用汇编而非 C：-g0 清不掉 .debug$S，
│   │                       #   而那里会写进构建机的 zig 临时路径）
│   ├── x11_stub.c          # libX11 链接桩源（21 个 no-op 函数；-g0 -fno-ident 生成 .so/.o）
│   └── x11_stub/           # 各 arch 的 libX11.so/.o 桩；zigcc 按链接类型注入
│       ├── x86_64-linux/   #   静态 bin 注入 .o（GUI 优雅失败）/ 动态+cdylib 用 .so
│       ├── aarch64-linux/  #   （运行时绑真 libX11.so.6）
│       └── i686-linux/     #   仅 .so（动态 glibc 链接）
├── scripts/
│   ├── build-all.ps1       # 全目标构建 + 示例打包 + 发布组装
│   └── make-releases.ps1   # 发布目录打包
└── examples/
    ├── main.psp            # 综合示例（单文件，直接可编译运行）
    ├── main.pspc           # 工程配置（name/version + 依赖/watchdog 注释示例）
    ├── cli.psp             # 命令行工具示例
    ├── tui.psp             # TUI 终端界面示例（ANSI + key()）
    ├── gui.psp             # GUI 示例（点击移动方块、空格变色）
    ├── gui_smoke.psp       # 窗口生命周期自检
    ├── cb_test.psp         # 按键/点击回调 + 顶层全局变量共享
    └── native_rust/        # FFI 配套 Rust cdylib（rust_add / rust_mul）
```

`runtime/` 下的三个文件由 `#[path]` 嵌入到两处，改它们时要同时考虑这两条路径：

- `src/vm/mod.rs` 与 `src/lib.rs` 把 `runtime.rs` / `errorscreen.rs` 以 `#[path]`
  挂进 lib，供字节码 VM 使用；`runtime.rs` 内部再用 `#[path = "gui.rs"]` 挂 GUI
  子模块（相对**编译所在目录** `runtime/` 解析，与 lib 的挂载点无关）。
- `src/codegen/native.rs` 用 `include_str!` 把 `runtime.rs` 与 `errorscreen.rs` 原样
  内联进 `pssc` 生成的驱动源码，包进 `mod runtime { ... }`。此时 `runtime.rs` 里的
  `#[path = "gui.rs"]` 会改指向驱动目录而不存在，所以 `runtime_src()` 在生成驱动时
  把标记替换成 `mod gui { <gui.rs 全文> }` 的内联形式——`pssc --file-exe` 与 LLVM
  驱动都走这条路径。