-- 自动封面：上传模板新增「封面文字模板」字段
-- 留空=维持原有 cover_path 行为；填写=生成黑底封面，优先于 cover_path
ALTER TABLE uploadstreamers ADD COLUMN cover_template VARCHAR;
