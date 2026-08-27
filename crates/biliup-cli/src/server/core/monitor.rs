use crate::server::common::cookie_health;
use crate::server::common::download::start_download_workflow;
use crate::server::common::recording_lease;
use crate::server::common::upload::UploaderMessage;
use crate::server::common::upload_session::reusable_streamer_info;
use crate::server::core::live::{live_request, streamer_info};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::context::{Context, Stage, Worker, WorkerStatus};
use crate::server::infrastructure::models::StreamerInfo;
use async_channel::Sender;
use biliup::downloader::live::{LivePlugin, LiveStatus};
use ormlite::Model;
use ormlite::model::ModelBuilder;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace, warn};

/// 一次开播检查的结论。轮询循环只用它决定要不要等待间隔，主动检查接口把它翻译成人话。
#[derive(Debug, Clone)]
pub enum CheckOutcome {
    /// 已开播，录制流程已拉起。
    Started,
    /// 平台返回未开播。
    Offline,
    /// 未绑定投稿模板，按既定策略不录。
    NoUploadTemplate,
    /// 下载池已满，本次不检查。
    DownloadPoolFull,
    /// 录制租约拒绝了这一场。
    LeaseRejected,
    /// 已开播但登记录制会话失败。
    StartFailed,
    /// 检查直播间本身出错（已脱敏）。
    CheckFailed(String),
}

/// 主动检查的结果：要么真检查了一次，要么说明为什么没检查。
#[derive(Debug, Clone)]
pub enum ManualCheckResult {
    /// 完成了一次检查。
    Checked(CheckOutcome),
    /// 房间不在监控中（刚被删除，或 URL 没有匹配的平台插件）。
    NotFound,
    /// 已经在录制，不需要再连一次。
    Recording,
    /// 已暂停录制，主动检查不越过人工暂停。
    Paused,
    /// 轮询循环正在检查这个房间，让它出结果就好。
    Busy,
}

/// 摘队列的结果。只在 Actor 内部产生，保证「判断状态 + 摘走房间」是一步原子操作。
enum ManualCheckTake {
    Ready(Arc<Worker>, Arc<dyn LivePlugin + Send + Sync>),
    NotFound,
    Recording,
    Paused,
    Busy,
}

/// 房间处理器
/// 管理多个直播间的状态和操作
#[derive(Debug)]
pub struct Monitor {
    /// 消息发送器
    sender: tokio::sync::mpsc::Sender<ActorMessage>,
    /// Actor任务句柄
    pool: ConnectionPool,
    /// 上传消息发送器，下载任务产生分段后会通过它交给上传流程。
    uploader: Sender<UploaderMessage>,
    /// 下载池许可。监控循环必须先拿到许可，才允许检测开播并启动录制。
    /// 这样 “开播了/成功开始录制” 只会出现在真正拥有下载并发槽位时。
    /// 许可由下载任务持有到录制结束，pool1_size 的唯一限流语义在这里表达。
    download_slots: Arc<Semaphore>,
    monitors: RwLock<HashMap<String, JoinHandle<()>>>,
}

impl Drop for Monitor {
    /// 监控器销毁时的清理逻辑
    fn drop(&mut self) {
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let msg = ActorMessage::Shutdown;
            let _ = sender.send(msg).await;
            info!("RoomsHandle killed")
        });
        // 终止监控任务
        // self.kill.abort();
        // self.rooms_handle.kill.abort();
    }
}

impl Monitor {
    /// 创建新的房间处理器实例
    ///
    /// # 参数
    /// * `name` - 平台名称
    pub fn new(
        uploader: Sender<UploaderMessage>,
        download_slots: Arc<Semaphore>,
        pool: ConnectionPool,
    ) -> Self {
        // 创建消息通道
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let mut actor = RoomsActor::new(receiver);
        // 启动Actor任务
        let _kill = tokio::spawn(async move { actor.run().await });

        Self {
            sender,
            pool,
            uploader,
            download_slots,
            monitors: Default::default(),
        }
    }

