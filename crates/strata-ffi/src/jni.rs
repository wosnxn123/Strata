//! JNI 导出符号层：`Java_dev_strata_bridge_StrataNative_*`（零依赖手写 JNI FFI）。
//!
//! 对应 Java 侧 `dev.strata.bridge.StrataNative` 的 9 个 `private static native`
//! 声明（见 `canvas-patch/canvas-server/src/main/java/dev/strata/bridge/
//! StrataNative.java`）。JNI 是稳定 C ABI：`JNIEnv` 是指向函数指针表
//! （`JNINativeInterface_`）的指针。本模块按 OpenJDK `jni.h` 的字段顺序手写
//! 该表（0–228，即本 crate 用到的最后一个槽位 `ExceptionCheck`）：被调用的
//! 入口按精确 C 签名声明为 `Option<unsafe extern "system" fn>`，其余槽位以
//! `usize` 占位（同为指针宽度，`#[repr(C)]` 逐槽对齐）。编译期 `offset_of!`
//! 断言把每个被用入口钉在其规范索引上——任何转录错位都会编译失败。
//!
//! 核心逻辑全部复用 `lib.rs` 的 `core_*`（与 C ABI 同一套）；错误写入
//! thread-local `LAST_ERROR`，可经 `lastErrorNative` 读取。panic 不穿越 JNI
//! 边界（所有入口以 `catch_unwind` 包裹）。
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use crate::{
    core_close, core_flush, core_gc, core_open, core_read, core_tier, core_version, core_write,
    guarded_i32, last_error_message, panic_message, set_last_error, ERR, OK,
};

/// JNI 引用类型在本模块内一律是不透明指针（不解引用，仅回传给 JVM）。
pub(crate) type JniEnv = *mut c_void;
type JClass = *mut c_void;
type JString = *mut c_void;
type JByteArray = *mut c_void;

/// `Release*ArrayElements` 模式：只读借用，不回写（等价 jni.h 的 JNI_ABORT）。
const RELEASE_ABORT: i32 = 2;

/* ------------------------------------------------------------------ */
/* JNINativeInterface_ 函数指针表（字段顺序与 OpenJDK jni.h 逐一对应）  */
/* ------------------------------------------------------------------ */

