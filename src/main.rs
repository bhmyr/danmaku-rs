use log::{info, error};
use tokio::io::AsyncBufReadExt;
mod storage;
mod client;
mod config;
mod logger;
mod packet;
mod task_manager;
mod login;
use task_manager::TaskManager;
use tokio::{net::TcpListener, sync::mpsc};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
enum Command {
    Reload,
    Stop,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        error!("应用程序错误: {}", e);
        std::process::exit(1);
    }
    info!("资源清理完成，正常退出");
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化阶段
    let config = config::init();
    logger::init();
    
    login::init(&config.auth_file).await?;
    login::refresh_cookie().await?;
    let storage = storage::Storage::start().await?;
    let tx = storage.get_sender()?;
    let task_manager = TaskManager::get_or_init(Some(tx));
    
    let mut cmd_receiver = init_command_listener(&config.control_port).await?;

    // 启动状态检查器
    let cancel_token = CancellationToken::new();
    start_status_checker(cancel_token.child_token());

    // 主事件循环
    main_loop(&mut cmd_receiver, cancel_token).await?;

    // 清理阶段
    shutdown(task_manager, storage).await?;
    Ok(())
}


// 初始化命令监听器
async fn init_command_listener(control_port: &str) -> Result<mpsc::UnboundedReceiver<Command>, Box<dyn std::error::Error>> {
    let (cmd_sender, cmd_receiver) = mpsc::unbounded_channel();
    
    let listener = TcpListener::bind(control_port)
        .await
        .map_err(|e| format!("无法绑定控制端口 {}: {}", control_port, e))?;

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((socket, _)) => {
                    let sender = cmd_sender.clone();
                    tokio::spawn(handle_connection(socket, sender));
                }
                Err(e) => error!("接受连接失败: {}", e),
            }
        }
    });

    Ok(cmd_receiver)
}

// 启动状态检查器
fn start_status_checker(cancel_token: CancellationToken) {
    let task_manager = TaskManager::get_or_init(None);
    tokio::spawn(async move {
        tokio::select! {
            _ = task_manager.run_status_checker() => {}
            _ = cancel_token.cancelled() => {
                info!("状态检查器已取消");
            }
        }
    });
}

// 主事件循环
async fn main_loop(
    cmd_receiver: &mut mpsc::UnboundedReceiver<Command>,
    cancel_token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut shutdown = false;
    let task_manager = TaskManager::get_or_init(None);
    // 初始启动任务
    task_manager.start_and_update().await;

    while !shutdown {
        tokio::select! {
            // 处理系统信号
            _ = tokio::signal::ctrl_c() => {
                info!("接收到终止信号，开始关闭流程...");
                cancel_token.cancel();
                shutdown = true;
            }

            // 处理管理命令
            cmd = cmd_receiver.recv() => match cmd {
                Some(Command::Reload) => {
                    info!("重新加载任务配置");
                    task_manager.start_and_update().await;
                }
                Some(Command::Stop) => {
                    info!("接收到停止命令");
                    cancel_token.cancel();
                    shutdown = true;
                }
                None => shutdown = true,
            },
        }
    }
    Ok(())
}

// 关闭清理过程
async fn shutdown(
    task_manager: &TaskManager,
    storage: storage::Storage,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("开始资源清理...");
    task_manager.shutdown().await;
    storage.close()
        .await
        .map_err(|e| format!("存储关闭失败: {}", e))?;
    Ok(())
}

// 连接处理函数（优化后）
async fn handle_connection(
    mut socket: tokio::net::TcpStream,
    sender: mpsc::UnboundedSender<Command>,
) {
    let mut reader = tokio::io::BufReader::new(&mut socket);
    let mut line = String::new();

    while let Ok(n) = reader.read_line(&mut line).await {
        if n == 0 {
            break;
        }

        let cmd = match line.trim().to_lowercase().as_str() {
            "reload" => Some(Command::Reload),
            "stop" => {
                line.clear();
                Some(Command::Stop)
            }
            _ => {
                info!("收到未知命令: {}", line.trim());
                line.clear();
                continue;
            }
        };
        let is_stop = matches!(cmd, Some(Command::Stop));
        if let Some(command) = cmd {
            if let Err(e) = sender.send(command) {
                error!("命令发送失败: {}", e);
                break;
            }
        }

        line.clear();
        if is_stop {
            break;
        }
    }
}