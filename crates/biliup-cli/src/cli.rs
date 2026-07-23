use biliup::uploader::bilibili::{Studio, Vid};
use biliup::uploader::util::SubmitOption;
use clap::{Parser, Subcommand};

use crate::UploadLine;
use std::path::PathBuf;

/// 扩展路径中的 ~ 为用户主目录
pub fn expand_path(path: PathBuf) -> PathBuf {
    if let Some(path_str) = path.to_str() {
        let expanded = shellexpand::tilde(path_str);
        return PathBuf::from(expanded.as_ref());
    }
    path
}

#[derive(Parser)]
#[command(author, version, about)]
pub struct Cli {
    // /// Turn debugging information on
    // #[clap(short, long, parse(from_occurrences))]
    // debug: usize,
    #[clap(subcommand)]
    pub command: Commands,

    /// 配置代理
    #[arg(short, long, default_value = None)]
    pub proxy: Option<String>,

    /// 登录信息文件
    #[arg(short, long, default_value = "cookies.json")]
    pub user_cookie: PathBuf,

    // #[arg(long, default_value = "sqlx=debug,tower_http=debug,info")]
    #[arg(long, default_value = "tower_http=debug,info")]
    pub rust_log: String,
}

#[derive(Subcommand)]
pub enum Commands {
    /// 登录B站并保存登录信息
    Login,
    /// 手动验证并刷新登录信息
    Renew,
    /// 上传视频
    Upload {
        /// 提交接口
        #[arg(long)]
        submit: Option<SubmitOption>,

        // Optional name to operate on
        // name: Option<String>,
        /// 需要上传的视频路径,若指定配置文件投稿不需要此参数
        #[arg()]
        video_path: Vec<PathBuf>,

        /// Sets a custom config file
        #[arg(short, long, value_name = "FILE")]
        config: Option<PathBuf>,

        /// 选择上传线路
        #[arg(short, long, value_enum)]
        line: Option<UploadLine>,

        /// 单视频文件最大并发数
        #[arg(long, default_value = "3")]
        limit: usize,

        #[command(flatten)]
        studio: Studio,
        // #[arg(required = false, last = true, default_value = "client")]
        // submit: Option<String>,
    },
    /// 是否要对某稿件追加视频
    Append {
        /// 提交接口
        #[arg(long)]
        submit: Option<SubmitOption>,

        // Optional name to operate on
        // name: Option<String>,
        /// vid为稿件 av 或 bv 号
        #[arg(short, long)]
        vid: Vid,
        /// 需要上传的视频路径,若指定配置文件投稿不需要此参数
        #[arg()]
        video_path: Vec<PathBuf>,

        /// 选择上传线路
        #[arg(short, long, value_enum)]
        line: Option<UploadLine>,

        /// 单视频文件最大并发数
        #[arg(long, default_value = "3")]
        limit: usize,

        #[command(flatten)]
        studio: Studio,
    },
    /// 打印视频详情
    Show {
        /// vid为稿件 av 或 bv 号
        // #[clap()]
        vid: Vid,
    },
    /// 查看视频评论
    Comments {
        /// vid为稿件 av 或 bv 号
        vid: Vid,

        /// 排序方式，0为按时间，2为按热度
        #[arg(long, default_value = "0")]
        sort: u8,

        /// 页码
        #[arg(long, default_value = "1")]
        pn: u32,

        /// 每页条数
        #[arg(long, default_value = "20")]
        ps: u32,
    },
    /// 回复视频评论，默认只打印将要回复的内容
    Reply {
        /// vid为稿件 av 或 bv 号
        vid: Vid,

        /// 评论 rpid
        rpid: u64,

        /// 回复内容
        message: String,

        /// 实际发送回复
        #[arg(long)]
        execute: bool,
    },
    /// 输出flv元数据
    DumpFlv {
        #[arg()]
        file_name: PathBuf,
    },
    /// 下载视频
    Download {
        url: String,

        /// Output filename template. e.p. "./video/%Y-%m-%dT%H_%M_%S{title}"
        #[arg(short, long, default_value = "{title}")]
        output: String,

        /// 按照大小分割视频
        #[arg(long, value_parser = human_size)]
        split_size: Option<u64>,

        /// 按照时间分割视频
        #[arg(long)]
        split_time: Option<humantime::Duration>,
    },
    /// 启动web服务，默认端口19159
    Server {
        /// Specify bind address
        #[arg(short, long, default_value = "0.0.0.0")]
        bind: String,

        /// Port to use
        #[arg(short, long, default_value = "19159")]
        port: u16,

        /// 开启登录密码认证
        #[arg(long, default_value = "false")]
        auth: bool,

        /// 使用 biliup 1.0.7 风格配置文件启动录制
        #[arg(short, long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// 本地渲染一张封面看效果，用于挑背景图与试参数
    ///
    /// 复用线上同一套渲染逻辑，因此本地调好的效果就是实际投稿的效果。
    /// --dim 与 --blur 只作用于这里的输出：把满意的效果烘焙进成品图后再上传，
    /// 服务端不保存也不认识这两个参数。
    CoverPreview {
        /// 封面文字，用 \n 分行（与网页「封面文字模板」同样的写法）
        #[arg(short, long)]
        text: String,

        /// 背景图路径，省略则为纯黑底
        #[arg(short, long)]
        background: Option<PathBuf>,

        /// 输出的 JPG 路径
        #[arg(short, long, default_value = "cover-preview.jpg")]
        output: PathBuf,

        /// 背景压暗百分比 0-100，越大越暗，便于白字浮出来
        #[arg(long, default_value = "0", value_parser = clap::value_parser!(u8).range(0..=100))]
        dim: u8,

        /// 背景高斯模糊半径，0 为不模糊；数值越大越糊、也越慢
        #[arg(long, default_value = "0", value_parser = non_negative_blur)]
        blur: f32,

        /// 只输出处理好的背景图、不画文字——调满意后用它导出可直接上传的成品背景
        #[arg(long)]
        background_only: bool,
    },
    /// 列出所有已上传的视频
    List {
        /// 只包含进行中的视频
        #[arg(long)]
        is_pubing: bool,

        /// 只包含已通过的视频
        #[arg(long)]
        pubed: bool,

        /// 只包含未通过的视频
        #[arg(long)]
        not_pubed: bool,

        /// 从第几页开始获取
        #[arg(short, long, default_value = "1")]
        from_page: u32,

        /// 最大获取页数
        #[arg(short, long)]
        max_pages: Option<u32>,
    },
}

/// 模糊半径必须是非负的有限数。放任负值或 NaN 会被静默忽略，
/// 用户以为调了参数、实际什么也没发生。
fn non_negative_blur(s: &str) -> Result<f32, String> {
    match s.parse::<f32>() {
        Ok(v) if v.is_finite() && v >= 0.0 => Ok(v),
        Ok(_) => Err(format!("模糊半径需为非负有限数，收到 {s}")),
        Err(e) => Err(format!("{s} 不是合法数字: {e}")),
    }
}

fn human_size(s: &str) -> Result<u64, String> {
    let ret = match s.as_bytes() {
        [init @ .., b'K'] => parse_u8(init)? * 1000.0,
        [init @ .., b'M'] => parse_u8(init)? * 1000.0 * 1000.0,
        [init @ .., b'G'] => parse_u8(init)? * 1000.0 * 1000.0 * 1000.0,
        init => parse_u8(init)?,
    };
    Ok(ret as u64)
}

fn parse_u8(string: &[u8]) -> Result<f64, String> {
    let string = String::from_utf8_lossy(string);
    string
        .parse()
        .map_err(|e| format!("{string} is not ascii digit. {:?}", e))
}
