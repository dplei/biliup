//! 把上传前的 ffmpeg 预处理降到后台优先级，让网页请求优先拿到 CPU 和磁盘。
//!
//! 预处理（响度标准化、时间戳检测/修复、分段恢复合并）是长时间的 CPU/IO 密集任务，
//! 而网页请求是短促突发。Linux CFS 下 nice 19 与 nice 0 的权重比接近 1:100，降级后
//! 网页几乎总能立刻抢到 CPU；机器空闲时 ffmpeg 依然全速跑，单段处理总时长基本不变。
//!
//! 录制进程（`core/downloader`）和用户自定义 hook 不在此列：前者要实时写流，降优先级
//! 可能丢帧；后者该占多少资源由用户自己决定。

/// 后台预处理进程的 nice 值。
#[cfg(unix)]
const BACKGROUND_NICE: libc::c_int = 19;

/// `IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT | 7`：best-effort 里的最低一档，无需特权即可设置。
/// 不用 IDLE 类是因为预处理本来就要读写 GB 级文件，完全饿死只会让上传排队变长。
#[cfg(target_os = "linux")]
const IOPRIO_BACKGROUND: libc::c_long = (2 << 13) | 7;

/// 让 ffmpeg/ffprobe 子进程以后台优先级运行。
pub fn background(command: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(unix)]
    unsafe {
        command.pre_exec(demote);
    }
    command
}

/// `background` 的同步 `Command` 版本。
pub fn background_std(command: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(demote);
    }
    command
}

/// fork 之后、exec 之前把自己降级。
///
/// 降级只是优化：设不上就按普通优先级跑，绝不能让预处理链路失败，所以一律忽略返回值。
/// 这里只调 `setpriority` / `ioprio_set` 两个裸系统调用，满足 `pre_exec` 要求的
/// async-signal-safe 约束。
#[cfg(unix)]
fn demote() -> std::io::Result<()> {
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS as _, 0, BACKGROUND_NICE);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        // ioprio_set(IOPRIO_WHO_PROCESS = 1, who = 0 表示自己, ioprio)
        libc::syscall(libc::SYS_ioprio_set, 1, 0, IOPRIO_BACKGROUND);
    }
    Ok(())
}
