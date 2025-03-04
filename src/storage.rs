use crate::config;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use csv::Writer;
use dashmap::{DashMap, DashSet};
use log::error;
use serde::Deserialize;
use serde_json::Value;
use sqlx::Acquire;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{MySql, Pool, Postgres, Sqlite};
use std::fs::{File, OpenOptions};

use std::path::{Path, PathBuf};

use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Duration, interval};

#[derive(Debug, Clone)]
pub struct Record {
    pub ispaid: String,
    pub datetime: DateTime<Local>,
    pub uid: u64,
    pub cmd: String,
    pub keys: Vec<String>,
    pub values: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub enum StorageType {
    Csv,
    MySql,
    Postgres,
    Sqlite,
}
fn storage_type_deserialize<'de, D>(deserializer: D) -> Result<StorageType, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    match s.as_str() {
        "csv" => Ok(StorageType::Csv),
        "mysql" => Ok(StorageType::MySql),
        "postgres" => Ok(StorageType::Postgres),
        "sqlite" => Ok(StorageType::Sqlite),
        _ => Err(serde::de::Error::custom(format!("未知存储类型: {}", s))),
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct StorageConfig {
    #[serde(rename = "DateFormat")]
    pub date_format: String,
    #[serde(rename = "StorageType", deserialize_with = "storage_type_deserialize")]
    pub storage_type: StorageType,
    #[serde(rename = "MysqlUrl")] // mysql://user:password@host:port/dbname
    pub mysql_url: Option<String>,
    #[serde(rename = "PostgresUrl")] // postgres://user:password@host:port/dbname
    pub postgres_url: Option<String>,
    #[serde(rename = "SqlitePath")] // sqlite://./data/
    pub sqlite_url: Option<String>,
    #[serde(rename = "CsvPath")] // ./data/
    pub csv_path: Option<String>,
}

#[derive(Clone)]
pub struct Storage {
    config: StorageConfig,
    mysql_pool: Option<Pool<MySql>>,
    pg_pool: Option<Pool<Postgres>>,
    sqlite_pool: Option<Pool<Sqlite>>,

    csv_writers: Arc<DashMap<String, Mutex<Writer<File>>>>,
    created_tables: Arc<DashSet<String>>,
    batch_size: usize,
    max_buffer: usize,
    flush_interval: Duration,

    // 新增字段
    buffer: Arc<DashMap<String, Vec<Record>>>,
    tx: Option<mpsc::Sender<Record>>,
    shutdown_tx: Option<mpsc::Sender<()>>,
}

impl Storage {
    pub async fn start() -> Result<Self> {
        let config = config::init().storage_config.clone();
        let (tx, mut receiver) = mpsc::channel::<Record>(10000);
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        let storage_type = config.storage_type.clone();

        if let StorageType::Csv = &storage_type {
            if let Some(csv_path) = &config.csv_path {
                std::fs::create_dir_all(csv_path)
                    .with_context(|| format!("创建CSV目录失败: {}", csv_path))?;
            }
        }

        let buffer = Arc::new(DashMap::<String, Vec<Record>>::new());
        let buffer_clone = buffer.clone();

        let mut storage = Storage {
            config,
            mysql_pool: None,
            pg_pool: None,
            sqlite_pool: None,
            csv_writers: Arc::new(DashMap::new()),
            created_tables: Arc::new(DashSet::new()),
            batch_size: 1000,
            max_buffer: 10_000,
            flush_interval: Duration::from_secs(10),
            buffer,
            tx: Some(tx.clone()),
            shutdown_tx: Some(shutdown_tx),
        };

        // 数据库连接初始化代码保持不变
        match storage_type {
            StorageType::MySql => {
                let url = storage.config.mysql_url.as_ref().context("MySQL配置缺失")?;
                storage.mysql_pool = Some(
                    sqlx::MySqlPool::connect(url)
                        .await
                        .with_context(|| format!("MySQL连接失败: {}", url))?,
                );
            }
            StorageType::Postgres => {
                let url = storage
                    .config
                    .postgres_url
                    .as_ref()
                    .context("Postgres配置缺失")?;
                storage.pg_pool = Some(
                    sqlx::PgPool::connect(url)
                        .await
                        .with_context(|| format!("Postgres连接失败: {}", url))?,
                );
            }
            StorageType::Sqlite => {
                let url = storage
                    .config
                    .sqlite_url
                    .as_ref()
                    .context("SQLite配置缺失")?;

                let base_dir = url.replace("sqlite:", "");
                let db_path = PathBuf::from(base_dir).join("blive.db");

                if let Some(parent) = Path::new(&db_path).parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("创建目录失败: {}", parent.display()))?;
                }
                let options = SqliteConnectOptions::new()
                    .filename(&db_path)
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .statement_cache_capacity(10);

                storage.sqlite_pool = Some(
                    SqlitePoolOptions::new()
                        .max_connections(2)
                        .connect_with(options)
                        .await
                        .with_context(|| "SQLite连接池创建失败")?,
                );
            }
            _ => {}
        }

