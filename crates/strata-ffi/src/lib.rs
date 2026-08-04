//! strata-ffi — Strata vstore 的 C ABI 绑定。
//!
//! 句柄为 `*mut c_void`（内部指向 [`SyncStore`]，RwLock 串行化，线程安全）。
//! 所有函数体以 `catch_unwind` 包裹，panic 不会穿越 FFI 边界。
//!
//! 错误码（见 `include/strata_ffi.h`）：
//! `0` 成功 · `1` 失败（详情见 `strata_last_error`）· `2` Rust 侧 panic 被捕获 ·
//! `3` 仅 `strata_read`：键不存在。
//!
//! 最近错误存放在 thread-local：每线程一份，互不覆盖。
//
// C ABI 函数必须可从 C 侧安全调用（错误经返回值/strata_last_error 报告），
// 不能声明为 `unsafe fn`；所有指针参数在解引用前逐一判空校验。
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::any::Any;
use std::cell::RefCell;
use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::ptr;

use strata_core::gc::GcConfig;
use strata_core::store::StoreConfig;
use strata_core::sync_store::SyncStore;
use strata_core::tier::TierConfig;

/// 成功。
pub(crate) const OK: i32 = 0;
/// 失败（详情见 strata_last_error）。
pub(crate) const ERR: i32 = 1;
/// Rust 侧 panic，已在边界捕获。
const PANIC: i32 = 2;
/// strata_read 专用：键无记录。
const MISSING: i32 = 3;

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

pub(crate) fn set_last_error(msg: impl AsRef<str>) {
    LAST_ERROR.with(|e| {
        let mut e = e.borrow_mut();
        e.clear();
        e.push_str(msg.as_ref());
    });
}

pub(crate) fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return s.to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "panic".to_string()
}

/// 运行闭包，把 `Result`/panic 归一到 C 错误码。
pub(crate) fn guarded_i32(f: impl FnOnce() -> Result<i32, String>) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(code)) => code,
        Ok(Err(msg)) => {
            set_last_error(msg);
            ERR
        }
        Err(payload) => {
            set_last_error(format!("panic: {}", panic_message(payload)));
            PANIC
        }
    }
}

/// 同 [`guarded_i32`]，但返回句柄指针（失败 → NULL）。
fn guarded_open(f: impl FnOnce() -> Result<*mut c_void, String>) -> *mut c_void {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(h)) => h,
        Ok(Err(msg)) => {
            set_last_error(msg);
            ptr::null_mut()
        }
        Err(payload) => {
            set_last_error(format!("panic: {}", panic_message(payload)));
            ptr::null_mut()
        }
    }
}

/// 把 `str` 复制到 C 缓冲（NUL 结尾，不足则截断）。
/// 返回复制的字节数（不含终止符）；buf 为 NULL 或 len 为 0 时返回 -1。
///
/// # Safety
///
/// `buf` 非 NULL 时必须指向至少 `len` 个可写字节。
fn copy_to_buffer(text: &str, buf: *mut u8, len: usize) -> i32 {
    if buf.is_null() || len == 0 {
        return -1;
    }
    let n = text.len().min(len - 1);
    // SAFETY: 调用方保证 buf 指向至少 len 个可写字节（见上）。
    unsafe {
        ptr::copy_nonoverlapping(text.as_ptr(), buf, n);
        *buf.add(n) = 0;
    }
    n as i32
}

/// 还原句柄。
///
/// # Safety
///
/// `h` 必须是 [`strata_open`] 返回且未被 [`strata_close`] 释放的句柄。
unsafe fn handle(h: *mut c_void) -> Result<&'static SyncStore, String> {
    if h.is_null() {
        return Err("null pointer".to_string());
    }
    // SAFETY: 句柄生命周期纪律由调用方遵守（C 头文件已声明）。
    Ok(&*(h as *const SyncStore))
}

/// 构造 [`StoreConfig`]（布尔位按 C 约定：非 0 = 启用；
/// `compression_threads` 负值按 0 = 自动处理）。
fn config_from_parts(
    hot_level: i32,
    hot_enabled: i32,
    cold_level: i32,
    cold_enabled: i32,
    dictionary: i32,
    cache_mb: u64,
    segment_max_bytes: u64,
    compression_threads: i32,
) -> StoreConfig {
    StoreConfig {
        hot_level,
        hot_enabled: hot_enabled != 0,
        cold_level,
        cold_enabled: cold_enabled != 0,
        dictionary: dictionary != 0,
        cache_mb,
        segment_max_bytes,
        compression_threads: compression_threads.max(0) as u32,
    }
}