    /// 启动客户端监控循环
    ///
    /// # 参数
    /// * `platform_name` - 平台名称
    /// * `plugin` - 下载插件
    pub(crate) async fn start_monitor(
        self: &Arc<Self>,
        platform_name: &str,
        plugin: Arc<dyn LivePlugin + Send + Sync>,
    ) {
        info!("start -> [{platform_name}]");
        // 获取下一个要检查的房间
        while let Some(room) = self.next(platform_name).await {
            let interval = room.get_config().event_loop_interval;
            // 租约拒绝和建会话失败这两条路径上房间已经离开队列，历史行为是立刻轮到下一个
            // 房间、不消耗本轮检查间隔，这里保持不变。
            if matches!(
                self.check_room_once(&room, &plugin).await,
                CheckOutcome::LeaseRejected | CheckOutcome::StartFailed
            ) {
                continue;
            }
            // 等待下一次检查
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
        info!("exit -> [{platform_name}]")
    }

    /// 对单个房间做一次开播检查，命中开播就地拉起录制。
    ///
    /// 轮询循环和「主动检查」接口共用这一份实现：会话复用、租约准入、下载许可这些判断一旦
    /// 分裂成两份，两条路径迟早会对同一场直播给出不同结论。
    ///
    /// 调用前提是房间已经不在轮询队列里（轮询用 `next` 弹出，主动检查用 `take_for_check`
    /// 摘走）。没能拉起录制的分支都会自己 `wake_waker` 放回队列。
    pub(crate) async fn check_room_once(
        self: &Arc<Self>,
        room: &Arc<Worker>,
        plugin: &Arc<dyn LivePlugin + Send + Sync>,
    ) -> CheckOutcome {
        let platform_name = plugin.name();
        // 更新状态为等待中
        room.change_status(Stage::Download, WorkerStatus::Pending)
            .await;
        let url = room.get_streamer().url.clone();
        let webhook = room.get_config().cookie_health_webhook.clone();
        // 未绑定投稿模板：不录制（录了传不上、还白占磁盘）。前端据 /v1/streamers 返回的
        // upload_streamers_id==null 显示「缺少投稿」标签。绑定后 worker 会重建并恢复录制。
        if room.get_upload_config().is_none() {
            room.change_status(Stage::Download, WorkerStatus::Idle)
                .await;
            debug!(url = url, "未绑定投稿模板，跳过录制（缺少投稿）");
            self.wake_waker(room.id()).await;
            return CheckOutcome::NoUploadTemplate;
        }
        let Some(download_permit) = self.try_acquire_download_slot(room).await else {
            self.wake_waker(room.id()).await;
            return CheckOutcome::DownloadPoolFull;
        };
        let request = live_request(room);
        // 检查直播状态
        match plugin.check_stream(request).await {
            Ok(LiveStatus::Live { stream }) => {
                // 检查成功（cookie 工作正常）
                cookie_health::record_success(platform_name, webhook.as_deref());
                let sql_no_id = streamer_info(&stream);
                // 同一场直播不该有两个身份。此前每检测到一次开播就无条件插入一行
                // streamer_info，于是「录制中重启」必然换掉 ctx.id()，会话续接的两条路
                // 同时失效，一场直播被拆成两个会话、两个稿件。
                let reused = match reusable_streamer_info(
                    &self.pool,
                    room.live_streamer.id,
                    &sql_no_id.url,
                    sql_no_id.live_session_key.as_deref(),
                )
                .await
                {
                    Ok(reused) => reused,
                    Err(e) => {
                        // 查不到就当没有：宁可多建一行（退回今天的行为），也不能因为一次
                        // 读库失败就拒绝录制。
                        error!(e=?e, "查询同场 streamer_info 失败，按新开一场处理");
                        None
                    }
                };
                let is_reused = reused.is_some();
                if !recording_lease::admit_detected_session(
                    &self.pool,
                    room,
                    reused.as_ref().map(|row| row.id),
                    sql_no_id.live_session_key.as_deref(),
                    sql_no_id.date,
                    is_reused,
                    chrono::Utc::now(),
                )
                .await
                .unwrap_or_else(|error| {
                    error!(error = ?error, live_streamer_id = room.id(), "录制租约准入检查失败，保守拒绝本场");
                    false
                })
                {
                    return CheckOutcome::LeaseRejected;
                }
                let insert = match reused {
                    Some(existing) => {
                        info!(
                            url = url,
                            streamer_info = existing.id,
                            live_session_key = ?sql_no_id.live_session_key,
                            "room: is live -> 续接同一场直播（复用 streamer_info）"
                        );
                        existing
                    }
                    None => match StreamerInfo::builder()
                        .url(sql_no_id.url.clone())
                        .name(room.live_streamer.remark.clone())
                        .title(sql_no_id.title.clone())
                        .date(sql_no_id.date)
                        .live_cover_path(sql_no_id.live_cover_path.clone())
                        .live_session_key(sql_no_id.live_session_key.clone())
                        .insert(&self.pool)
                        .await
                    {
                        Ok(insert) => insert,
                        Err(e) => {
                            error!(e=?e, "插入数据库失败");
                            self.wake_waker(room.id()).await;
                            return CheckOutcome::StartFailed;
                        }
                    },
                };
                info!(url = url, "room: is live -> 开播了");

                let context = Context::new(
                    insert.id,
                    room.clone(),
                    self.pool.clone(),
                    *stream,
                    is_reused,
                );
                let downloader = Arc::clone(plugin);
                let uploader = self.uploader.clone();
                let rooms_handle = Arc::clone(self);

                // 只能在已经拿到下载池许可后启动录制。许可移动到任务内并持有到流程结束，
                // 因此 pool1_size 只在这里表达，不再通过下载 Actor 池或消息队列重复限流。
                tokio::spawn(async move {
                    let _download_permit = download_permit;
                    start_download_workflow(downloader, context, uploader, rooms_handle).await;
                });

                info!("成功开始录制 {}", url);
                CheckOutcome::Started
            }
            Ok(LiveStatus::Offline) => {
                // 未开播也是一次成功的检查（cookie 正常，只是主播没播）
                cookie_health::record_success(platform_name, webhook.as_deref());
                self.wake_waker(room.id()).await;
                debug!(url = url, "未开播");
                CheckOutcome::Offline
            }
            Err(e) => {
                // 健康模块会区分鉴权失败与普通传输/服务端错误。
                let sanitized_error = cookie_health::redact_sensitive(&format!("{e:?}"));
                cookie_health::record_error(platform_name, &sanitized_error, webhook.as_deref());
                self.wake_waker(room.id()).await;
                error!(error = sanitized_error, ctx = url, "检查直播间出错");
                CheckOutcome::CheckFailed(sanitized_error)
            }
        }
    }

    /// 立刻检查一个直播间，不等轮询轮到它。
    ///
    /// 服务重启后轮询要绕完一整圈才轮到某个房间，已经在播的场次就白等；这里把那一次检查
    /// 提前。摘队列这一步在 Actor 里完成，所以不会和轮询循环同时检查同一个房间。
    pub async fn check_now(self: &Arc<Self>, id: i64) -> ManualCheckResult {
        match self.take_for_check(id).await {
            ManualCheckTake::Ready(room, plugin) => {
                info!(
                    live_streamer_id = id,
                    url = room.get_streamer().url,
                    "主动检查直播流"
                );
                ManualCheckResult::Checked(self.check_room_once(&room, &plugin).await)
            }
            ManualCheckTake::NotFound => ManualCheckResult::NotFound,
            ManualCheckTake::Recording => ManualCheckResult::Recording,
            ManualCheckTake::Paused => ManualCheckResult::Paused,
            ManualCheckTake::Busy => ManualCheckResult::Busy,
        }
    }

    /// 从轮询队列里摘走房间，拿到它的独占检查权。
    async fn take_for_check(self: &Arc<Self>, id: i64) -> ManualCheckTake {
        let (send, recv) = oneshot::channel();
        let _ = self
            .sender
            .send(ActorMessage::TakeForCheck {
                respond_to: send,
                id,
            })
            .await;
        recv.await.expect("Actor task has been killed")
    }

    async fn try_acquire_download_slot(&self, room: &Arc<Worker>) -> Option<OwnedSemaphorePermit> {
        match self.download_slots.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                debug!(
                    url = room.get_streamer().url,
                    "download pool is full, skip live check"
                );
                None
            }
        }
    }

    /// 添加工作器到房间列表
    ///
    /// # 参数
    /// * `worker` - 要添加的工作器
    pub async fn add(
        self: &Arc<Self>,
        worker: Arc<Worker>,
    ) -> Option<Arc<dyn LivePlugin + Send + Sync>> {
        let (send, recv) = oneshot::channel();
        let msg = ActorMessage::Add(send, worker.clone());
        let _ = self.sender.send(msg).await;
        let plugin = recv.await.expect("Actor task has been killed")?;

        self.rooms_handle_pool(plugin.clone());
        Some(plugin)
    }

    /// 添加工作器到房间列表
    ///
    /// # 参数
    /// * `worker` - 要添加的工作器
    pub async fn add_plugin(&self, plugin: Arc<dyn LivePlugin + Send + Sync>) {
        let (send, recv) = oneshot::channel();
        let msg = ActorMessage::AddPlugin(send, plugin);
        let _ = self.sender.send(msg).await;
        recv.await.expect("Actor task has been killed")
    }

    /// 删除指定ID的工作器
    ///
    /// # 参数
    /// * `id` - 要删除的工作器ID
    ///
    /// # 返回
    /// 返回剩余工作器数量
    pub async fn del(&self, id: i64) {
        let (send, recv) = oneshot::channel();
        let msg = ActorMessage::Del {
            respond_to: send,
            id,
        };

        // 忽略发送错误。如果发送失败，下面的recv.await也会失败
        // 没有必要检查两次失败
        let _ = self.sender.send(msg).await;
        if let Some(worker) = recv.await.expect("Actor task has been killed") {
            worker
                .change_status(Stage::Download, WorkerStatus::Idle)
                .await;
        }
    }

    /// 删除指定ID的工作器
    ///
    /// # 参数
    /// * `id` - 要删除的工作器ID
    ///
    /// # 返回
    /// 返回剩余工作器数量
    pub async fn get_worker(&self, id: i64) -> Option<Arc<Worker>> {
        let (send, recv) = oneshot::channel();
        let msg = ActorMessage::GetWorker {
            respond_to: send,
            id,
        };

        // 忽略发送错误。如果发送失败，下面的recv.await也会失败
        // 没有必要检查两次失败
        let _ = self.sender.send(msg).await;
        recv.await.expect("Actor task has been killed")
    }

    /// 删除指定ID的工作器
    ///
    /// # 参数
    /// * `id` - 要删除的工作器ID
    ///
    /// # 返回
    /// 返回剩余工作器数量
    pub async fn get_all(&self) -> Vec<Arc<Worker>> {
        let (send, recv) = oneshot::channel();
        let msg = ActorMessage::GetAll { respond_to: send };

        // 忽略发送错误。如果发送失败，下面的recv.await也会失败
        // 没有必要检查两次失败
        let _ = self.sender.send(msg).await;
        recv.await.expect("Actor task has been killed")
    }

    /// 获取下一个要处理的工作器
    ///
    /// # 返回
    /// 返回下一个工作器，如果没有则返回None
    async fn next(self: &Arc<Self>, platform_name: &str) -> Option<Arc<Worker>> {
        let (send, recv) = oneshot::channel();
        let msg = ActorMessage::NextRoom {
            respond_to: send,
            platform_name: platform_name.to_owned(),
        };

        // 忽略发送错误。如果发送失败，下面的recv.await也会失败
        // 没有必要检查两次失败
        let _ = self.sender.send(msg).await;
        recv.await.expect("Actor task has been killed")
    }

    /// 放回工作队列
    ///
    /// # 参数
    /// * `worker` - 要切换的工作器
    pub async fn wake_waker(
        self: &Arc<Self>,
        id: i64,
    ) -> Option<Arc<dyn LivePlugin + Send + Sync>> {
        let (send, recv) = oneshot::channel();

        let msg = ActorMessage::WakeWaker(send, id);

        // 忽略发送错误
        let _ = self.sender.send(msg).await;
        let plugin = recv.await.expect("Actor task has been killed")?;
        self.rooms_handle_pool(plugin.clone());
        Some(plugin)
    }

    /// 移出工作队列
    ///
    /// # 参数
    /// * `worker` - 要切换的工作器
    pub async fn make_waker(&self, id: i64) {
        let (send, recv) = oneshot::channel();

        let msg = ActorMessage::MakeWaker(send, id);

        // 忽略发送错误
        let _ = self.sender.send(msg).await;
        recv.await.expect("Actor task has been killed")
    }

    fn spawn_monitor_task(
        this: Arc<Self>,
        plugin: Arc<dyn LivePlugin + Send + Sync>,
        platform_name: String,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            this.start_monitor(&platform_name, plugin).await;
        })
    }

    fn rooms_handle_pool(self: &Arc<Self>, plugin: Arc<dyn LivePlugin + Send + Sync>) {
        let platform_name = plugin.name().to_owned();
        match self.monitors.write().unwrap().entry(platform_name.clone()) {
            Entry::Occupied(mut entry) => {
                // 已经有一个任务了，检查是否结束
                if entry.get().is_finished() {
                    // 旧任务已经结束，重新 spawn 一个
                    let handle = Self::spawn_monitor_task(
                        Arc::clone(self),
                        plugin.clone(),
                        platform_name.clone(),
                    );
                    entry.insert(handle); // 替换旧的 JoinHandle
                } else {
                    // 任务还在跑，不做任何事
                }
            }
            Entry::Vacant(entry) => {
                // 没有任务，正常 spawn
                let handle = Self::spawn_monitor_task(
                    Arc::clone(self),
                    plugin.clone(),
                    platform_name.clone(),
                );
                entry.insert(handle);
            }
        }
    }
}