/// JNI 函数表，字段 0–228。注释中的编号即规范索引（`GetVersion` = 4，
/// 0–3 为保留槽）。被本模块调用的入口带精确签名；其余槽位为 `usize`
/// 占位。字段顺序即 ABI，任何增删都必须对照 jni.h。
#[repr(C)]
#[allow(non_snake_case, missing_docs)]
struct JNINativeInterface {
    reserved0: usize,
    reserved1: usize,
    reserved2: usize,
    reserved3: usize,
    GetVersion: usize,                          // 4
    DefineClass: usize,                         // 5
    FindClass: usize,                           // 6
    FromReflectedMethod: usize,                 // 7
    FromReflectedField: usize,                  // 8
    ToReflectedMethod: usize,                   // 9
    GetSuperclass: usize,                       // 10
    IsAssignableFrom: usize,                    // 11
    ToReflectedField: usize,                    // 12
    Throw: usize,                               // 13
    ThrowNew: usize,                            // 14
    ExceptionOccurred: usize,                   // 15
    ExceptionDescribe: usize,                   // 16
    ExceptionClear: Option<unsafe extern "system" fn(JniEnv)>, // 17
    FatalError: usize,                          // 18
    PushLocalFrame: usize,                      // 19
    PopLocalFrame: usize,                       // 20
    NewGlobalRef: usize,                        // 21
    DeleteGlobalRef: usize,                     // 22
    DeleteLocalRef: usize,                      // 23
    IsSameObject: usize,                        // 24
    NewLocalRef: usize,                         // 25
    EnsureLocalCapacity: usize,                 // 26
    AllocObject: usize,                         // 27
    NewObject: usize,                           // 28
    NewObjectV: usize,                          // 29
    NewObjectA: usize,                          // 30
    GetObjectClass: usize,                      // 31
    IsInstanceOf: usize,                        // 32
    GetMethodID: usize,                         // 33
    CallObjectMethod: usize,                    // 34
    CallObjectMethodV: usize,                   // 35
    CallObjectMethodA: usize,                   // 36
    CallBooleanMethod: usize,                   // 37
    CallBooleanMethodV: usize,                  // 38
    CallBooleanMethodA: usize,                  // 39
    CallByteMethod: usize,                      // 40
    CallByteMethodV: usize,                     // 41
    CallByteMethodA: usize,                     // 42
    CallCharMethod: usize,                      // 43
    CallCharMethodV: usize,                     // 44
    CallCharMethodA: usize,                     // 45
    CallShortMethod: usize,                     // 46
    CallShortMethodV: usize,                    // 47
    CallShortMethodA: usize,                    // 48
    CallIntMethod: usize,                       // 49
    CallIntMethodV: usize,                      // 50
    CallIntMethodA: usize,                      // 51
    CallLongMethod: usize,                      // 52
    CallLongMethodV: usize,                     // 53
    CallLongMethodA: usize,                     // 54
    CallFloatMethod: usize,                     // 55
    CallFloatMethodV: usize,                    // 56
    CallFloatMethodA: usize,                    // 57
    CallDoubleMethod: usize,                    // 58
    CallDoubleMethodV: usize,                   // 59
    CallDoubleMethodA: usize,                   // 60
    CallVoidMethod: usize,                      // 61
    CallVoidMethodV: usize,                     // 62
    CallVoidMethodA: usize,                     // 63
    CallNonvirtualObjectMethod: usize,          // 64
    CallNonvirtualObjectMethodV: usize,         // 65
    CallNonvirtualObjectMethodA: usize,         // 66
    CallNonvirtualBooleanMethod: usize,         // 67
    CallNonvirtualBooleanMethodV: usize,        // 68
    CallNonvirtualBooleanMethodA: usize,        // 69
    CallNonvirtualByteMethod: usize,            // 70
    CallNonvirtualByteMethodV: usize,           // 71
    CallNonvirtualByteMethodA: usize,           // 72
    CallNonvirtualCharMethod: usize,            // 73
    CallNonvirtualCharMethodV: usize,           // 74
    CallNonvirtualCharMethodA: usize,           // 75
    CallNonvirtualShortMethod: usize,           // 76
    CallNonvirtualShortMethodV: usize,          // 77
    CallNonvirtualShortMethodA: usize,          // 78
    CallNonvirtualIntMethod: usize,             // 79
    CallNonvirtualIntMethodV: usize,            // 80
    CallNonvirtualIntMethodA: usize,            // 81
    CallNonvirtualLongMethod: usize,            // 82
    CallNonvirtualLongMethodV: usize,           // 83
    CallNonvirtualLongMethodA: usize,           // 84
    CallNonvirtualFloatMethod: usize,           // 85
    CallNonvirtualFloatMethodV: usize,          // 86
    CallNonvirtualFloatMethodA: usize,          // 87
    CallNonvirtualDoubleMethod: usize,          // 88
    CallNonvirtualDoubleMethodV: usize,         // 89
    CallNonvirtualDoubleMethodA: usize,         // 90
    CallNonvirtualVoidMethod: usize,            // 91
    CallNonvirtualVoidMethodV: usize,           // 92
    CallNonvirtualVoidMethodA: usize,           // 93
    GetFieldID: usize,                          // 94
    GetObjectField: usize,                      // 95
    GetBooleanField: usize,                     // 96
    GetByteField: usize,                        // 97
    GetCharField: usize,                        // 98
    GetShortField: usize,                       // 99
    GetIntField: usize,                         // 100
    GetLongField: usize,                        // 101
    GetFloatField: usize,                       // 102
    GetDoubleField: usize,                      // 103
    SetObjectField: usize,                      // 104
    SetBooleanField: usize,                     // 105
    SetByteField: usize,                        // 106
    SetCharField: usize,                        // 107
    SetShortField: usize,                       // 108
    SetIntField: usize,                         // 109
    SetLongField: usize,                        // 110
    SetFloatField: usize,                       // 111
    SetDoubleField: usize,                      // 112
    GetStaticMethodID: usize,                   // 113
    CallStaticObjectMethod: usize,              // 114
    CallStaticObjectMethodV: usize,             // 115
    CallStaticObjectMethodA: usize,             // 116
    CallStaticBooleanMethod: usize,             // 117
    CallStaticBooleanMethodV: usize,            // 118
    CallStaticBooleanMethodA: usize,            // 119
    CallStaticByteMethod: usize,                // 120
    CallStaticByteMethodV: usize,               // 121
    CallStaticByteMethodA: usize,               // 122
    CallStaticCharMethod: usize,                // 123
    CallStaticCharMethodV: usize,               // 124
    CallStaticCharMethodA: usize,               // 125
    CallStaticShortMethod: usize,               // 126
    CallStaticShortMethodV: usize,              // 127
    CallStaticShortMethodA: usize,              // 128
    CallStaticIntMethod: usize,                 // 129
    CallStaticIntMethodV: usize,                // 130
    CallStaticIntMethodA: usize,                // 131
    CallStaticLongMethod: usize,                // 132
    CallStaticLongMethodV: usize,               // 133
    CallStaticLongMethodA: usize,               // 134
    CallStaticFloatMethod: usize,               // 135
    CallStaticFloatMethodV: usize,              // 136
    CallStaticFloatMethodA: usize,              // 137
    CallStaticDoubleMethod: usize,              // 138
    CallStaticDoubleMethodV: usize,             // 139
    CallStaticDoubleMethodA: usize,             // 140
    CallStaticVoidMethod: usize,                // 141
    CallStaticVoidMethodV: usize,               // 142
    CallStaticVoidMethodA: usize,               // 143
    GetStaticFieldID: usize,                    // 144
    GetStaticObjectField: usize,                // 145
    GetStaticBooleanField: usize,               // 146
    GetStaticByteField: usize,                  // 147
    GetStaticCharField: usize,                  // 148
    GetStaticShortField: usize,                 // 149
    GetStaticIntField: usize,                   // 150
    GetStaticLongField: usize,                  // 151
    GetStaticFloatField: usize,                 // 152
    GetStaticDoubleField: usize,                // 153
    SetStaticObjectField: usize,                // 154
    SetStaticBooleanField: usize,               // 155
    SetStaticByteField: usize,                  // 156
    SetStaticCharField: usize,                  // 157
    SetStaticShortField: usize,                 // 158
    SetStaticIntField: usize,                   // 159
    SetStaticLongField: usize,                  // 160
    SetStaticFloatField: usize,                 // 161
    SetStaticDoubleField: usize,                // 162
    NewString: usize,                           // 163
    GetStringLength: usize,                     // 164
    GetStringChars: usize,                      // 165
    ReleaseStringChars: usize,                  // 166
    NewStringUTF: Option<unsafe extern "system" fn(JniEnv, *const c_char) -> JString>, // 167
    GetStringUTFLength: usize,                  // 168
    GetStringUTFChars: Option<unsafe extern "system" fn(JniEnv, JString, *mut u8) -> *const c_char>, // 169
    ReleaseStringUTFChars: Option<unsafe extern "system" fn(JniEnv, JString, *const c_char)>, // 170
    GetArrayLength: Option<unsafe extern "system" fn(JniEnv, JByteArray) -> i32>, // 171
    NewObjectArray: usize,                      // 172
    GetObjectArrayElement: usize,               // 173
    SetObjectArrayElement: usize,               // 174
    NewBooleanArray: usize,                     // 175
    NewByteArray: Option<unsafe extern "system" fn(JniEnv, i32) -> JByteArray>, // 176
    NewCharArray: usize,                        // 177
    NewShortArray: usize,                       // 178
    NewIntArray: usize,                         // 179
    NewLongArray: usize,                        // 180
    NewFloatArray: usize,                       // 181
    NewDoubleArray: usize,                      // 182
    GetBooleanArrayElements: usize,             // 183
    GetByteArrayElements: Option<unsafe extern "system" fn(JniEnv, JByteArray, *mut u8) -> *mut i8>, // 184
    GetCharArrayElements: usize,                // 185
    GetShortArrayElements: usize,               // 186
    GetIntArrayElements: usize,                 // 187
    GetLongArrayElements: usize,                // 188
    GetFloatArrayElements: usize,               // 189
    GetDoubleArrayElements: usize,              // 190
    ReleaseBooleanArrayElements: usize,         // 191
    ReleaseByteArrayElements: Option<unsafe extern "system" fn(JniEnv, JByteArray, *mut i8, i32)>, // 192
    ReleaseCharArrayElements: usize,            // 193
    ReleaseShortArrayElements: usize,           // 194
    ReleaseIntArrayElements: usize,             // 195
    ReleaseLongArrayElements: usize,            // 196
    ReleaseFloatArrayElements: usize,           // 197
    ReleaseDoubleArrayElements: usize,          // 198
    GetBooleanArrayRegion: usize,               // 199
    GetByteArrayRegion: usize,                  // 200
    GetCharArrayRegion: usize,                  // 201
    GetShortArrayRegion: usize,                 // 202
    GetIntArrayRegion: usize,                   // 203
    GetLongArrayRegion: usize,                  // 204
    GetFloatArrayRegion: usize,                 // 205
    GetDoubleArrayRegion: usize,                // 206
    SetBooleanArrayRegion: usize,               // 207
    SetByteArrayRegion: Option<unsafe extern "system" fn(JniEnv, JByteArray, i32, i32, *const i8)>, // 208
    SetCharArrayRegion: usize,                  // 209
    SetShortArrayRegion: usize,                 // 210
    SetIntArrayRegion: usize,                   // 211
    SetLongArrayRegion: usize,                  // 212
    SetFloatArrayRegion: usize,                 // 213
    SetDoubleArrayRegion: usize,                // 214
    RegisterNatives: usize,                     // 215
    UnregisterNatives: usize,                   // 216
    MonitorEnter: usize,                        // 217
    MonitorExit: usize,                         // 218
    GetJavaVM: usize,                           // 219
    GetStringRegion: usize,                     // 220
    GetStringUTFRegion: usize,                  // 221
    GetPrimitiveArrayCritical: usize,           // 222
    ReleasePrimitiveArrayCritical: usize,       // 223
    GetStringCritical: usize,                   // 224
    ReleaseStringCritical: usize,               // 225
    NewWeakGlobalRef: usize,                    // 226
    DeleteWeakGlobalRef: usize,                 // 227
    ExceptionCheck: Option<unsafe extern "system" fn(JniEnv) -> u8>, // 228
}