/// 打开（或创建）`root` 处的 vstore，返回堆上句柄。C ABI 与 JNI 共用。
// 两套 ABI 共用同一组参数，不做拆分。
#[allow(clippy::too_many_arguments)]
pub(crate) fn core_open(
    root: &str,
    hot_level: i32,
    hot_enabled: i32,
    cold_level: i32,
    cold_enabled: i32,
    dictionary: i32,
    cache_mb: u64,
    segment_max_bytes: u64,
    compression_threads: i32,
) -> Result<*mut c_void, String> {
    let cfg = config_from_parts(
        hot_level,
        hot_enabled,
        cold_level,
        cold_enabled,
        dictionary,
        cache_mb,
        segment_max_bytes,
        compression_threads,
    );
    let store = SyncStore::open(Path::new(root), cfg).map_err(|e| e.to_string())?;
    Ok(Box::into_raw(Box::new(store)) as *mut c_void)
}

/// 写入一条记录（payload 为不透明字节，内部压缩）。C ABI 与 JNI 共用。
pub(crate) fn core_write(
    h: *mut c_void,
    x: i32,
    z: i32,
    type_id: u16,
    nbt: &[u8],
) -> Result<(), String> {
    // SAFETY: 句柄有效性契约见 `handle`。
    let store = unsafe { handle(h)? };
    store.write(x, z, type_id, nbt).map_err(|e| e.to_string())
}

/// 读取最新版本；键不存在 → `None`。C ABI 与 JNI 共用。
pub(crate) fn core_read(
    h: *mut c_void,
    x: i32,
    z: i32,
    type_id: u16,
) -> Result<Option<Vec<u8>>, String> {
    // SAFETY: 句柄有效性契约见 `handle`。
    let store = unsafe { handle(h)? };
    store.read(x, z, type_id).map_err(|e| e.to_string())
}

/// 落盘：fsync 活跃段 → 合并增量索引 → 推进 epoch → 保存 manifest。
pub(crate) fn core_flush(h: *mut c_void) -> Result<(), String> {
    // SAFETY: 句柄有效性契约见 `handle`。
    let store = unsafe { handle(h)? };
    store.flush().map_err(|e| e.to_string())
}

/// 一轮 GC：整段删除 → hole-punch → 打分压实。
pub(crate) fn core_gc(
    h: *mut c_void,
    invalid_threshold: f64,
    budget_bytes: u64,
    min_hole_bytes: u64,
) -> Result<(), String> {
    // SAFETY: 句柄有效性契约见 `handle`。
    let store = unsafe { handle(h)? };
    let cfg = GcConfig {
        invalid_threshold,
        budget_bytes,
        min_hole_bytes,
    };
    store.gc_pass(&cfg).map(|_| ()).map_err(|e| e.to_string())
}

/// 一轮分层迁移：热 → 冷晋升 / 解包降级。
pub(crate) fn core_tier(
    h: *mut c_void,
    enabled: i32,
    stable_flushes: u32,
    invalid_demote_ratio: f64,
) -> Result<(), String> {
    // SAFETY: 句柄有效性契约见 `handle`。
    let store = unsafe { handle(h)? };
    let cfg = TierConfig {
        enabled: enabled != 0,
        stable_flushes,
        invalid_demote_ratio,
    };
    store.tier_pass(&cfg).map(|_| ()).map_err(|e| e.to_string())
}

/// 关闭并释放 store；NULL 为无操作。之后句柄不得再使用。
pub(crate) fn core_close(h: *mut c_void) {
    if h.is_null() {
        return;
    }
    // SAFETY: h 由 core_open 经 Box::into_raw 创建，且仅此一处回收。
    unsafe {
        drop(Box::from_raw(h as *mut SyncStore));
    }
}

/// 版本字符串（C ABI 与 JNI 共用）。
pub(crate) fn core_version() -> &'static str {
    concat!("strata-ffi ", env!("CARGO_PKG_VERSION"))
}

/// 读取本线程最近一次错误信息的副本。
pub(crate) fn last_error_message() -> String {
    LAST_ERROR.with(|e| e.borrow().clone())
}

/// 打开（或创建）`root` 处的 vstore；失败返回 NULL（详情 strata_last_error）。
/// 布尔开关以 i32 传入：非 0 = 启用。`compression_threads`：批量写入的压缩
/// 线程数，`0` = 自动（全部可用核心），`1` = 串行（默认），`N ≥ 2` = 限 N 线程。
///
/// # Safety
///
/// `root` 必须是 NUL 结尾、读之有效的 UTF-8 C 字符串。
#[no_mangle]
// C ABI 签名由外部约定固定为 9 参数，不做 Rust 侧拆分。
#[allow(clippy::too_many_arguments)]
pub extern "C" fn strata_open(
    root: *const c_char,
    hot_level: i32,
    hot_enabled: i32,
    cold_level: i32,
    cold_enabled: i32,
    dictionary: i32,
    cache_mb: u64,
    segment_max_bytes: u64,
    compression_threads: i32,
) -> *mut c_void {
    guarded_open(move || {
        if root.is_null() {
            return Err("null pointer".to_string());
        }
        // SAFETY: 调用方保证 root 为 NUL 结尾的 C 字符串（见上）。
        let path = unsafe { CStr::from_ptr(root) };
        let path = path
            .to_str()
            .map_err(|_| "root is not valid UTF-8".to_string())?;
        core_open(
            path,
            hot_level,
            hot_enabled,
            cold_level,
            cold_enabled,
            dictionary,
            cache_mb,
            segment_max_bytes,
            compression_threads,
        )
    })
}