        let storage_clone = storage.clone();

        tokio::spawn(async move {
            let mut flush_timer = interval(storage_clone.flush_interval);
            let mut current_buffer = 0;

            loop {
                tokio::select! {
                    // 接收关闭信号
                    Some(_) = shutdown_rx.recv() => {
                        log::info!("接收到关闭信号，正在刷新剩余数据...");
                        if let Err(e) = flush_all(&buffer_clone, &storage_clone).await {
                            log::error!("关闭时刷新失败: {:?}", e);
                        }
                        break;
                    }

                    // 接收记录
                    Some(record) = receiver.recv() => {
                        if let Err(e) = storage_clone.precheck_storage(&record).await {
                            log::error!("存储预检查失败: {:?}", e);
                            continue;
                        }

                        let table = get_name(&record.datetime, &record.ispaid, &storage_clone.config.date_format);
                        buffer_clone.entry(table).or_default().push(record);
                        current_buffer += 1;

                        // 批量刷新逻辑
                        if current_buffer >= storage_clone.batch_size {
                            if let Err(e) = flush_all(&buffer_clone, &storage_clone).await {
                                log::error!("批量刷新失败: {:?}", e);
                            } else {
                                current_buffer = 0;
                            }
                        }
                    }

                    // 定时刷新
                    _ = flush_timer.tick() => {
                        if current_buffer > 0 {
                            if let Err(e) = flush_all(&buffer_clone, &storage_clone).await {
                                log::error!("定时刷新失败: {:?}", e);
                            } else {
                                current_buffer = 0;
                            }
                        }
                    }
                }

                // 内存保护
                if current_buffer > storage_clone.max_buffer {
                    log::warn!("缓冲区溢出，强制刷新");
                    if let Err(e) = flush_all(&buffer_clone, &storage_clone).await {
                        log::error!("强制刷新失败: {:?}", e);
                        buffer_clone.clear();
                    }
                    current_buffer = 0;
                }
            }

            log::info!("存储后台任务已退出");
        });

