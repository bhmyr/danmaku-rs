use crate::{
    config::{self, MessageTypeConfig},
    login::{get_cookie, get_uid},
    packet, storage, task_manager,
};
use anyhow::{Context, Result, anyhow};
use chrono::Local;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::time::sleep;
use tokio_native_tls::{TlsConnector, native_tls};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{client::IntoClientRequest, http::header, protocol::Message},
};
use url::Url;

type WsStream = WebSocketStream<tokio_native_tls::TlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Host {
    pub host: String,
    pub port: u16,
    pub wss_port: u16,
    pub ws_port: u16,
}

#[derive(Debug, Serialize, Deserialize)]
struct APIResponse<T> {
    code: i32,
    message: String,
    data: T,
}

#[derive(Debug, Serialize, Deserialize)]
struct DanmuData {
    token: String,
    host_list: Vec<Host>,
}

struct WsConnection {
    writer: Arc<Mutex<SplitSink<WsStream, Message>>>,
    token: String,
    room_id: u64,
}

impl WsConnection {
    async fn new(
        host: &Host,
        token: String,
        room_id: u64,
        cookie: &str,
    ) -> Result<(Self, SplitStream<WsStream>)> {
        let tcp_stream = tokio::net::TcpStream::connect(format!("{}:{}", host.host, host.wss_port))
            .await
            .context(format!("TCP连接失败: {}:{}", host.host, host.wss_port))?;

        let tls_connector = TlsConnector::from(
            native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .context("TLS连接器构建失败")?,
        );

        let tls_stream = tls_connector
            .connect(&host.host, tcp_stream)
            .await
            .context(format!("TLS握手失败: {}", host.host))?;

        let mut request = Url::parse(&format!("wss://{}:{}/sub", host.host, host.wss_port))
            .context(format!("URL解析失败: {}:{}", host.host, host.wss_port))?
            .into_client_request()
            .context("WebSocket请求构建失败")?;

        request.headers_mut().insert(
            header::COOKIE,
            header::HeaderValue::from_str(cookie).context("Cookie格式无效")?,
        );

        let (stream, _) = tokio_tungstenite::client_async(request, tls_stream)
            .await
            .context("WebSocket连接失败")?;

        let (writer, reader) = stream.split();
        Ok((
            Self {
                writer: Arc::new(Mutex::new(writer)),
                token,
                room_id,
            },
            reader,
        ))
    }

    async fn send_packet(&self, packet: Vec<u8>) -> Result<()> {
        self.writer
            .lock()
            .await
            .send(Message::Binary(packet))
            .await
            .context("发送WebSocket消息失败")?;
        Ok(())
    }

    async fn send_auth(&self) -> Result<()> {
        let auth_packet = packet::get_auth_packet(get_uid(), self.room_id, self.token.clone())?;
        self.send_packet(auth_packet).await
    }

    async fn send_heartbeat(&self) -> Result<()> {
        let heartbeat_packet = packet::get_heartbeat_packet();
        self.send_packet(heartbeat_packet).await
    }
}

#[derive(Clone)]
pub struct Client {
    pub room_id: u64,
    pub uid: u64,
    pub uname: String,
    pub status: Arc<AtomicU64>,
    pub last_active: Arc<AtomicU64>,
    sender: mpsc::Sender<storage::Record>,
    token: String,
    hosts: Vec<Host>,
}

impl Client {
    pub async fn new(
        room_info: task_manager::RoomInfo,
        sender: mpsc::Sender<storage::Record>,
    ) -> Result<Self> {
        let (token, hosts) = Self::refresh_connection_info(room_info.room_id).await?;

        Ok(Self {
            room_id: room_info.room_id,
            uid: room_info.uid,
            uname: room_info.uname,
            status: Arc::new(AtomicU64::new(room_info.live_status)),
            last_active: Arc::new(AtomicU64::new(0)),
            sender,
            token,
            hosts,
        })
    }

