//! 跨平台 hole punch（为 GC 回收稀疏失效区域而设）。
//!
//! 将文件中的一段区间挖洞：磁盘空间被释放，之后读回该区间得到全零。
//! 所有失败路径（文件系统/平台不支持、系统调用报错）都返回
//! [`PunchOutcome::Unsupported`]，并保证文件内容不变——调用方只需把该区间
//! 视为"未被回收"即可。

use std::fs::File;

use crate::StrataError;

/// 允许挖洞的最小字节数。
///
/// 取自 NTFS 稀疏分配粒度（64 KiB 簇边界）：小于该区间的挖洞在 Windows 上
/// 无法对齐到可释放的分配单元，因此一律拒绝（返回 `Unsupported`）。
pub const MIN_HOLE_BYTES: u64 = 64 * 1024;

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
/// - `len < MIN_HOLE_BYTES` → 返回 [`PunchOutcome::Unsupported`]，文件内容不变；
/// - 平台不支持（无 `fallocate(FALLOC_FL_PUNCH_HOLE)`、卷不支持稀疏归零等）
///   → [`PunchOutcome::Unsupported`]；
/// - 任何其他系统调用错误同样归为 [`PunchOutcome::Unsupported`]，不向上传播
///   ——挖洞只是优化，失败时保留数据永远比报错更安全；
/// - 仅当 `offset + len` 溢出 `u64` 时返回 [`StrataError::Io`]（非法参数，
///   文件内容同样不变）。
pub fn punch_hole(file: &mut File, offset: u64, len: u64) -> Result<PunchOutcome, StrataError> {
    if len < MIN_HOLE_BYTES {
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
    pub(super) fn punch(file: &mut File, offset: u64, len: u64) -> Result<PunchOutcome, StrataError> {
        let fd = file.as_raw_fd();
        let rc = unsafe {
            libc::fallocate(
                fd,
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                offset as libc::off_t,
                len as libc::off_t,
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
    fn too_small_hole_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, vec![0u8; 128 * 1024]).unwrap();
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert!(matches!(
            punch_hole(&mut f, 0, 4096).unwrap(),
            PunchOutcome::Unsupported
        ));
    }

    #[test]
    fn exact_minimum_boundary_is_attempted() {
        // len == MIN_HOLE_BYTES 不应在阈值检查处被拒，而是真正尝试挖洞
        // （结果取决于文件系统，二者皆合法，但内容语义必须符合结果）。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, vec![0x5Au8; 3 * MIN_HOLE_BYTES as usize]).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let r = punch_hole(&mut f, MIN_HOLE_BYTES, MIN_HOLE_BYTES).unwrap();
        let data = std::fs::read(&path).unwrap();
        match r {
            PunchOutcome::Done => assert!(
                data[MIN_HOLE_BYTES as usize..2 * MIN_HOLE_BYTES as usize]
                    .iter()
                    .all(|&b| b == 0)
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