        Ok(storage)
    }

    // 获取发送器方法
    pub fn get_sender(&self) -> Result<mpsc::Sender<Record>> {
        self.tx.clone().context("发送器已被消费")
    }

    // 完整的关闭方法
    pub async fn close(mut self) -> Result<()> {
        log::info!("正在关闭存储资源...");

        // 发送关闭信号
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            if let Err(e) = shutdown_tx.send(()).await {
                log::warn!("发送关闭信号失败: {:?}", e);
            }
        }

        // 等待一段时间确保后台任务有时间处理关闭信号
        tokio::time::sleep(Duration::from_secs(2)).await;

        // 手动刷新剩余数据以防万一
        if let Err(e) = flush_all(&self.buffer, &self).await {
            log::error!("关闭时手动刷新失败: {:?}", e);
        }

        // 关闭所有CSV写入器
        for mut entry in self.csv_writers.iter_mut() {
            let mut writer = entry.value_mut().lock().await;
            if let Err(e) = writer.flush() {
                log::error!("关闭CSV写入器失败: {:?}", e);
            }
        }

        // 关闭数据库连接池
        if let Some(pool) = &self.mysql_pool {
            pool.close().await;
        }

        if let Some(pool) = &self.pg_pool {
            pool.close().await;
        }

        if let Some(pool) = &self.sqlite_pool {
            pool.close().await;
        }

        log::info!("所有存储资源已关闭");
        Ok(())
    }

    async fn create_table(&self, storage_type: &StorageType, table: &str) -> Result<()> {
        match storage_type {
            StorageType::MySql => {
                let pool = self.mysql_pool.as_ref().context("MySQL连接池未初始化")?;
                sqlx::query(&format!(
                    "CREATE TABLE IF NOT EXISTS {} (
                        timestamp BIGINT,
                        uid BIGINT,
                        cmd VARCHAR(255),
                        info_jsonb JSON
                    )",
                    table
                ))
                .execute(pool)
                .await
                .with_context(|| format!("创建MySQL表失败: {}", table))?;
            }
            StorageType::Postgres => {
                let pool = self.pg_pool.as_ref().context("Postgres连接池未初始化")?;
                sqlx::query(&format!(
                    "CREATE TABLE IF NOT EXISTS {} (
                        timestamp BIGINT,
                        uid BIGINT,
                        cmd VARCHAR(255),
                        info_jsonb JSONB
                    )",
                    table
                ))
                .execute(pool)
                .await
                .with_context(|| format!("创建Postgres表失败: {}", table))?;
            }
            StorageType::Sqlite => {
                let pool = self.sqlite_pool.as_ref().context("SQLite连接池未初始化")?;
                sqlx::query(&format!(
                    "CREATE TABLE IF NOT EXISTS {} (
                        timestamp INTEGER,
                        uid INTEGER,
                        cmd TEXT,
                        info_tostring TEXT
                    )",
                    table
                ))
                .execute(pool)
                .await
                .with_context(|| format!("创建SQLite表失败: {}", table))?;
            }
            _ => {}
        }
        Ok(())
    }
    fn get_full_csv_path(&self, base: String) -> String {
        let base_name = format!("{}.csv", base);
        if let Some(csv_path) = &self.config.csv_path {
            Path::new(csv_path)
                .join(base_name)
                .to_string_lossy()
                .into_owned()
        } else {
            base_name
        }
    }
    // 新增预检查方法（带缓存）
    async fn precheck_storage(&self, record: &Record) -> Result<()> {
        let key = match &self.config.storage_type {
            StorageType::Csv => {
                let filename = self.get_full_csv_path(get_name(
                    &record.datetime,
                    &record.ispaid,
                    &self.config.date_format,
                ));
                format!("csv:{}", filename)
            }
            _ => {
                let table = get_name(&record.datetime, &record.ispaid, &self.config.date_format);
                format!("{:?}:{}", self.config.storage_type, table)
            }
        };

        if self.created_tables.contains(&key) {
            return Ok(());
        }

        match &self.config.storage_type {
            StorageType::Csv => {
                let filename = self.get_full_csv_path(get_name(
                    &record.datetime,
                    &record.ispaid,
                    &self.config.date_format,
                ));
                let path = Path::new(&filename);
                if !path.exists() {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)
                            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
                    }
                    let file = File::create(path).context("创建CSV文件失败")?;
                    let mut wtr = Writer::from_writer(file);
                    wtr.write_record(&["timestamp", "uid", "cmd", "info"])
                        .context("写入CSV头失败")?;
                    wtr.flush().context("刷新CSV失败")?;
                }
            }
            _ => {
                let table = get_name(&record.datetime, &record.ispaid, &self.config.date_format);
                self.create_table(&self.config.storage_type, &table).await?;
            }
        }

        self.created_tables.insert(key);
        Ok(())
    }

    // 优化后的CSV写入
    async fn write_csv_batch(&self, filename: String, records: Vec<Record>) -> Result<()> {
        let filename = self.get_full_csv_path(filename);
        let entry = self.csv_writers.entry(filename.clone());
        let mutex = match entry {
            dashmap::mapref::entry::Entry::Occupied(o) => o.into_ref(),
            dashmap::mapref::entry::Entry::Vacant(v) => {
                let path = Path::new(&filename);
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("打开CSV文件失败: {}", filename))?;
                v.insert(Mutex::new(Writer::from_writer(file)))
            }
        };

        let mut writer = mutex.lock().await;
        for record in records {
            let info = build_json(record.keys, record.values).unwrap_or_default();
            writer.write_record(&[
                record.datetime.timestamp().to_string(),
                record.uid.to_string(),
                record.cmd,
                info.to_string(),
            ])?;
        }
        writer.flush()?;
        Ok(())
    }

    // 并行刷新入口
    async fn flush_internal(&self, table: String, records: Vec<Record>) -> Result<()> {
        match &self.config.storage_type {
            StorageType::Csv => self.write_csv_batch(table, records).await,
            StorageType::MySql => self.flush_mysql(table, records).await,
            StorageType::Postgres => self.flush_postgres(table, records).await,
            StorageType::Sqlite => self.flush_sqlite(table, records).await,
        }
    }

    // 优化后的SQLite写入
    async fn flush_sqlite(&self, table: String, records: Vec<Record>) -> Result<()> {
        let pool = self.sqlite_pool.as_ref().context("SQLite未配置")?;
        let mut conn = pool.acquire().await?;
        let mut tx = conn.begin().await?;

        // 使用预编译语句
        let query = format!(
            "INSERT INTO {} (timestamp, uid, cmd, info_tostring) VALUES (?, ?, ?, ?)",
            table
        );

        for chunk in records.chunks(1000) {
            for record in chunk {
                let stmt = sqlx::query(&query);
                let info =
                    build_json(record.keys.clone(), record.values.clone()).unwrap_or_default();
                stmt.bind(record.datetime.timestamp())
                    .bind(record.uid as i64)
                    .bind(&record.cmd)
                    .bind(info.to_string())
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            tx = conn.begin().await?;
        }
        Ok(())
    }

    // MySQL批量写入
    async fn flush_mysql(&self, table: String, records: Vec<Record>) -> Result<()> {
        let pool = self.mysql_pool.as_ref().context("MySQL未配置")?;

        let mut query_builder = sqlx::QueryBuilder::new(format!(
            "INSERT INTO {} (timestamp, uid, cmd, info_jsonb) ",
            table
        ));

        query_builder.push_values(records, |mut b, record| {
            let info_json = build_json(record.keys, record.values).expect("JSON构建失败");
            b.push_bind(record.datetime.timestamp() as i64)
                .push_bind(record.uid)
                .push_bind(record.cmd)
                .push_bind(info_json);
        });

        let query = query_builder.build();
        query.execute(pool).await.context("MySQL批量插入失败")?;
        Ok(())
    }
    // Postgres批量写入
    async fn flush_postgres(&self, table: String, records: Vec<Record>) -> Result<()> {
        let pool = self.pg_pool.as_ref().context("Postgres未配置")?;

        let timestamps: Vec<i64> = records
            .iter()
            .map(|r| r.datetime.timestamp() as i64)
            .collect();
        let uids: Vec<i64> = records.iter().map(|r| r.uid as i64).collect();
        let cmds: Vec<&str> = records.iter().map(|r| r.cmd.as_str()).collect();
        let infos: Vec<Value> = records
            .iter()
            .map(|r| build_json(r.keys.clone(), r.values.clone()).unwrap())
            .collect();

        sqlx::query(&format!(
            "INSERT INTO {} (timestamp, uid, cmd, info_jsonb)
             SELECT * FROM UNNEST($1::BIGINT[], $2::BIGINT[], $3::TEXT[], $4::JSONB[])",
            table
        ))
        .bind(timestamps)
        .bind(uids)
        .bind(cmds)
        .bind(infos)
        .execute(pool)
        .await
        .context("Postgres批量插入失败")?;

        Ok(())
    }
}