    async fn refresh_connection_info(room_id: u64) -> Result<(String, Vec<Host>)> {
        get_danmu_info(room_id, &get_cookie())
            .await
            .context("刷新连接信息失败")
    }

    pub fn update_status(&self, room_info: &task_manager::RoomInfo) {
        let old_status = self.status.load(Ordering::Acquire);
        if old_status != room_info.live_status {
            match room_info.live_status {
                1 => {
                    let minutes =(Local::now().timestamp() - room_info.live_time as  i64) / 60;
                    info!("{} 已直播“{}”{}分钟", room_info.uname, room_info.title, minutes);
                }
                _ => {
                    info!("{} 停止直播。", room_info.uname);
                }
            }
            
            self.status.store(room_info.live_status, Ordering::Release);
        }
    }

    pub async fn run(&self) -> Result<()> {
        let mut connection_info = (self.token.clone(), self.hosts.clone());
        let mut refresh_attempts = 0;

        loop {
            match self
                .try_all_hosts(&connection_info.1, &connection_info.0)
                .await
            {
                Ok(()) => warn!("{}, 连接断开，正在重连...", self.uname),
                Err(_) if refresh_attempts < 3 => {
                    connection_info = Self::refresh_connection_info(self.room_id).await?;
                    refresh_attempts += 1;
                }
                Err(e) => return Err(e),
            }

            sleep(Duration::from_secs(1 << refresh_attempts)).await;
        }
    }

    async fn try_all_hosts(&self, hosts: &[Host], token: &str) -> Result<()> {
        for (idx, host) in hosts.iter().enumerate() {
            let delay = Duration::from_secs(2u64.pow(idx as u32));
            match self.connect_and_handle(host, token).await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    warn!(
                        "{} 连接主机 {} 失败: {}. {}秒后重试",
                        self.uname,
                        host.host,
                        e,
                        delay.as_secs()
                    );
                    sleep(delay).await;
                }
            }
        }
        Err(anyhow!("所有主机连接尝试失败"))
    }

    async fn connect_and_handle(&self, host: &Host, token: &str) -> Result<()> {
        let (conn, reader) =
            WsConnection::new(host, token.to_string(), self.room_id, &get_cookie()).await?;
        conn.send_auth().await?;
        info!("成功连接到房间 {}", self.uname);

        let heartbeat = self.spawn_heartbeat(conn);
        let message_handler = self.spawn_message_handler(reader);

        tokio::select! {
            result = heartbeat => result,
            result = message_handler => result,
        }
    }

    async fn spawn_heartbeat(&self, conn: WsConnection) -> Result<()> {
        let interval = Duration::from_secs(30);
        loop {
            sleep(interval).await;
            match conn.send_heartbeat().await {
                Ok(_) => {}
                Err(e) => warn!("心跳包发送失败: {}", e),
            }
        }
    }

    async fn spawn_message_handler(&self, mut reader: SplitStream<WsStream>) -> Result<()> {
        while let Some(msg) = reader.next().await {
            let msg = match msg {
                Ok(msg) => msg,
                Err(e) => {
                    error!("{} 消息接收失败: {}", self.uname, e);
                    continue;
                }
            };
            if let Message::Binary(data) = msg {
                match self.process_binary_data(&data).await {
                    Ok(_) => {}
                    Err(e) => error!("{} 消息处理失败: {}", self.uname, e),
                }
            }
            self.last_active
                .store(Local::now().timestamp() as u64, Ordering::Release);
        }
        Ok(())
    }

    async fn process_binary_data(&self, data: &[u8]) -> Result<()> {
        for packet in packet::parse_packet(data).context("解析数据包失败")? {
            if packet.opcode == 5 {
                let json = packet.to_json().context("解析JSON数据失败")?;
                self.handle_json_message(&json).await?;
            }
        }
        Ok(())
    }

    async fn handle_json_message(&self, json: &serde_json::Value) -> Result<()> {
        let cmd = json
            .get("cmd")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("JSON数据中缺少cmd字段"))?;

        let status = self.status.load(Ordering::Acquire);
        if !should_process(cmd) && status != 1 {
            return Ok(());
        }

        for ispaid in ["free", "paid"] {
            if let Some(config) = get_message_config(cmd, ispaid) {
                if self.should_process_message(json, config)? {
                    self.send_to_csv(cmd, ispaid, json, config).await?;
                }
            }
        }
        Ok(())
    }

    fn should_process_message(
        &self,
        json: &serde_json::Value,
        config: &MessageTypeConfig,
    ) -> Result<bool> {
        if let Some(condition) = config.selection_condition() {
            Ok(check_condition(json, condition))
        } else {
            Ok(true)
        }
    }

    async fn send_to_csv(
        &self,
        cmd: &str,
        ispaid: &str,
        json: &serde_json::Value,
        config: &MessageTypeConfig,
    ) -> Result<()> {
        let name = match &config.name {
            Some(name) => name,
            None => &cmd.to_string(),
        };

        let mut record = storage::Record {
            ispaid: ispaid.to_string(),
            datetime: Local::now(),
            uid: self.uid,
            cmd: name.to_string(),
            keys: vec![],
            values: vec![],
        };

        for (key, path) in config.data_fields() {
            let value = match get_json_value(json, path) {
                Some(v) => v,
                None => {
                    warn!("JSON数据中缺少字段 {}:{:#?}", path, json);
                    &serde_json::Value::Null
                }
            };

            record.keys.push(key.to_string());
            record.values.push(value.clone());
        }

        self.sender.send(record).await?;
        Ok(())
    }
}

