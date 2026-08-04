//! 跨平台 hole punch（为 GC 回收稀疏失效区域而设）。
//!
//! 将文件中的一段区间挖洞：磁盘空间被释放，之后读回该区间得到全零。
//! 所有失败路径（文件系统/平台不支持、系统调用报错）都返回
//! [`PunchOutcome::Unsupported`]，并保证文件内容不变——调用方只需把该区间
//! 视为"未被回收"即可。
//!
//! 调用方（GC）负责把死区间切成 ≤32KB 的子区间逐个挖洞：段扫描器的中部
//! 坏区重同步窗口为 64KB，≤32KB 的洞保证洞后第一条有效记录的 MAGIC 永远
//! 落在窗口内（见 `segment::scan_segment`）。

use std::fs::File;

use crate::StrataError;

/// 一次挖洞操作的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunchOutcome {
    /// 区间已被挖洞：磁盘空间已释放，读回为零。
    Done,
    /// 平台/文件系统不支持挖洞，或系统调用失败；文件内容保持不变。
    Unsupported,
}

/// 将文件区间 `[offset, offset + len)` 挖洞（释放磁盘空间，之后读回为零）。
///
/// 语义约定：
/// - `len == 0` → 返回 [`PunchOutcome::Unsupported`]，文件内容不变；
/// - 平台不支持（无 `fallocate(FALLOC_FL_PUNCH_HOLE)`、卷不支持稀疏归零等）
///   → [`PunchOutcome::Unsupported`]；
/// - Linux 上 `fallocate` 要求区间按文件系统块对齐：实现向内收缩到块边界
///   （绝不外扩触碰区间外的活数据），收缩后为空同样返回 `Unsupported`；
/// - 任何其他系统调用错误同样归为 [`PunchOutcome::Unsupported`]，不向上传播
///   ——挖洞只是优化，失败时保留数据永远比报错更安全；
/// - 仅当 `offset + len` 溢出 `u64` 时返回 [`StrataError::Io`]（非法参数，
///   文件内容同样不变）。
pub fn punch_hole(file: &mut File, offset: u64, len: u64) -> Result<PunchOutcome, StrataError> {
    if len == 0 {
        return Ok(PunchOutcome::Unsupported);
    }
    // offset + len 溢出 u64 视为非法参数（文件内容不变）；
    // 提前在这里拦截，保证两个平台分支行为一致。
    if offset.checked_add(len).is_none() {
        return Err(StrataError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "punch_hole: offset + len overflows u64",
        )));
    }
    #[cfg(unix)]
    {
        unix::punch(file, offset, len)
    }
    #[cfg(windows)]
    {
        windows::punch(file, offset, len)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, offset, len);
        Ok(PunchOutcome::Unsupported)
    }
}

#[cfg(unix)]
mod unix {
    use std::fs::File;
    use std::os::unix::io::AsRawFd;

    use super::{PunchOutcome, StrataError};

