# dwarf_stub.s - no-op stubs for the mingw frame-registration entry points.
#
# i686-pc-windows-gnu unwinds with DWARF, so rust's libstd references
# ___register_frame_info / ___deregister_frame_info. zig/llvm use SEH for
# that target and never export them, so the linker needs something to bind to.
#
# This is assembly rather than C on purpose. `zig cc` compiling C emits
# CodeView debug info that records zig's own temporary output path
# (-MT <AppData>\zig\tmp\*.obj) into the `.debug$S` section, and that string
# survives -g0: every compile would bake a machine-specific path into the
# committed object. Assembling directly produces an object with no debug
# sections at all, so the file carries no path whatsoever.
#
# AT&T syntax: `#` starts a comment, `;` does not.
#
# Built with:
#   zig cc -c -target x86-windows-gnu dwarf_stub.s -o dwarf_stub.o
# and injected by tools/zigcc.rs when linking an i686-pc-windows-gnu target.
	.text
	.globl ___register_frame_info
___register_frame_info:
	ret
	.globl ___deregister_frame_info
___deregister_frame_info:
	ret
