use crate::storage;
use crate::{client::Client, config};
use anyhow::{Context, Result};
use chrono::Local;
use dashmap::DashMap;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::{sync::mpsc, time::sleep};
type RecordSender = mpsc::Sender<storage::Record>;
type RoomInfoMap = Vec<(u64, RoomInfo)>;

static TASK_MANAGER: OnceLock<TaskManager> = OnceLock::new();

#[derive(Clone)]
pub struct TaskManager {
    tx: RecordSender,
    tasks: DashMap<u64, MonitoringTask>,
}

#[derive(Clone)]
struct MonitoringTask {
    client: Client,
    abort_handle: tokio::task::AbortHandle,
}

#[derive(Debug, Serialize, Deserialize)]
struct APIResponse<T> {
    code: i32,
    message: String,
    data: T,
}

impl TaskManager {
    pub fn get_or_init(tx: Option<RecordSender>) -> &'static Self {
        TASK_MANAGER.get_or_init(|| {
            let tx = tx.expect("RecordSender must be provided during initialization");
            TaskManager {
                tx,
                tasks: DashMap::new(),
            }
        })
    }

    pub async fn start_and_update(&self) {
        let new_uids = load_livers(&&config::init().liver_file)
            .context("加载主播列表失败")
            .unwrap_or_default();
        self.cleanup_stale_tasks(&new_uids).await;

        match get_room_info_by_uids(&new_uids).await {
            Ok(room_list) => {
                self.process_room_updates(&room_list).await;
            }
            Err(e) => error!("房间信息更新失败: {}", e),
        }
    }

    /// 清理不再需要监控的任务
    async fn cleanup_stale_tasks(&self, new_uids: &HashSet<u64>) {
        self.tasks.retain(|uid, task| {
            if !new_uids.contains(uid) {
                debug!("停止监控过期任务: {}", uid);
                task.abort_handle.abort();
                false
            } else {
                true
            }
        });
    }

    /// 获取最新的房间信息

    /// 处理房间更新逻辑
    async fn process_room_updates(&self, room_list: &RoomInfoMap) {
        for (uid, room_info) in room_list {
            self.send_initial_status(*uid, room_info).await;

            if !self.tasks.contains_key(uid) {
                self.start_new_monitoring_task(*uid, room_info.clone())
                    .await;
            }
        }
    }

    /// 发送初始状态记录
    async fn send_initial_status(&self, uid: u64, info: &RoomInfo) {
        let record = create_status_record(uid, info);
        if let Err(e) = self.tx.send(record).await {
            error!("初始状态发送失败 {}: {}", uid, e);
        }
    }

    /// 启动新的监控任务
    async fn start_new_monitoring_task(&self, uid: u64, room_info: RoomInfo) {
        let client = match Client::new(room_info, self.tx.clone()).await {
            Ok(c) => c,
            Err(e) => {
                error!("客户端初始化失败 {}: {}", uid, e);
                return;
            }
        };

        let abort_handle = self.spawn_monitoring_task(uid, client.clone());
        self.tasks.insert(
            uid,
            MonitoringTask {
                client,
                abort_handle,
            },
        );
        debug!("新增监控任务: {}", uid);

        sleep(Duration::from_secs(2)).await;
    }

    /// 生成监控任务
    fn spawn_monitoring_task(&self, uid: u64, client: Client) -> tokio::task::AbortHandle {
        tokio::spawn(async move {
            info!("监控任务启动: {}", uid);
            if let Err(e) = client.run().await {
                warn!("监控任务异常结束 {}: {}", uid, e);
            }
        })
        .abort_handle()
    }

    /// 定期状态检查
    pub async fn run_status_checker(&self) {
        debug!("定期状态检查任务启动...");
        if config::init().live_interval < Duration::from_secs(120) {
            return;
        }
        sleep(config::init().live_interval).await;
        let mut interval = tokio::time::interval(config::init().live_interval);
        loop {
            interval.tick().await;
            debug!("执行定期状态检查");
            let uids: HashSet<u64> = self.tasks.iter().map(|entry| entry.key().clone()).collect();
            match get_room_info_by_uids(&uids).await {
                Ok(room_list) => self.process_status_updates(&room_list).await,
                Err(e) => error!("状态检查失败: {}", e),
            }
        }
    }

    /// 处理状态更新
    async fn process_status_updates(&self, room_list: &RoomInfoMap) {
        let livers = room_list.len();
        let mut online = 0;
        let mut active = 0;
        for (uid, info) in room_list {
            if let Some(entry) = self.tasks.get(uid) {
                entry.client.update_status(info);
                if info.live_status == 1 {
                    online += 1;
                }
                let last_active = entry.client.last_active.load(Ordering::Relaxed);
                if Local::now().timestamp() as u64 - last_active < 180 {
                    active += 1;
                }
            }
            let record = create_status_record(*uid, info);
            if let Err(e) = self.tx.send(record).await {
                error!("状态更新发送失败 {}: {}", uid, e);
            }
        }
        info!(
            "主播数: {}, 正在直播: {}, 正常监控: {}",
            livers, online, active
        );
    }

    /// 安全关闭所有任务
    pub async fn shutdown(&self) {
        debug!("开始安全关闭流程");
        for entry in self.tasks.iter() {
            debug!("关闭任务: {}", entry.key());
            entry.abort_handle.abort();
        }
    }
}

/// 创建状态记录对象
fn create_status_record(uid: u64, info: &RoomInfo) -> storage::Record {
    storage::Record {
        ispaid: "free".to_string(),
        datetime: Local::now(),
        uid,
        cmd: "LIVE_STATUS".to_string(),
        keys: vec!["live_status".to_string(), "live_time".to_string()],
        values: vec![json!(info.live_status), json!(info.live_time)],
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RoomInfo {
    pub title: String,
    pub room_id: u64,
    #[serde(skip)]
    pub uid: u64,
    pub uname: String,
    pub live_status: u64,
    pub live_time: u64,
}

/// 批量获取直播间信息
pub async fn get_room_info_by_uids(uids: &HashSet<u64>) -> Result<RoomInfoMap> {
    let response = reqwest::Client::new()
        .post("https://api.live.bilibili.com/room/v1/Room/get_status_info_by_uids")
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/89.0.4389.90 Safari/537.36")
        .header("Content-Type", "application/json")
        .json(&json!({ "uids": uids }))
        .send()
        .await?
        .json::<APIResponse<HashMap<String, RoomInfo>>>()
        .await?;

    if response.code != 0 {
        return Err(anyhow::anyhow!("API 返回错误: code {}", response.code));
    }

    response
        .data
        .into_iter()
        .map(|(uid_str, mut room_info)| {
            let uid = uid_str.parse().context("UID 格式错误")?;
            room_info.uid = uid;
            Ok((uid, room_info))
        })
        .collect()
}

#[derive(Debug, Serialize, Deserialize)]
struct LiverInfo {
    uid: u64,
    status: bool,
}

/// 加载主播列表
pub fn load_livers(file_path: &str) -> Result<HashSet<u64>> {
    let content = fs::read_to_string(file_path).context("读取主播列表文件失败")?;
    let data: HashMap<String, LiverInfo> = toml::from_str(&content).context("解析主播列表失败")?;

    Ok(data
        .into_iter()
        .filter(|(_, v)| v.status)
        .map(|(_, v)| v.uid)
        .collect())
}