async fn flush_all(buffer: &DashMap<String, Vec<Record>>, storage: &Storage) -> Result<()> {
    let mut tasks = Vec::new();
    let mut errors = Vec::new();

    let entries: Vec<(String, Vec<Record>)> = buffer
        .iter_mut()
        .map(|mut entry| (entry.key().clone(), entry.value_mut().drain(..).collect()))
        .collect();

    for (table, records) in entries {
        if records.is_empty() {
            continue;
        }

        let storage_clone = storage.clone();

        let task = tokio::spawn(async move {
            match storage_clone.flush_internal(table.clone(), records).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    error!("写入失败 {}: {}", table, e);
                    Err(e)
                }
            }
        });
        tasks.push(task);
    }

    // 等待所有任务完成
    for task in tasks {
        match task.await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => errors.push(e),
            Err(e) => errors.push(anyhow::anyhow!("任务崩溃: {:?}", e)),
        }
    }

    if !errors.is_empty() {
        bail!("刷新失败: {:?}", errors);
    }
    Ok(())
}

fn get_name(datetime: &DateTime<Local>, ispaid: &str, format: &str) -> String {
    let date = datetime.format(format).to_string();
    format!("{}_{}", ispaid, date)
}

fn build_json(keys: Vec<String>, values: Vec<Value>) -> Result<Value> {
    if keys.len() != values.len() {
        bail!("keys和values长度不一致");
    }
    let mut map = serde_json::Map::new();
    for (key, value) in keys.into_iter().zip(values.into_iter()) {
        map.insert(key, value);
    }
    Ok(Value::Object(map))
}