/// Actor消息枚举
/// 定义RoomsActor可以处理的消息类型
enum ActorMessage {
    /// 获取下一个房间
    NextRoom {
        respond_to: oneshot::Sender<Option<Arc<Worker>>>,
        platform_name: String,
    },
    /// 添加工作器
    Add(
        oneshot::Sender<Option<Arc<dyn LivePlugin + Send + Sync>>>,
        Arc<Worker>,
    ),
    /// 添加工作器
    AddPlugin(oneshot::Sender<()>, Arc<dyn LivePlugin + Send + Sync>),
    /// 删除工作器
    Del {
        respond_to: oneshot::Sender<Option<Arc<Worker>>>,
        id: i64,
    },
    /// 查找
    GetWorker {
        respond_to: oneshot::Sender<Option<Arc<Worker>>>,
        id: i64,
    },
    /// 查找所有
    GetAll {
        respond_to: oneshot::Sender<Vec<Arc<Worker>>>,
    },
    /// 放回工作队列
    WakeWaker(
        oneshot::Sender<Option<Arc<dyn LivePlugin + Send + Sync>>>,
        i64,
    ),
    /// 移出工作队列
    MakeWaker(oneshot::Sender<()>, i64),
    /// 摘走一个房间做主动检查
    TakeForCheck {
        respond_to: oneshot::Sender<ManualCheckTake>,
        id: i64,
    },
    Shutdown,
}