/// 写入一条记录（内部压缩）。`nbt` 为 NULL 且 `len == 0` 时视为空负载。
///
/// # Safety
///
/// `h` 必须是有效句柄；`nbt` 非 NULL 时必须指向至少 `len` 个已初始化字节。
#[no_mangle]
pub extern "C" fn strata_write(
    h: *mut c_void,
    x: i32,
    z: i32,
    type_id: u16,
    nbt: *const u8,
    len: usize,
) -> i32 {
    guarded_i32(move || {
        let data: &[u8] = if nbt.is_null() {
            if len != 0 {
                return Err("null pointer".to_string());
            }
            &[]
        } else {
            // SAFETY: 调用方保证 nbt 指向至少 len 个已初始化字节（见上）。
            unsafe { std::slice::from_raw_parts(nbt, len) }
        };
        core_write(h, x, z, type_id, data)?;
        Ok(OK)
    })
}

/// 读取最新版本。成功返回 0，负载经 `*out_ptr`/`*out_len` 返回，须用
/// [`strata_read_free`] 释放；键不存在返回 3。
///
/// # Safety
///
/// `h` 必须是有效句柄；`out_ptr`/`out_len` 必须可写。
#[no_mangle]
pub extern "C" fn strata_read(
    h: *mut c_void,
    x: i32,
    z: i32,
    type_id: u16,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    guarded_i32(move || {
        if out_ptr.is_null() || out_len.is_null() {
            return Err("null pointer".to_string());
        }
        match core_read(h, x, z, type_id)? {
            Some(data) => {
                // 经 Box<[u8]> 转交所有权（等价于 into_raw_parts，但稳定 API）：
                // into_boxed_slice 已把容量压实到 len，free 侧按 (ptr, len) 还原。
                let boxed: Box<[u8]> = data.into_boxed_slice();
                let len = boxed.len();
                let ptr = Box::into_raw(boxed).cast::<u8>();
                // SAFETY: 调用方保证 out_ptr/out_len 可写（上方已判空）。
                unsafe {
                    *out_ptr = ptr;
                    *out_len = len;
                }
                Ok(OK)
            }
            None => {
                // SAFETY: 同上。
                unsafe {
                    *out_ptr = ptr::null_mut();
                    *out_len = 0;
                }
                Ok(MISSING)
            }
        }
    })
}

/// 释放 [`strata_read`] 分配的缓冲区。NULL 为无操作。
///
/// # Safety
///
/// `buf`/`len` 必须原样来自 [`strata_read`] 的输出；释放后不得再使用。
#[no_mangle]
pub extern "C" fn strata_read_free(buf: *mut u8, len: usize) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if buf.is_null() {
            return;
        }
        // SAFETY: buf/len 来自 strata_read 的 Box::into_raw（容量 == len）。
        unsafe {
            let slice = std::slice::from_raw_parts_mut(buf, len);
            drop(Box::from_raw(slice));
        }
    }));
}

/// 落盘：fsync 活跃段 → 合并增量索引 → 推进 epoch → 保存 manifest。
///
/// # Safety
///
/// `h` 必须是有效句柄。
#[no_mangle]
pub extern "C" fn strata_flush(h: *mut c_void) -> i32 {
    guarded_i32(move || {
        core_flush(h)?;
        Ok(OK)
    })
}

/// 一轮 GC：整段删除 → hole-punch → 打分压实。
///
/// # Safety
///
/// `h` 必须是有效句柄。
#[no_mangle]
pub extern "C" fn strata_gc(
    h: *mut c_void,
    invalid_threshold: f64,
    budget_bytes: u64,
    min_hole_bytes: u64,
) -> i32 {
    guarded_i32(move || {
        core_gc(h, invalid_threshold, budget_bytes, min_hole_bytes)?;
        Ok(OK)
    })
}

