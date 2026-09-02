-- UPOS 取回描述符：上传成功后仍能把源对象下载回来所需的 endpoint + upos_uri + auth。
--
-- 为什么必须落库：`auth` 只能在 preupload 的响应里拿到，事后重新 preupload 得到的新 auth
-- 访问旧对象是 403，投稿账号的 Cookie 也是 403（实测见 dplei/biliup#13）。上传完把它丢掉
-- 就等于永久失去灾后取回通道——站内异步转码报错时，本地源文件早已按钩子删除。
--
-- 明文存放：同一个 data/ 目录里已经放着登录 cookie，而 cookie 的权限远大于「下载自己
-- 某一个对象」的短期令牌，只加密这一列是安全剧场。代价用 TTL 控制：写新描述符时顺带把
-- 过期的清成 NULL。**这一列的内容不得进入日志、事件或告警。**
alter table upload_missing_segment add column upos_recovery_json TEXT;
alter table upload_missing_segment add column upos_recovery_at DATETIME;