/// 房间Actor
/// 管理房间列表的内部Actor
/// 平台名称
//     name: String,
struct RoomsActor {
    /// 消息接收器
    receiver: tokio::sync::mpsc::Receiver<ActorMessage>,
    /// 活跃房间列表
    platforms: HashMap<String, VecDeque<Arc<Worker>>>,
    /// 当前索引
    /// 等待房间列表
    all_workers: Vec<Arc<Worker>>,
    // index: usize,
    // rooms: Vec<Arc<Worker>>,
    // waiting: Vec<Arc<Worker>>,
    /// 下载插件
    plugins: Vec<Arc<dyn LivePlugin + Send + Sync>>,
}

impl RoomsActor {
    /// 创建新的房间Actor实例
    fn new(receiver: tokio::sync::mpsc::Receiver<ActorMessage>) -> Self {
        Self {
            receiver,
            // index: 0,
            platforms: Default::default(),
            all_workers: Default::default(),
            plugins: Vec::new(),
        }
    }

    /// 运行Actor主循环
    /// 处理接收到的消息
    async fn run(&mut self) {
        while let Some(msg) = self.receiver.recv().await {
            match msg {
                ActorMessage::NextRoom {
                    respond_to,
                    platform_name,
                } => {
                    // `let _ =` 忽略发送时的任何错误
                    // 如果使用`select!`宏取消等待响应，可能会发生这种情况
                    let _ = respond_to.send(self.next(&platform_name));
                }
                ActorMessage::Add(respond_to, worker) => {
                    let plugin = self.add(worker);
                    let _ = respond_to.send(plugin);
                }
                ActorMessage::Del { respond_to, id } => {
                    // `let _ =` 忽略发送时的任何错误
                    // 如果使用`select!`宏取消等待响应，可能会发生这种情况

                    let _ = respond_to.send(self.del(id).await);
                }
                ActorMessage::WakeWaker(sender, id) => {
                    // `let _ =` 忽略发送时的任何错误
                    let _ = sender.send(self.push_back(id));
                }
                ActorMessage::Shutdown => {
                    return;
                }
                ActorMessage::GetWorker { respond_to, id } => {
                    let option = self.get_worker(id);
                    // `let _ =` 忽略发送时的任何错误
                    let _ = respond_to.send(option);
                }
                ActorMessage::GetAll { respond_to } => {
                    // `let _ =` 忽略发送时的任何错误
                    let _ = respond_to.send(self.get_all());
                }
                ActorMessage::MakeWaker(respond_to, id) => {
                    self.pop(id);
                    // `let _ =` 忽略发送时的任何错误
                    let _ = respond_to.send(());
                }
                ActorMessage::TakeForCheck { respond_to, id } => {
                    let take = self.take_for_check(id);
                    // `let _ =` 忽略发送时的任何错误
                    let _ = respond_to.send(take);
                }
                ActorMessage::AddPlugin(respond_to, plugin) => {
                    self.add_plugin(plugin);
                    // `let _ =` 忽略发送时的任何错误
                    let _ = respond_to.send(());
                }
            }
        }
        info!("Rooms actor terminated");
    }