/// 一轮分层迁移：热 → 冷晋升 / 解包降级。
///
/// # Safety
///
/// `h` 必须是有效句柄。
#[no_mangle]
pub extern "C" fn strata_tier(
    h: *mut c_void,
    enabled: i32,
    stable_flushes: u32,
    invalid_demote_ratio: f64,
) -> i32 {
    guarded_i32(move || {
        core_tier(h, enabled, stable_flushes, invalid_demote_ratio)?;
        Ok(OK)
    })
}

/// 关闭并释放 store。NULL 为无操作；之后句柄不得再使用。
///
/// # Safety
///
/// `h` 必须是 [`strata_open`] 返回的句柄或 NULL，且未被释放过。
#[no_mangle]
pub extern "C" fn strata_close(h: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| core_close(h)));
}

/// 复制本线程最近一次错误信息到 `buf`（NUL 结尾，不足截断）。
/// 返回复制字节数（不含终止符）；buf 为 NULL 或 len 为 0 时返回 -1。
///
/// # Safety
///
/// `buf` 非 NULL 时必须指向至少 `len` 个可写字节。
#[no_mangle]
pub extern "C" fn strata_last_error(buf: *mut u8, len: usize) -> i32 {
    let msg = match catch_unwind(AssertUnwindSafe(last_error_message)) {
        Ok(m) => m,
        Err(_) => return -1,
    };
    // SAFETY: buf/len 契约同 `copy_to_buffer`。
    copy_to_buffer(&msg, buf, len)
}

/// 写入 `"strata-ffi <crate version>"`（截断规则同 [`strata_last_error`]）。
///
/// # Safety
///
/// `buf` 非 NULL 时必须指向至少 `len` 个可写字节。
#[no_mangle]
pub extern "C" fn strata_version(buf: *mut u8, len: usize) -> i32 {
    // SAFETY: buf/len 契约同 `copy_to_buffer`。
    copy_to_buffer(core_version(), buf, len)
}

#[cfg(feature = "jni")]
mod jni;

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn open_store(dir: &Path) -> *mut c_void {
        let root = CString::new(dir.to_str().unwrap()).unwrap();
        let h = strata_open(
            root.as_ptr(),
            3, // hot_level
            1, // hot_enabled
            9, // cold_level
            1, // cold_enabled
            1, // dictionary
            512,
            64 * 1024 * 1024,
            1, // compression_threads
        );
        assert!(!h.is_null(), "open failed");
        h
    }

    #[test]
    fn ffi_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_store(dir.path());

        // write ×3
        for i in 0..3i32 {
            let payload = [i as u8; 32];
            let code = strata_write(h, i, -i, i as u16, payload.as_ptr(), payload.len());
            assert_eq!(code, OK);
        }
        assert_eq!(strata_flush(h), OK);

        // read 回一致
        for i in 0..3i32 {
            let mut out_ptr: *mut u8 = ptr::null_mut();
            let mut out_len: usize = 0;
            let code = strata_read(h, i, -i, i as u16, &mut out_ptr, &mut out_len);
            assert_eq!(code, OK);
            assert!(!out_ptr.is_null());
            let got = unsafe { std::slice::from_raw_parts(out_ptr, out_len) };
            assert_eq!(got, vec![i as u8; 32].as_slice());
            strata_read_free(out_ptr, out_len);
        }

        // 不存在的键 → 3
        let mut out_ptr: *mut u8 = ptr::null_mut();
        let mut out_len: usize = 0;
        let code = strata_read(h, 999, 999, 0, &mut out_ptr, &mut out_len);
        assert_eq!(code, MISSING);
        assert!(out_ptr.is_null());

        strata_close(h);
        // NULL 关闭为无操作
        strata_close(ptr::null_mut());
    }

    #[test]
    fn last_error_after_null_pointer() {
        let code = strata_write(ptr::null_mut(), 0, 0, 0, ptr::null(), 0);
        assert_eq!(code, ERR);

        let mut buf = [0u8; 128];
        let n = strata_last_error(buf.as_mut_ptr(), buf.len());
        assert!(n > 0, "last_error should be non-empty after a failure");
        let msg = std::str::from_utf8(&buf[..n as usize]).unwrap();
        assert!(msg.contains("null pointer"), "got: {msg}");
    }

    #[test]
    fn open_null_root_reports_error() {
        let h = strata_open(ptr::null(), 3, 1, 9, 1, 1, 512, 64 * 1024 * 1024, 1);
        assert!(h.is_null());
        let mut buf = [0u8; 64];
        let n = strata_last_error(buf.as_mut_ptr(), buf.len());
        assert!(n > 0);
    }

    #[test]
    fn version_string() {
        let mut buf = [0u8; 64];
        let n = strata_version(buf.as_mut_ptr(), buf.len());
        assert!(n > 0);
        let s = std::str::from_utf8(&buf[..n as usize]).unwrap();
        assert!(s.starts_with("strata-ffi "));
    }
}
