//! 会话锁：防止两个进程（或同进程两次 open）同时读写同一 vstore。
//!
//! `Store::open` 在触碰任何数据文件前获取 `root/.strata.lock` 的独占锁并
//! 写入持有者信息（PID + 主机名 + 时间戳）；锁句柄随 `Store` 存活，
//! `Store` drop 时解锁并删除锁文件。抢锁失败返回 [`StrataError::Lock`]，
//! 错误信息携带文件中已写入的持有者描述。
//!
//! 实现：Unix 用 `flock(LOCK_EX | LOCK_NB)`；Windows 用 `LockFileEx`
//! （`LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY`），手写 FFI 与
//! `punch.rs` 同风格，不引入额外依赖。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::StrataError;

/// 锁文件名（位于 vstore 根目录）。
pub const LOCK_FILE_NAME: &str = ".strata.lock";

/// 已获取的会话锁；drop 时解锁并尽力删除锁文件。
pub(crate) struct SessionLock {
    path: PathBuf,
    file: File,
}

impl SessionLock {
    /// 获取 `root/.strata.lock` 独占锁。已被占用时返回
    /// [`StrataError::Lock`]（含锁文件中记录的持有者信息）。
    pub(crate) fn acquire(root: &Path) -> Result<Self, StrataError> {
        let path = root.join(LOCK_FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)?;

        if !try_lock_exclusive(&file) {
            // 读取持有者信息（可能为空或不可读，均降级为无信息）。
            let holder = std::fs::read_to_string(&path).unwrap_or_default();
            let holder = holder.trim();
            let detail = if holder.is_empty() {
                format!("`{}` is held by another session", path.display())
            } else {
                format!("`{}` is held by another session: {holder}", path.display())
            };
            return Err(StrataError::Lock(detail));
        }

        // 抢到锁后写入持有者信息（供下一个抢占者报错展示）。
        let _ = file.set_len(0);
        let mut f = &file;
        let info = format!(
            "pid={} host={} opened_unix={}\n",
            std::process::id(),
            hostname(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        let _ = f.write_all(info.as_bytes());
        let _ = f.flush();

        Ok(Self { path, file })
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        unlock(&self.file);
        // 删除失败（例如别的进程正等着读它）不影响正确性：锁已释放，
        // 文件内容过期即可，下一次 acquire 会覆盖。
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 尽力取主机名：环境变量在两大平台都有常见取值，取不到用 unknown。
fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(unix)]
mod platform {
    use std::fs::File;
    use std::os::unix::io::AsRawFd;

    /// `flock(LOCK_EX | LOCK_NB)`：成功 true；已被占用或出错 false。
    pub(super) fn try_lock_exclusive(file: &File) -> bool {
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        rc == 0
    }

    pub(super) fn unlock(file: &File) {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;

    type WinBool = i32;
    type WinHandle = *mut std::ffi::c_void;

    /// 与 winnt 的 OVERLAPPED 布局一致（x86_64：两个 ULONG_PTR + 偏移联合体 + 句柄）。
    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut std::ffi::c_void,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LockFileEx(
            h_file: WinHandle,
            dw_flags: u32,
            dw_reserved: u32,
            n_number_of_bytes_to_lock_low: u32,
            n_number_of_bytes_to_lock_high: u32,
            lp_overlapped: *mut Overlapped,
        ) -> WinBool;
        fn UnlockFileEx(
            h_file: WinHandle,
            dw_reserved: u32,
            n_number_of_bytes_to_unlock_low: u32,
            n_number_of_bytes_to_unlock_high: u32,
            lp_overlapped: *mut Overlapped,
        ) -> WinBool;
    }

    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;

    /// `LockFileEx` 独占锁第 1 个字节，立即失败语义。
    pub(super) fn try_lock_exclusive(file: &File) -> bool {
        let handle = file.as_raw_handle() as WinHandle;
        let mut ov = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event: std::ptr::null_mut(),
        };
        let ok = unsafe {
            LockFileEx(
                handle,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut ov,
            )
        };
        ok != 0
    }

    pub(super) fn unlock(file: &File) {
        let handle = file.as_raw_handle() as WinHandle;
        let mut ov = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event: std::ptr::null_mut(),
        };
        unsafe { UnlockFileEx(handle, 0, 1, 0, &mut ov) };
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::fs::File;

    // 未知平台：无锁可用，退化为"总是成功"（与既有行为一致）。
    pub(super) fn try_lock_exclusive(_file: &File) -> bool {
        true
    }

    pub(super) fn unlock(_file: &File) {}
}

use platform::{try_lock_exclusive, unlock};