    /// Linux/macOS 上通过 `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)`
    /// 挖洞；任何 errno 都按"内容不变"原则映射为 `Unsupported`。
    ///
    /// 许多文件系统要求 offset/len 对齐到文件系统块，未对齐参数直接 EINVAL。
    /// 这里先探测块大小并把区间**向内**收缩到块边界（外扩会归零区间外的活
    /// 数据），收缩后为空则视为不支持。
    pub(super) fn punch(file: &mut File, offset: u64, len: u64) -> Result<PunchOutcome, StrataError> {
        let fd = file.as_raw_fd();

        let block = {
            let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
            let bsize = if unsafe { libc::fstatfs(fd, &mut fs) } == 0 && fs.f_bsize > 0 {
                fs.f_bsize as u64
            } else {
                4096
            };
            bsize.max(1)
        };

        // punch_hole 已保证 offset + len 不溢出。
        let end = offset + len;
        let start = offset.next_multiple_of(block);
        let aligned_end = end / block * block;
        if aligned_end <= start {
            return Ok(PunchOutcome::Unsupported);
        }

        let rc = unsafe {
            libc::fallocate(
                fd,
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                start as libc::off_t,
                (aligned_end - start) as libc::off_t,
            )
        };
        if rc == 0 {
            Ok(PunchOutcome::Done)
        } else {
            // ENOSYS / EOPNOTSUPP / ENOTSUP 表示平台或文件系统不支持；
            // 其他 errno 也一视同仁：挖洞失败绝不破坏数据。
            Ok(PunchOutcome::Unsupported)
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::fs::File;
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;

    // 手写 FFI 绑定（windows-sys 0.59 的 feature 组合下这两项不稳定）。
    type WinBool = i32;
    type WinHandle = *mut std::ffi::c_void;

    #[repr(C)]
    struct FileZeroDataInformation {
        file_offset: i64,
        beyond_final_zero: i64,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn DeviceIoControl(
            h_device: WinHandle,
            dw_io_control_code: u32,
            in_buffer: *const std::ffi::c_void,
            n_in_buffer_size: u32,
            out_buffer: *mut std::ffi::c_void,
            n_out_buffer_size: u32,
            bytes_returned: *mut u32,
            overlapped: *mut std::ffi::c_void,
        ) -> WinBool;
    }

    use super::{PunchOutcome, StrataError};

    /// FSCTL_SET_SPARSE = CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 98, METHOD_BUFFERED, FILE_ANY_ACCESS)
    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
    /// FSCTL_SET_ZERO_DATA = CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 12, METHOD_BUFFERED, FILE_WRITE_DATA)
    const FSCTL_SET_ZERO_DATA: u32 = 0x0009_80C8;

    /// Windows 上先把文件标记为稀疏（FSCTL_SET_SPARSE），再用
    /// FSCTL_SET_ZERO_DATA 把区间归零。任一步失败都返回 `Unsupported`。
    pub(super) fn punch(file: &mut File, offset: u64, len: u64) -> Result<PunchOutcome, StrataError> {
        // punch_hole 已保证 offset + len 不溢出。
        let handle = file.as_raw_handle() as WinHandle;
        let end = offset + len;
        let mut ret: u32 = 0;

        // 1) 设置稀疏属性；失败（例如卷不支持稀疏文件）→ Unsupported。
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Ok(PunchOutcome::Unsupported);
        }

        // 2) 将 [offset, end) 归零（稀疏卷上会真正释放磁盘空间）。
        let info = FileZeroDataInformation {
            file_offset: offset as i64,
            beyond_final_zero: end as i64,
        };
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_SET_ZERO_DATA,
                &info as *const FileZeroDataInformation as *const std::ffi::c_void,
                size_of::<FileZeroDataInformation>() as u32,
                std::ptr::null_mut(),
                0,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Ok(PunchOutcome::Unsupported);
        }

        Ok(PunchOutcome::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punch_makes_range_zero_or_reports_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, vec![0xAAu8; 256 * 1024]).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let r = punch_hole(&mut f, 64 * 1024, 64 * 1024).unwrap();
        let data = std::fs::read(&path).unwrap();
        match r {
            PunchOutcome::Done => assert!(data[64 * 1024..128 * 1024].iter().all(|&b| b == 0)),
            PunchOutcome::Unsupported => assert!(data.iter().all(|&b| b == 0xAA)),
        }
    }

    #[test]
    fn zero_length_hole_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, vec![0u8; 128 * 1024]).unwrap();
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert!(matches!(
            punch_hole(&mut f, 0, 0).unwrap(),
            PunchOutcome::Unsupported
        ));
    }

    #[test]
    fn small_hole_is_attempted() {
        // 不再有 64KB 最小门槛（GC 按 ≤32KB 子区间挖洞）：小区间同样应真正
        // 尝试挖洞，结果取决于文件系统，但内容语义必须符合结果。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, vec![0x5Au8; 128 * 1024]).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let r = punch_hole(&mut f, 32 * 1024, 32 * 1024).unwrap();
        let data = std::fs::read(&path).unwrap();
        match r {
            PunchOutcome::Done => assert!(
                data[32 * 1024..64 * 1024].iter().all(|&b| b == 0)
            ),
            PunchOutcome::Unsupported => assert!(data.iter().all(|&b| b == 0x5A)),
        }
    }

    #[test]
    fn overflowing_range_is_invalid_input_and_content_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, vec![0x11u8; 128 * 1024]).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let err = punch_hole(&mut f, u64::MAX, 128 * 1024).unwrap_err();
        assert!(matches!(err, StrataError::Io(_)));
        assert!(std::fs::read(&path).unwrap().iter().all(|&b| b == 0x11));
    }
}