/// 编译期把每个被调用入口钉在其规范 JNI 索引上（索引在 JNI 1.1 起固定）。
macro_rules! assert_jni_index {
    ($field:ident = $idx:literal) => {
        const _: () = assert!(
            std::mem::offset_of!(JNINativeInterface, $field)
                == $idx * std::mem::size_of::<usize>(),
            concat!(
                "JNI vtable index mismatch: ",
                stringify!($field),
                " expected at ",
                stringify!($idx),
            ),
        );
    };
}

assert_jni_index!(GetVersion = 4);
assert_jni_index!(ExceptionClear = 17);
assert_jni_index!(NewStringUTF = 167);
assert_jni_index!(GetStringUTFChars = 169);
assert_jni_index!(ReleaseStringUTFChars = 170);
assert_jni_index!(GetArrayLength = 171);
assert_jni_index!(NewByteArray = 176);
assert_jni_index!(GetByteArrayElements = 184);
assert_jni_index!(ReleaseByteArrayElements = 192);
assert_jni_index!(SetByteArrayRegion = 208);
assert_jni_index!(ExceptionCheck = 228);

/* ------------------------------------------------------------------ */
/* JNIEnv → 函数表                                                     */
/* ------------------------------------------------------------------ */

/// `JNIEnv` 解引用后得到函数表指针。
///
/// # Safety
///
/// `env` 必须是当前线程有效的 `JNIEnv*`（JVM 调用 native 方法的约定）。
unsafe fn functions(env: JniEnv) -> &'static JNINativeInterface {
    // SAFETY: JNIEnv 指向函数表指针（见上）。
    let vtbl = unsafe { *(env as *const *const JNINativeInterface) };
    // SAFETY: 函数表在 VM 生命周期内常驻。
    unsafe { &*vtbl }
}

