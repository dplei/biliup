-- 自动封面：上传模板新增「封面背景图」字段
-- 存文件名而非绝对路径，运行时拼到 data/cover-backgrounds/ 下——更换挂载点或迁移部署时数据无需修改
-- 留空/NULL = 维持纯黑底，与升级前行为一致
ALTER TABLE uploadstreamers ADD COLUMN cover_background VARCHAR;
