//! 用户提供路径的安全解析。
//!
//! 静态文件接口把用户传入的路径直接交给文件服务，等同于任意文件读取。
//! 这里提供统一的收口：给定一个「允许的根目录」，把用户路径解析成根目录内的
//! 绝对路径，任何越界尝试一律拒绝。
//!
//! 之所以做成「根目录」参数而不是写死某个目录：静态文件接口服务的是工作目录下的
//! 日志与录播视频，而后续的封面背景图上传要限定在背景图目录。两者规则相同、根不同，
//! 共用一个函数可确保规则不会漂移。

use std::path::{Component, Path, PathBuf};

/// 路径被拒绝的原因。
///
/// 越界的三种（`Absolute`/`Traversal`/`Escapes`）都该按「拒绝」处理，分开只为日志可读；
/// `RootUnavailable` 是另一回事——它表示服务自身的目录配置有问题，不是用户输入的错。
#[derive(Debug, PartialEq, Eq)]
pub enum PathRejection {
    /// 用户提供了绝对路径
    Absolute,
    /// 路径里含 `..` 等非普通片段
    Traversal,
    /// 解析后落在根目录之外（典型来源是指向外部的符号链接）
    Escapes,
    /// 根目录或目标所在的上级目录不可用（被删除、权限不足）
    RootUnavailable,
}