/// 从函数表槽位还原函数指针（None = 槽位为 NULL，属于 VM 违约）。
fn slot<F: Copy>(f: &Option<F>) -> Result<F, String> {
    f.ok_or_else(|| "JNI vtable slot is NULL".to_string())
}

/// 若存在未决 Java 异常则清除（防御性：本模块的入口只应被无异常地调用）。
///
/// # Safety
///
/// `env` 有效性契约见 [`functions`]。
unsafe fn clear_pending_exception(env: JniEnv) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: env 有效性契约由调用方转达（见上）。
        let fns = unsafe { functions(env) };
        let (Ok(check), Ok(clear)) = (slot(&fns.ExceptionCheck), slot(&fns.ExceptionClear))
        else {
            return;
        };
        // SAFETY: 槽位签名由 `JNINativeInterface` 的字段声明保证。
        if unsafe { check(env) } != 0 {
            unsafe { clear(env) };
        }
    }));
}

/* ------------------------------------------------------------------ */
/* JNI 类型转换助手                                                     */
/* ------------------------------------------------------------------ */

/// jstring（modified UTF-8）→ Rust `String`；NULL/非 UTF-8/VM 失败均 Err。
fn jstring_to_string(env: JniEnv, s: JString) -> Result<String, String> {
    if s.is_null() {
        return Err("null jstring".to_string());
    }
    // SAFETY: env 有效性契约见各导出函数（JVM 约定）。
    let fns = unsafe { functions(env) };
    let get_chars = slot(&fns.GetStringUTFChars)?;
    let release_chars = slot(&fns.ReleaseStringUTFChars)?;
    // SAFETY: s 为非 NULL jstring（上方已校验）；isCopy 传 NULL 表示不关心。
    let chars = unsafe { get_chars(env, s, ptr::null_mut()) };
    if chars.is_null() {
        return Err("GetStringUTFChars failed (pending exception or OOM)".to_string());
    }
    // SAFETY: 返回指针 NUL 结尾，且在 Release 之前读之有效。
    let text = unsafe { CStr::from_ptr(chars) }
        .to_str()
        .map_err(|_| "jstring is not valid modified UTF-8".to_string())
        .map(|t| t.to_string());
    // SAFETY: chars 来自同一 env 对同一 s 的 get_chars。
    unsafe { release_chars(env, s, chars) };
    text
}

