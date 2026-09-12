//! A Rust native library callable from Pointerses through the Arrow
//! shared-memory descriptor ABI.
//!
//! The Pointerses runtime loads this `cdylib` (via `PSS_RUST_LIB`) and calls the
//! exported symbol with a descriptor pointing to a contiguous Arrow record batch:
//!
//!   `extern "C" fn(desc: *const i64, len: i64) -> i64`
//!
//! Arguments are zero-copy: the caller's Arrow buffer is passed directly to the
//! Rust function (no marshalling/copying at the boundary).

#![allow(non_camel_case_types)]

/// Add the first two elements of the Arrow record batch. Exported with C ABI
/// and a stable symbol so `GetProcAddress` can resolve it.
#[no_mangle]
pub extern "C" fn rust_add(desc: *const i64, len: i64) -> i64 {
    if desc.is_null() || len < 2 {
        return 0;
    }
    // SAFETY: the caller guarantees `desc` points to `len` contiguous i64s.
    let slice = unsafe { std::slice::from_raw_parts(desc, len as usize) };
    slice[0] + slice[1]
}

/// Multiply the first two elements of the Arrow record batch.
#[no_mangle]
pub extern "C" fn rust_mul(desc: *const i64, len: i64) -> i64 {
    if desc.is_null() || len < 2 {
        return 0;
    }
    let slice = unsafe { std::slice::from_raw_parts(desc, len as usize) };
    slice[0] * slice[1]
}