    fn add(&mut self, worker: Arc<Worker>) -> Option<Arc<dyn LivePlugin + Send + Sync>> {
        let plugin = self.matches(&worker.live_streamer.url)?;
        let platform_name = plugin.name().to_owned();
        self.all_workers.push(worker.clone());

        match self.platforms.entry(platform_name) {
            Entry::Occupied(mut entry) => {
                if !matches!(
                    *worker.downloader_status.read().unwrap(),
                    WorkerStatus::Pause
                ) {
                    entry.get_mut().push_back(worker.clone());
                }
                // entry.remove(); // 可以删除
            }
            Entry::Vacant(entry) => {
                let queue = if matches!(
                    *worker.downloader_status.read().unwrap(),
                    WorkerStatus::Pause
                ) {
                    VecDeque::new()
                } else {
                    VecDeque::from([worker.clone()])
                };
                entry.insert(queue); // 插入新值
            }
        }
        debug!("Added room [{}]", worker.live_streamer.url);
        Some(plugin)
    }

    fn add_plugin(&mut self, plugin: Arc<dyn LivePlugin + Send + Sync>) {
        self.plugins.push(plugin);
        debug!("Added plugin size[{}]", self.plugins.len());
    }

    fn get_worker(&mut self, id: i64) -> Option<Arc<Worker>> {
        self.all_workers
            .iter()
            .find(|worker| worker.id() == id)
            .cloned()
    }