/// jbyteArray → 新 `Vec<u8>`（只读借用后复制，`JNI_ABORT` 释放不回写）。
/// NULL → 空负载（与 C ABI 的 `nbt == NULL && len == 0` 语义一致）。
fn jbyte_array_to_vec(env: JniEnv, arr: JByteArray) -> Result<Vec<u8>, String> {
    if arr.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: env 有效性契约见各导出函数（JVM 约定）。
    let fns = unsafe { functions(env) };
    let get_len = slot(&fns.GetArrayLength)?;
    let get_elems = slot(&fns.GetByteArrayElements)?;
    let release_elems = slot(&fns.ReleaseByteArrayElements)?;
    // SAFETY: arr 为非 NULL jbyteArray（上方已校验）。
    let len = unsafe { get_len(env, arr) };
    let len = usize::try_from(len).map_err(|_| "negative array length".to_string())?;
    // SAFETY: 同上。
    let elems = unsafe { get_elems(env, arr, ptr::null_mut()) };
    if elems.is_null() {
        return Err("GetByteArrayElements failed (pending exception or OOM)".to_string());
    }
    // SAFETY: elems 指向 len 个已初始化字节（GetByteArrayElements 契约）。
    let data = unsafe { std::slice::from_raw_parts(elems.cast::<u8>(), len) }.to_vec();
    // SAFETY: elems 来自上方 get_elems；RELEASE_ABORT = 只读不回写。
    unsafe { release_elems(env, arr, elems, RELEASE_ABORT) };
    Ok(data)
}

/// `&[u8]` → 新 jbyteArray（`NewByteArray` + `SetByteArrayRegion`）。
fn vec_to_jbyte_array(env: JniEnv, data: &[u8]) -> Result<JByteArray, String> {
    // SAFETY: env 有效性契约见各导出函数（JVM 约定）。
    let fns = unsafe { functions(env) };
    let new_arr = slot(&fns.NewByteArray)?;
    let set_region = slot(&fns.SetByteArrayRegion)?;
    let len = i32::try_from(data.len()).map_err(|_| "payload exceeds jsize".to_string())?;
    // SAFETY: len >= 0（try_from 已校验）。
    let arr = unsafe { new_arr(env, len) };
    if arr.is_null() {
        return Err("NewByteArray failed (OOM)".to_string());
    }
    if !data.is_empty() {
        // SAFETY: arr 来自本线程同 env 的 new_arr；data 指向 len 个有效字节。
        unsafe { set_region(env, arr, 0, len, data.as_ptr().cast::<i8>()) };
    }
    Ok(arr)
}

