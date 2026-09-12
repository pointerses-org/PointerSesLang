// x11_stub.c - link-time-only stub for libX11.
//
// gui.rs declares `#[link(name = "X11")]`, so every Linux target link carries
// `-lX11`. The linker has to actually FIND a libX11.so in order to record
// `libX11.so.6` as a DT_NEEDED entry - even though the real library is only
// needed at run time. Cross-building from a machine without libX11 dies with:
//
//   error: unable to find dynamic system library 'X11' using strategy
//          'no_fallback'. searched paths:
//
// This file supplies the 21 symbols the link needs. Two build products, used
// per link kind (see tools/zigcc.rs):
//
//   - libX11.so (shared, SONAME libX11.so.6): used by dynamic executables
//     (i686-linux) and left in place for cdylib links. At run time the loader
//     follows the SONAME and binds every symbol to the real system
//     libX11.so.6, so these stub bodies never execute.
//   - libX11.o (static): used by the 64-bit musl static executables, which
//     cannot carry undefined symbols. The 21 no-op bodies are baked in; GUI
//     calls fail gracefully (XOpenDisplay returns 0 -> "cannot open X
//     display") rather than crash. This is the trade-off of the portable
//     static build: CLI everywhere, GUI only where a dynamic build can bind X11.
//
// Signatures are deliberately `void f(void)` - for a symbol that only exists
// to be resolved at link time the prototype is irrelevant to both the linker
// and the loader, and it keeps this file to 21 lines.
//
// tools/zigcc.rs adds the arch-matched subdirectory of tools/x11_stub/ as a
// `-L` search path for Linux targets, drops `-lX11` and injects libX11.o for
// static executables, and replaces `-lX11` with the libX11.so path for dynamic
// executables. (zig 0.16 resolves `-l<name>` through its own system-library
// search and ignores `-L`, so neither -L alone nor `-l:libX11.so` works; the
// direct path input does.)
//
// Built per-arch (zig calls 32-bit x86 `x86`, not `i686`; zigcc retargets the
// 64-bit Linux targets to musl while i686 keeps the dynamic glibc link):
//
//   # shared stubs (dynamic executables + cdylib):
//   zig cc -g0 -O2 -fno-ident -shared -fPIC -target x86_64-linux-musl \
//       -Wl,-soname,libX11.so.6 x11_stub.c -o x11_stub/x86_64-linux/libX11.so
//   zig cc -g0 -O2 -fno-ident -shared -fPIC -target aarch64-linux-musl \
//       -Wl,-soname,libX11.so.6 x11_stub.c -o x11_stub/aarch64-linux/libX11.so
//   zig cc -g0 -O2 -fno-ident -shared -fPIC -target x86-linux-gnu \
//       -Wl,-soname,libX11.so.6 x11_stub.c -o x11_stub/i686-linux/libX11.so
//
//   # static stubs (64-bit musl static executables):
//   zig cc -g0 -O2 -fno-ident -c -target x86_64-linux-musl \
//       x11_stub.c -o x11_stub/x86_64-linux/libX11.o
//   zig cc -g0 -O2 -fno-ident -c -target aarch64-linux-musl \
//       x11_stub.c -o x11_stub/aarch64-linux/libX11.o
//
// `-g0` and `-fno-ident` keep the committed binaries free of build-machine
// fingerprints: the debug sections record the build machine's zig install path
// and the full compiler invocation, and the comment section records the clang
// version.

void XOpenDisplay(void) { }
void XDefaultRootWindow(void) { }
void XCreateSimpleWindow(void) { }
void XStoreName(void) { }
void XMapWindow(void) { }
void XFlush(void) { }
void XNextEvent(void) { }
void XCreateGC(void) { }
void XFreeGC(void) { }
void XSetForeground(void) { }
void XFillRectangle(void) { }
void XDrawRectangle(void) { }
void XDrawString(void) { }
void XFilterEvent(void) { }
void XDestroyWindow(void) { }
void XSelectInput(void) { }
void XInternAtom(void) { }
void XSetWMProtocols(void) { }
void XLookupString(void) { }
void XSetInputFocus(void) { }
void XClearArea(void) { }