    fn get_all(&mut self) -> Vec<Arc<Worker>> {
        reuse_vec_arc(&mut self.all_workers.iter())
    }

    /// 获取下一个工作器（循环遍历）
    fn next(&mut self, platform_name: &str) -> Option<Arc<Worker>> {
        // 如果内部Vec是空的，迭代结束（虽然是循环迭代器，但空集合无法产生任何值）
        let arc = self.platforms.get_mut(platform_name)?.pop_front()?;

        *arc.downloader_status.write().unwrap() = WorkerStatus::Pending;

        Some(arc)
    }

    /// 放回工作队列
    fn push_back(&mut self, id: i64) -> Option<Arc<dyn LivePlugin + Send + Sync>> {
        // 在总数组中找不到，说明该房间已被移除我们也不放回
        let worker = self.get_worker(id)?;
        if let WorkerStatus::Pause = *worker.downloader_status.write().unwrap() {
            // 暂停状态则不放回
            warn!("Paused room [{}]", worker.live_streamer.url);
            return None;
        }
        for (name, queue) in self.platforms.iter_mut() {
            if queue.iter().any(|w| w.id() == id) {
                // 说明找到了已经入队的房间，则是更新的情况
                warn!(name = name, "房间已更新无需入队");
                return None;
            }
        }

        let plugin = self.matches(&worker.live_streamer.url)?;
        self.platforms
            .get_mut(plugin.name())?
            .push_back(worker.clone());
        *worker.downloader_status.write().unwrap() = WorkerStatus::Idle;
        Some(plugin)
    }