/// Rust `&str` → 新 jstring；调用方保证不返回 NULL（VM 失败除外）。
fn string_to_jstring(env: JniEnv, text: &str) -> Result<JString, String> {
    // SAFETY: env 有效性契约见各导出函数（JVM 约定）。
    let fns = unsafe { functions(env) };
    let new_utf = slot(&fns.NewStringUTF)?;
    let c = CString::new(text).map_err(|_| "text contains interior NUL".to_string())?;
    // SAFETY: c 为 NUL 结尾的合法 C 字符串。
    let s = unsafe { new_utf(env, c.as_ptr()) };
    if s.is_null() {
        return Err("NewStringUTF failed (OOM or pending exception)".to_string());
    }
    Ok(s)
}

/* ------------------------------------------------------------------ */
/* 边界守卫（与 lib.rs 的 guarded_* 同款，适配 JNI 返回类型）           */
/* ------------------------------------------------------------------ */

/// 运行闭包；Err/panic 置 LAST_ERROR 并返回 NULL（用于返回 jobject 的入口）。
fn guarded_jobject(f: impl FnOnce() -> Result<*mut c_void, String>) -> *mut c_void {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
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

/// 运行闭包；Err/panic 置 LAST_ERROR 并返回 NULL jstring（用于字符串入口）。
fn guarded_jstring(f: impl FnOnce() -> Result<JString, String>) -> JString {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(s)) => s,
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

/* ------------------------------------------------------------------ */
/* 9 个导出符号（与 StrataNative.java 的 native 声明一一对应）          */
/* ------------------------------------------------------------------ */

/// `private static native long openNative(String root, int hotLevel,
/// int hotEnabled, int coldLevel, int coldEnabled, int dictionary,
/// long cacheMb, long segmentMaxBytes)` → 句柄（失败 0，详情 lastErrorNative）。
///
/// # Safety
///
/// 由 JVM 以有效 `env` 调用；`root` 为 jstring 引用或 NULL。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_dev_strata_bridge_StrataNative_openNative(
    env: JniEnv,
    _class: JClass,
    root: JString,
    hot_level: i32,
    hot_enabled: i32,
    cold_level: i32,
    cold_enabled: i32,
    dictionary: i32,
    cache_mb: i64,
    segment_max_bytes: i64,
) -> i64 {
    // SAFETY: env 有效性契约见上。
    unsafe { clear_pending_exception(env) };
    guarded_jobject(move || {
        let root = jstring_to_string(env, root)?;
        core_open(
            &root,
            hot_level,
            hot_enabled,
            cold_level,
            cold_enabled,
            dictionary,
            cache_mb as u64,
            segment_max_bytes as u64,
        )
    }) as i64
}

/// `private static native int writeNative(long handle, int x, int z,
/// short typeId, byte[] nbt)` → 状态码（0 成功；1/2 见 lastErrorNative）。
///
/// # Safety
///
/// 由 JVM 以有效 `env` 调用。
#[no_mangle]
pub extern "system" fn Java_dev_strata_bridge_StrataNative_writeNative(
    env: JniEnv,
    _class: JClass,
    handle: i64,
    x: i32,
    z: i32,
    type_id: i16,
    nbt: JByteArray,
) -> i32 {
    // SAFETY: env 有效性契约见上。
    unsafe { clear_pending_exception(env) };
    // 先把 payload 拷出（JNI 调用可能失败），再进核心逻辑。
    let payload = match jbyte_array_to_vec(env, nbt) {
        Ok(v) => v,
        Err(msg) => {
            set_last_error(msg);
            return ERR;
        }
    };
    guarded_i32(move || {
        core_write(handle as *mut c_void, x, z, type_id as u16, &payload)?;
        Ok(OK)
    })
}

