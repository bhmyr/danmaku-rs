#!/bin/zsh

# macOS应用管理脚本
# 支持功能：start | stop | reload
# 默认控制端口：24349

APP_PATH="./danmaku-rs"
DEFAULT_PORT=24349
OS_TYPE=$(uname -s)

case "$1" in
start)
    # 检测进程是否存在
    if pgrep -fq "$APP_PATH"; then
        echo "错误：应用已在运行中"
        exit 1
    fi

    # 启动应用并后台运行
    echo "正在启动应用..."
    nohup "$APP_PATH" >/dev/null 2>&1 &

    # 等待进程启动
    sleep 1
    if ! pgrep -fq "$APP_PATH"; then
        echo "启动失败，请检查应用可执行权限"
        exit 2
    fi
    echo "应用已启动 (PID: $(pgrep -f "$APP_PATH"))"
    ;;

stop | reload)
    cmd_type=$1
    port=${2:-$DEFAULT_PORT}

    # 验证端口有效性
    if [[ ! "$port" =~ ^[0-9]+$ ]] || ((port > 65535)); then
        echo "错误：无效端口号"
        exit 3
    fi

    # macOS适配的nc命令参数
    case $cmd_type in
    stop)
        echo -n "stop" | nc -4 -G 2 localhost "$port"
        result=$?
        ;;
    reload)
        echo -n "reload" | nc -4 -G 2 localhost "$port"
        result=$?
        ;;
    esac

    # 结果处理
    if [ $result -eq 0 ]; then
        echo "信号已发送到端口 $port"
    else
        echo "错误：无法连接到端口 $port"
        exit 4
    fi
    ;;

*)
    echo "使用方法: $0 {start|stop|reload} [端口] (默认端口=$DEFAULT_PORT)"
    exit 1
    ;;
esac

exit 0
