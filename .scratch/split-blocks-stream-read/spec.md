# 分段处理阻塞拉流，FLV 定时切分后连接被 CDN 断开

来源：[dplei/biliup#79](https://github.com/dplei/biliup/issues/79)。发起时版本 1.3.38。

> 本目录用 `steps/` 存放实施步骤。**这些不是 GitHub issue**，编号只在本目录内有意义。

## 一句话

`DownloadTask::download` 在 `select!` 分支体里 await `processor.process()`，这期间 `download`
future 没人 poll；`process()` 又同步执行 `FileValidator::validate`（完整段约 20s）和短段合并
（ffmpeg），于是每次定时切分后 socket 约 20s 无人读取，抖音 CDN 判定客户端过慢，主动断开连接。

## 证据

生产日志只读核对，三次 FLV 定时切分全部符合同一条日志链：`splitting` → 约 20s 后上一段
`validated and enrolled`、同一秒 `httpflv connection closed outcome="stream_ended" silent_ms=0
since_last_split_ms≈20000` → 新段 `media_duration_ms` 只有 2–4s。

同一场改走 HLS 之后，切分不再出问题：HLS 按分片拉取，客户端卡住也不会被断开。

## 方案

- 录制循环改为 `tokio::join!(downloading, processing)`；拉流结束后调用 `segment_rx.close()`，
  `async_channel` 关闭后仍能取出剩余消息，处理端取完再退出。原来的收尾 `try_recv` 循环一并删除。
- `validate` 与 `merge_compatible_segments` 放进 `spawn_blocking`。只做 join 不够：同步调用会
  卡住整个 task，拉流也跟着停。

分段处理仍然在单个 future 里串行执行，顺序语义（短段 flush 先于下一个 Valid 入队）不变。

## 不在本题

同一场录制中，一次上传明显变慢期间拉流也连续断了 4 次（每条连接首字节延迟 8–11s）。
更像当时服务器网络整体劣化，与本题无关，已记在 #79 正文待观察。