/// `private static native byte[] readNative(long handle, int x, int z,
/// short typeId)` → 负载数组；键无记录返回 NULL；错误置 lastErrorNative
/// 并返回 NULL。
///
/// # Safety
///
/// 由 JVM 以有效 `env` 调用。
#[no_mangle]
pub extern "system" fn Java_dev_strata_bridge_StrataNative_readNative(
    env: JniEnv,
    _class: JClass,
    handle: i64,
    x: i32,
    z: i32,
    type_id: i16,
) -> JByteArray {
    // SAFETY: env 有效性契约见上。
    unsafe { clear_pending_exception(env) };
    guarded_jobject(move || match core_read(handle as *mut c_void, x, z, type_id as u16)? {
        Some(data) => vec_to_jbyte_array(env, &data),
        None => Ok(ptr::null_mut()),
    })
}

/// `private static native int flushNative(long handle)` → 状态码。
///
/// # Safety
///
/// 由 JVM 以有效 `env` 调用。
#[no_mangle]
pub extern "system" fn Java_dev_strata_bridge_StrataNative_flushNative(
    env: JniEnv,
    _class: JClass,
    handle: i64,
) -> i32 {
    // SAFETY: env 有效性契约见上。
    unsafe { clear_pending_exception(env) };
    guarded_i32(move || {
        core_flush(handle as *mut c_void)?;
        Ok(OK)
    })
}

/// `private static native int gcNative(long handle, double threshold,
/// long budgetBytes, long minHoleBytes)` → 状态码。uint64 参数以 jlong
/// 传入，按位解释（负值 = 超大无符号值）。
///
/// # Safety
///
/// 由 JVM 以有效 `env` 调用。
#[no_mangle]
pub extern "system" fn Java_dev_strata_bridge_StrataNative_gcNative(
    env: JniEnv,
    _class: JClass,
    handle: i64,
    threshold: f64,
    budget_bytes: i64,
    min_hole_bytes: i64,
) -> i32 {
    // SAFETY: env 有效性契约见上。
    unsafe { clear_pending_exception(env) };
    guarded_i32(move || {
        core_gc(
            handle as *mut c_void,
            threshold,
            budget_bytes as u64,
            min_hole_bytes as u64,
        )?;
        Ok(OK)
    })
}

/// `private static native int tierNative(long handle, int enabled,
/// int stableFlushes, double demoteRatio)` → 状态码。
///
/// # Safety
///
/// 由 JVM 以有效 `env` 调用。
#[no_mangle]
pub extern "system" fn Java_dev_strata_bridge_StrataNative_tierNative(
    env: JniEnv,
    _class: JClass,
    handle: i64,
    enabled: i32,
    stable_flushes: i32,
    demote_ratio: f64,
) -> i32 {
    // SAFETY: env 有效性契约见上。
    unsafe { clear_pending_exception(env) };
    guarded_i32(move || {
        core_tier(
            handle as *mut c_void,
            enabled,
            stable_flushes as u32,
            demote_ratio,
        )?;
        Ok(OK)
    })
}

/// `private static native void closeNative(long handle)`；0 句柄为无操作。
///
/// # Safety
///
/// 由 JVM 以有效 `env` 调用。
#[no_mangle]
pub extern "system" fn Java_dev_strata_bridge_StrataNative_closeNative(
    env: JniEnv,
    _class: JClass,
    handle: i64,
) {
    // SAFETY: env 有效性契约见上。
    unsafe { clear_pending_exception(env) };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        core_close(handle as *mut c_void);
    }));
}

/// `private static native String lastErrorNative()` → 本线程最近错误；
/// 无错误时返回空串。
///
/// # Safety
///
/// 由 JVM 以有效 `env` 调用。
#[no_mangle]
pub extern "system" fn Java_dev_strata_bridge_StrataNative_lastErrorNative(
    env: JniEnv,
    _class: JClass,
) -> JString {
    // SAFETY: env 有效性契约见上。
    unsafe { clear_pending_exception(env) };
    guarded_jstring(move || {
        let msg = last_error_message();
        string_to_jstring(env, &msg)
    })
}

/// `private static native String versionNative()` → `"strata-ffi <version>"`。
///
/// # Safety
///
/// 由 JVM 以有效 `env` 调用。
#[no_mangle]
pub extern "system" fn Java_dev_strata_bridge_StrataNative_versionNative(
    env: JniEnv,
    _class: JClass,
) -> JString {
    // SAFETY: env 有效性契约见上。
    unsafe { clear_pending_exception(env) };
    guarded_jstring(move || {
        let text = core_version();
        string_to_jstring(env, text)
    })
}