fn get_json_value<'a>(json: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.').try_fold(json, |acc, part| match acc {
        serde_json::Value::Object(map) => map.get(part),
        serde_json::Value::Array(arr) => part.parse::<usize>().ok().and_then(|i| arr.get(i)),
        _ => None,
    })
}

fn check_condition(json: &serde_json::Value, condition: &str) -> bool {
    let (path, expected) = match condition.split_once('=') {
        Some(parts) => parts,
        None => return false,
    };

    get_json_value(json, path)
        .and_then(|v| v.as_str())
        .map(|v| v == expected)
        .unwrap_or(false)
}

impl MessageTypeConfig {
    fn selection_condition(&self) -> Option<&str> {
        self.keys
            .iter()
            .position(|k| k == "_select")
            .and_then(|i| self.values.get(i))
            .map(|s| s.as_str())
    }

    fn data_fields(&self) -> impl Iterator<Item = (&String, &String)> {
        self.keys
            .iter()
            .zip(self.values.iter())
            .filter(|(k, _)| *k != "_select")
    }
}

fn should_process(cmd: &str) -> bool {
    config::init().live_interval < Duration::from_secs(120)
        || config::init().offline_msg.is_empty()
        || config::init().offline_msg.contains(cmd)
}

fn get_message_config(cmd: &str, ispaid: &str) -> Option<&'static MessageTypeConfig> {
    config::init()
        .message_configs
        .get(cmd)
        .and_then(|cfg| cfg.get(ispaid))
}

/// 获取弹幕服务器信息
async fn get_danmu_info(room_id: u64, cookie: &str) -> Result<(String, Vec<Host>)> {
    let client = reqwest::Client::builder().cookie_store(true).build()?;

    let response = client
        .get(format!(
            "https://api.live.bilibili.com/xlive/web-room/v1/index/getDanmuInfo?id={}",
            room_id
        ))
        .header("Cookie", cookie)
        .send()
        .await
        .context("请求弹幕服务器信息失败")?
        .json::<APIResponse<DanmuData>>()
        .await
        .context("解析弹幕服务器信息失败")?;

    if response.code != 0 {
        return Err(anyhow!("API错误: 错误码 {}", response.code));
    }

    Ok((response.data.token, response.data.host_list))
}
