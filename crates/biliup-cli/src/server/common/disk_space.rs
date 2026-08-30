//! 文件系统可用空间探测。
//!
//! 只回答一个问题：某个路径所在的文件系统还剩多少字节可写。响度标准化的准入水位与
//! 转码期硬水位都靠它，两者都在探测不出结果时放行——平台能力缺失只降级，不失败。

use std::path::Path;

/// 返回 `path` 所在文件系统对**非特权用户**可用的字节数。
///
/// `path` 可以是文件（用其所在目录）或目录本身，且不必已经存在——产物还没写出来时也要
/// 能问。探测不出来时返回 `None`，调用方一律按「不做限制」处理。
pub fn available_bytes(path: &Path) -> Option<u64> {
    let directory = if path.is_dir() {
        path
    } else {
        path.parent()?
    };
    statvfs_available(directory)
}

/// `f_bavail` / `f_frsize` 的宽度随平台而异（64 位 Linux 与 macOS 上是 u64，32 位平台上
/// 是 u32），所以一律走 `try_from`——在宽度已是 u64 的平台上它确实是恒等转换，clippy 的
/// 提醒在这里是平台相关的假阳性。
#[cfg(unix)]
#[allow(clippy::useless_conversion, clippy::unnecessary_fallible_conversions)]
fn statvfs_available(directory: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let raw = CString::new(directory.as_os_str().as_bytes()).ok()?;
    // SAFETY: `raw` 是有效的 NUL 结尾 C 字符串，`stat` 是本地栈上的可写 statvfs。
    let stat = unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(raw.as_ptr(), &mut stat) != 0 {
            return None;
        }
        stat
    };
    // `f_bavail` 是非特权用户可用的块数；`f_bfree` 还含 root 保留块，用它会高估。
    let blocks = u64::try_from(stat.f_bavail).ok()?;
    // 少数文件系统不填 `f_frsize`，退回 `f_bsize`。
    let block_size = match u64::try_from(stat.f_frsize).ok()? {
        0 => u64::try_from(stat.f_bsize).ok()?,
        size => size,
    };
    blocks.checked_mul(block_size)
}

/// 非 unix 平台没有对应的免依赖探测手段。返回 `None` 让调用方按不限制处理，与
/// [`super::process_priority`] 在非 unix 上退化为 no-op 是同一个取舍。
#[cfg(not(unix))]
fn statvfs_available(_directory: &Path) -> Option<u64> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn reports_a_positive_figure_for_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(available_bytes(dir.path()).is_some_and(|bytes| bytes > 0));
    }

    #[test]
    fn a_file_path_reports_the_space_of_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("segment.flv");
        std::fs::write(&file, b"x").unwrap();

        let for_directory = available_bytes(dir.path()).unwrap();
        let for_file = available_bytes(&file).unwrap();
        let missing = available_bytes(&dir.path().join("not-written-yet.part.flv")).unwrap();

        // 并发写入会让读数漂移，所以比的是同一量级而不是相等。
        let drift = for_directory.abs_diff(for_file);
        assert!(
            drift < for_directory / 100 + 1024 * 1024,
            "{for_file} should track {for_directory}"
        );
        assert!(missing.abs_diff(for_directory) < for_directory / 100 + 1024 * 1024);
    }

    #[test]
    fn an_unreachable_path_reports_nothing_instead_of_panicking() {
        assert_eq!(
            available_bytes(Path::new("/definitely-not-a-mount-point-9c1f/segment.flv")),
            None
        );
    }

    #[test]
    fn agrees_with_an_independent_statvfs_reading() {
        let dir = tempfile::tempdir().unwrap();
        let ours = available_bytes(dir.path()).unwrap();
        let df = std::process::Command::new("df")
            .arg("-k")
            .arg(dir.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&df.stdout);
        let Some(available_kib) = stdout
            .lines()
            .nth(1)
            .and_then(|line| line.split_whitespace().nth(3))
            .and_then(|value| value.parse::<u64>().ok())
        else {
            // `df` 的列布局因平台而异；解析不出来就不把它当断言依据。
            return;
        };
        let reference = available_kib * 1024;
        assert!(
            ours.abs_diff(reference) < reference / 20 + 64 * 1024 * 1024,
            "ours={ours} df={reference}"
        );
    }
}