/// 把用户提供的相对路径解析为 `root` 内的绝对路径。
///
/// 分两道关：先按片段拒绝明显的穿越写法，再在 `canonicalize` 之后复查一次是否仍在
/// 根目录内——后者是必要的，因为符号链接由全是普通片段的路径构成，第一道关看不出问题。
///
/// **目标不存在不算错误**：读取场景由文件服务自己回 404，写入场景（上传落盘）目标
/// 本来就还不存在。若这里因「不存在」而拒绝，上传接口就没法复用本函数、只能另写一套
/// 规则，两处规则迟早漂移——那正是本函数要消除的问题。因此目标不存在时改为校验其
/// 上级目录，再把最后一段拼回去。
pub fn resolve_within(root: &Path, user_path: &str) -> Result<PathBuf, PathRejection> {
    let candidate = Path::new(user_path);

    if candidate.is_absolute() {
        return Err(PathRejection::Absolute);
    }

    // 只放行普通片段与 `.`；`..`、根、盘符前缀一律拒绝。
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(PathRejection::Traversal),
        }
    }

    let root = root
        .canonicalize()
        .map_err(|_| PathRejection::RootUnavailable)?;
    let joined = root.join(candidate);

    // 目标已存在：直接解析它，可一并识破指向外部的符号链接。
    if let Ok(resolved) = joined.canonicalize() {
        return if resolved.starts_with(&root) {
            Ok(resolved)
        } else {
            Err(PathRejection::Escapes)
        };
    }

    // 目标尚不存在：改为校验上级目录，最后一段按字面拼回。
    // 上级目录同样要解析，否则 `实为外部链接的子目录/新文件` 会溜过去。
    let (parent, file_name) = match (joined.parent(), joined.file_name()) {
        (Some(parent), Some(file_name)) => (parent, file_name.to_owned()),
        // 没有最后一段（例如空路径）——没有可写/可读的目标，按越界拒绝
        _ => return Err(PathRejection::Traversal),
    };

    let parent = parent
        .canonicalize()
        .map_err(|_| PathRejection::RootUnavailable)?;
    if !parent.starts_with(&root) {
        return Err(PathRejection::Escapes);
    }

    Ok(parent.join(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 造一个根目录，里面放一个文件；同时在根**之外**放一个文件，用于越界测试。
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("inside.log"), b"inside").unwrap();

        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, b"outside").unwrap();

        (tmp, root, outside)
    }

    #[test]
    fn accepts_plain_filename_inside_root() {
        let (_tmp, root, _) = fixture();
        let resolved = resolve_within(&root, "inside.log").unwrap();
        assert!(resolved.starts_with(root.canonicalize().unwrap()));
        assert_eq!(fs::read(&resolved).unwrap(), b"inside");
    }

    #[test]
    fn accepts_nested_path_inside_root() {
        let (_tmp, root, _) = fixture();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/nested.mp4"), b"nested").unwrap();

        let resolved = resolve_within(&root, "sub/nested.mp4").unwrap();
        assert_eq!(fs::read(&resolved).unwrap(), b"nested");
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        let (_tmp, root, _) = fixture();
        assert_eq!(
            resolve_within(&root, "../outside.txt"),
            Err(PathRejection::Traversal)
        );
    }

    #[test]
    fn rejects_traversal_buried_mid_path() {
        let (_tmp, root, _) = fixture();
        assert_eq!(
            resolve_within(&root, "sub/../../outside.txt"),
            Err(PathRejection::Traversal)
        );
    }

    #[test]
    fn rejects_absolute_path() {
        let (_tmp, root, outside) = fixture();
        assert_eq!(
            resolve_within(&root, outside.to_str().unwrap()),
            Err(PathRejection::Absolute)
        );
    }

    // 符号链接全由普通片段构成，片段检查看不出问题，只能靠 canonicalize 后复查拦下。
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_root() {
        let (_tmp, root, outside) = fixture();
        std::os::unix::fs::symlink(&outside, root.join("escape.txt")).unwrap();

        assert_eq!(
            resolve_within(&root, "escape.txt"),
            Err(PathRejection::Escapes)
        );
    }

    // 指向根目录内部的符号链接是正常用法，不该被误伤。
    #[cfg(unix)]
    #[test]
    fn accepts_symlink_staying_inside_root() {
        let (_tmp, root, _) = fixture();
        std::os::unix::fs::symlink(root.join("inside.log"), root.join("alias.log")).unwrap();

        let resolved = resolve_within(&root, "alias.log").unwrap();
        assert_eq!(fs::read(&resolved).unwrap(), b"inside");
    }

    // 目标不存在时仍然放行——上传落盘的目标本来就还不存在，
    // 这是上传接口能复用本函数、两处规则不漂移的前提。
    #[test]
    fn accepts_not_yet_existing_target_inside_root() {
        let (_tmp, root, _) = fixture();
        let resolved = resolve_within(&root, "new-upload.jpg").unwrap();

        assert!(resolved.starts_with(root.canonicalize().unwrap()));
        assert!(!resolved.exists(), "该路径应当尚未存在");
        assert_eq!(resolved.file_name().unwrap(), "new-upload.jpg");
    }

    // 但「尚不存在」不等于放弃校验：上级目录仍须落在根内。
    #[test]
    fn rejects_not_yet_existing_target_outside_root() {
        let (_tmp, root, _) = fixture();
        assert_eq!(
            resolve_within(&root, "../new-upload.jpg"),
            Err(PathRejection::Traversal)
        );
    }

    // 上级目录是指向根外的符号链接时，即便目标尚不存在也必须拒绝，
    // 否则可以借由链接把文件写到根目录之外。
    #[cfg(unix)]
    #[test]
    fn rejects_not_yet_existing_target_under_escaping_symlink_dir() {
        let (tmp, root, _) = fixture();
        let outside_dir = tmp.path().join("outside_dir");
        fs::create_dir(&outside_dir).unwrap();
        std::os::unix::fs::symlink(&outside_dir, root.join("link")).unwrap();

        assert_eq!(
            resolve_within(&root, "link/new-upload.jpg"),
            Err(PathRejection::Escapes)
        );
    }

    #[test]
    fn reports_root_unavailable_when_root_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_root = tmp.path().join("does-not-exist");

        assert_eq!(
            resolve_within(&missing_root, "a.log"),
            Err(PathRejection::RootUnavailable)
        );
    }

    // 同一个函数配不同的根：各自只放行自己根目录内的路径。
    // 这是「静态文件用工作目录、背景图上传用图片目录」能共用一套规则的前提。
    #[test]
    fn same_function_isolates_different_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        let images = tmp.path().join("images");
        fs::create_dir(&logs).unwrap();
        fs::create_dir(&images).unwrap();
        fs::write(logs.join("a.log"), b"log").unwrap();
        fs::write(images.join("b.jpg"), b"jpg").unwrap();

        assert!(resolve_within(&logs, "a.log").is_ok());
        assert!(resolve_within(&images, "b.jpg").is_ok());

        // 各自解析出的路径都落在自己的根里，够不到对方的目录
        let from_logs = resolve_within(&logs, "b.jpg").unwrap();
        assert!(from_logs.starts_with(logs.canonicalize().unwrap()));
        assert!(!from_logs.exists(), "logs 根下并没有这个文件");

        // 更不能靠穿越互相够到
        assert_eq!(
            resolve_within(&logs, "../images/b.jpg"),
            Err(PathRejection::Traversal)
        );
    }
}