    /// 摘走房间，交给主动检查独占。
    ///
    /// 「不在任何队列里」等价于轮询循环此刻正拿着它检查（`next` 已经弹出、还没 `wake_waker`
    /// 放回），这时候直接返回 Busy，不去开第二次检查——那会让同一场直播被拉起两次录制。
    fn take_for_check(&mut self, id: i64) -> ManualCheckTake {
        let Some(worker) = self.get_worker(id) else {
            return ManualCheckTake::NotFound;
        };
        let status = worker.downloader_status.read().unwrap().clone();
        match status {
            WorkerStatus::Working(_) => return ManualCheckTake::Recording,
            WorkerStatus::Pause => return ManualCheckTake::Paused,
            _ => {}
        }
        let Some(plugin) = self.matches(&worker.live_streamer.url) else {
            return ManualCheckTake::NotFound;
        };
        let Some(queue) = self.platforms.get_mut(plugin.name()) else {
            return ManualCheckTake::Busy;
        };
        let Some(pos) = queue.iter().position(|w| w.id() == id) else {
            return ManualCheckTake::Busy;
        };
        queue.remove(pos);
        *worker.downloader_status.write().unwrap() = WorkerStatus::Pending;
        ManualCheckTake::Ready(worker, plugin)
    }

    /// 移出工作队列
    fn pop(&mut self, id: i64) {
        for (_name, queue) in self.platforms.iter_mut() {
            if let Some(pos) = queue.iter().position(|w| w.id() == id) {
                queue.remove(pos); // 只删掉这个队列中第一个匹配的 worker
                return;
            }
        }
        warn!("移出工作队列 failed: No room found with id {}", id);
    }

