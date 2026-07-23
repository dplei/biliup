use ormlite::{Insert, Model};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 上传配置模型
/// 存储视频上传到B站时的各种配置参数
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[ormlite(table = "uploadstreamers")]
pub struct UploadStreamer {
    /// 主键ID
    pub id: i64,
    /// 模板名称
    pub template_name: String,
    /// 视频标题
    pub title: Option<String>,
    /// 分区ID
    pub tid: Option<u16>,
    /// 版权类型（1-自制，2-转载）
    pub copyright: Option<u8>,
    /// 转载来源
    pub copyright_source: Option<String>,
    /// 封面路径
    pub cover_path: Option<String>,
    /// 封面文字模板（留空=用 cover_path；填写=生成黑底封面，优先）
    pub cover_template: Option<String>,
    /// 封面背景图文件名（留空=纯黑底）。存文件名不存路径，实际路径运行时拼接。
    ///
    /// 刻意不在 `InsertUploadStreamer` 里：前端尚未认识这个字段，写入侧一旦有它，
    /// 编辑模板提交的 JSON 缺项就会把配好的背景清成 NULL。
    pub cover_background: Option<String>,
    /// 视频简介
    pub description: Option<String>,
    /// 动态内容
    pub dynamic: Option<String>,
    /// 定时发布时间
    pub dtime: Option<u32>,
    /// 杜比音效
    pub dolby: Option<u8>,
    /// Hi-Res音质
    pub hires: Option<u8>,
    /// 充电专属
    pub charging_pay: Option<u8>,
    /// 禁止转载
    pub no_reprint: Option<u8>,
    /// 仅自己可见
    pub is_only_self: Option<u8>,
    /// 上传者
    pub uploader: Option<String>,
    /// 用户Cookie
    pub user_cookie: Option<String>,
    /// 标签列表（JSON格式）
    #[ormlite(json)]
    pub tags: Vec<String>, // not null
    /// 制作人员信息
    pub credits: Option<Value>,
    /// 开启精选评论
    pub up_selection_reply: Option<bool>,
    /// 关闭评论
    pub up_close_reply: Option<bool>,
    /// 关闭弹幕
    pub up_close_danmu: Option<bool>,
    /// 额外字段
    pub extra_fields: Option<String>,
}

/// 插入上传配置的数据结构
/// 用于创建新的上传配置记录
#[derive(Model, Insert, Debug, Clone, Serialize, Deserialize)]
#[ormlite(table = "uploadstreamers", returns = "UploadStreamer")]
pub struct InsertUploadStreamer {
    pub id: Option<i64>,
    pub template_name: String,
    pub title: Option<String>,
    pub tid: Option<u16>,
    pub copyright: Option<u8>,
    pub copyright_source: Option<String>,
    pub cover_path: Option<String>,
    pub cover_template: Option<String>,
    pub description: Option<String>,
    pub dynamic: Option<String>,
    pub dtime: Option<u32>,
    pub dolby: Option<u8>,
    pub hires: Option<u8>,
    pub charging_pay: Option<u8>,
    pub no_reprint: Option<u8>,
    pub uploader: Option<String>,
    pub user_cookie: Option<String>,
    #[ormlite(json)]
    pub tags: Vec<String>, // not null
    pub credits: Option<Value>,
    pub up_selection_reply: Option<u8>,
    pub up_close_reply: Option<u8>,
    pub up_close_danmu: Option<u8>,
    pub extra_fields: Option<String>,
    pub is_only_self: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::infrastructure::connection_pool::test_support::migrated_pool;

    /// 迁移建出来的列名、类型要与模型对得上，否则线上一读就炸。
    /// 这条是迁移本身唯一的验证——它跑通才说明 7_add_cover_background.sql 真的生效了。
    #[tokio::test]
    async fn cover_background_round_trips_through_database() {
        let (_dir, pool) = migrated_pool().await;

        sqlx::query(
            "INSERT INTO uploadstreamers (template_name, tags, cover_template, cover_background)
             VALUES (?1, '[]', ?2, ?3)",
        )
        .bind("模板A")
        .bind("{streamer}\\n%Y-%m-%d")
        .bind("aurora.jpg")
        .execute(&pool)
        .await
        .unwrap();

        let row = UploadStreamer::select()
            .where_("template_name = ?")
            .bind("模板A")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(row.cover_background.as_deref(), Some("aurora.jpg"));
    }

    /// 升级路径：既有模板不会因为多了一列而读不出来，未配置即 NULL。
    #[tokio::test]
    async fn existing_template_without_background_reads_as_none() {
        let (_dir, pool) = migrated_pool().await;

        sqlx::query("INSERT INTO uploadstreamers (template_name, tags) VALUES (?1, '[]')")
            .bind("模板B")
            .execute(&pool)
            .await
            .unwrap();

        let row = UploadStreamer::select()
            .where_("template_name = ?")
            .bind("模板B")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(row.cover_background, None);
    }
}
