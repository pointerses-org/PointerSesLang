// musl_shim.c — glibc compatibility shim for statically linking rustc's GNU
// std objects against zig's musl libc.
//
// rustc's `*-unknown-linux-gnu` std references a small set of glibc-only
// symbols that musl does not provide:
//
//   * `open64` / `fstat64` / `stat64` / `mmap64` / `lseek64` — glibc's LFS
//     (`_LARGEFILE64_SOURCE`) aliases. On 64-bit targets musl's native
//     `open` / `fstat` / `stat` / `mmap` / `lseek` already use 64-bit
//     off_t/ino_t, so these aliases map 1:1.
//   * `gnu_get_libc_version` — glibc reports its version string; returning a
//     low version makes rust's runtime take the conservative fallback paths.
//   * `__res_init` — glibc's resolver initialization hook; a no-op satisfies it.
//
// Built per-arch (aarch64 / x86_64) with:
//   zig cc -g0 -O2 -fno-ident -target aarch64-linux-musl -c musl_shim.c -o musl_shim_aarch64.o
//   zig cc -g0 -O2 -fno-ident -target x86_64-linux-musl  -c musl_shim.c -o musl_shim_x86_64.o
// Both flags are required so the committed object carries no build-machine
// fingerprint:
//   `-g0`        drop the debug sections, which record the build machine's
//                zig install path and the full compiler invocation.
//   `-fno-ident` drop the `.comment`-style section holding `clang version N`.
// (The i686-windows stub lives in dwarf_stub.s instead of a .c file because
// `-g0` cannot suppress the CodeView `.debug$S` section there.)
//
// Injected by tools/zigcc.rs when linking a 64-bit Linux target statically.

#define _GNU_SOURCE
#include <sys/types.h>
#include <sys/stat.h>
#include <sys/mman.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stddef.h>
#include <dirent.h>

int open64(const char *path, int flags, ...) {
    mode_t mode = 0;
    va_list ap;
    va_start(ap, flags);
    mode = va_arg(ap, mode_t);
    va_end(ap);
    return open(path, flags, mode);
}

int fstat64(int fd, struct stat *buf) {
    return fstat(fd, buf);
}

int stat64(const char *path, struct stat *buf) {
    return stat(path, buf);
}

void *mmap64(void *addr, size_t length, int prot, int flags, int fd, off_t offset) {
    return mmap(addr, length, prot, flags, fd, offset);
}

off_t lseek64(int fd, off_t offset, int whence) {
    return lseek(fd, offset, whence);
}

// glibc `readdir64` / `struct dirent64`. On 64-bit targets musl's `struct
// dirent` has the identical layout (64-bit d_ino / d_off), so cast directly.
struct dirent64 {
    unsigned long long d_ino;
    long long d_off;
    unsigned short d_reclen;
    unsigned char d_type;
    char d_name[256];
};

struct dirent64 *readdir64(DIR *dirp) {
    return (struct dirent64 *)readdir(dirp);
}

const char *gnu_get_libc_version(void) {
    return "2.17";
}

void __res_init(void) {
}