    /// 删除指定ID的工作器
    async fn del(&mut self, id: i64) -> Option<Arc<Worker>> {
        let worker = self.get_worker(id)?;
        let plugin = self.matches(&worker.live_streamer.url)?;
        let platform_name = plugin.name();
        // 从 platforms 中删除
        if let Some(workers) = self.platforms.get_mut(platform_name) {
            workers.retain(|w| w.id() != id);
        } else {
            error!("Removed room [{:?}] {}", platform_name, id);
        }

        // 从 all_workers 中删除
        self.all_workers.retain(|w| w.id() != id);

        debug!("del worker size[{}]", self.all_workers.len());
        Some(worker)
    }

    /// 检查URL是否匹配此下载管理器的插件
    ///
    /// # 参数
    /// * `url` - 要检查的URL
    ///
    /// # 返回
    /// 如果URL匹配返回true，否则返回false
    pub fn matches(&self, url: &str) -> Option<Arc<dyn LivePlugin + Send + Sync>> {
        for plugin in &self.plugins {
            trace!(
                platform_name = plugin.name(),
                url = url,
                "Found plugin for URL"
            );
            if plugin.matches(url) {
                return Some(plugin.clone());
            }
        }
        None
    }
}

fn reuse_vec_arc<'a, T: 'a, U: Iterator<Item = &'a Arc<T>>>(v: &mut U) -> Vec<Arc<T>> {
    v.into_iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::config::Config;
    use crate::server::infrastructure::models::live_streamer::LiveStreamer;
    use biliup::downloader::live::builtin_plugins;

    fn worker(id: i64) -> Arc<Worker> {
        Arc::new(Worker::new(
            LiveStreamer {
                id,
                url: format!("https://live.bilibili.com/{id}"),
                remark: format!("主播{id}"),
                filename_prefix: None,
                time_range: None,
                upload_streamers_id: None,
                format: None,
                override_cfg: None,
                preprocessor: None,
                segment_processor: None,
                downloaded_processor: None,
                postprocessor: None,
                opt_args: None,
                excluded_keywords: None,
                cover_background: None,
            },
            None,
            Arc::new(RwLock::new(Config::default())),
            biliup::client::StatelessClient::default(),
        ))
    }

    fn actor_with(room: &Arc<Worker>) -> RoomsActor {
        let (_send, recv) = tokio::sync::mpsc::channel(1);
        let mut actor = RoomsActor::new(recv);
        for plugin in builtin_plugins() {
            actor.add_plugin(plugin);
        }
        actor.add(room.clone());
        actor
    }

    /// 主动检查必须把房间摘出队列，且同一时刻只能有一个检查者：否则轮询和按钮会对同一场
    /// 直播各拉起一次录制。
    #[test]
    fn manual_check_takes_room_out_of_queue_exactly_once() {
        let room = worker(1);
        let mut actor = actor_with(&room);

        assert!(matches!(
            actor.take_for_check(1),
            ManualCheckTake::Ready(..)
        ));
        assert!(matches!(actor.take_for_check(1), ManualCheckTake::Busy));

        // 检查结束放回队列后，才允许下一次主动检查。
        actor.push_back(1);
        assert!(matches!(
            actor.take_for_check(1),
            ManualCheckTake::Ready(..)
        ));
    }

    /// 主动检查不越过人工暂停。
    #[test]
    fn manual_check_refuses_paused_room() {
        let room = worker(2);
        *room.downloader_status.write().unwrap() = WorkerStatus::Pause;
        let mut actor = actor_with(&room);

        assert!(matches!(actor.take_for_check(2), ManualCheckTake::Paused));
    }

    #[test]
    fn manual_check_reports_unknown_room() {
        let room = worker(3);
        let mut actor = actor_with(&room);

        assert!(matches!(
            actor.take_for_check(99),
            ManualCheckTake::NotFound
        ));
    }
}
