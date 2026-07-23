-- 自动封面：主播新增「封面背景图」字段，覆盖所属上传模板的同名设置
-- 与模板级同语义：存文件名而非路径，NULL/空白 = 未配置（回退到模板，再回退纯黑）
ALTER TABLE livestreamers ADD COLUMN cover_background VARCHAR;
