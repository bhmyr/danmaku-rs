#!/bin/bash

# Shell版应用程序管理脚本（兼容Linux/macOS）
# 使用说明与PowerScript版保持一致

# 应用配置
APP_PATH="./danmaku"

# 参数解析
COMMAND=${1:-}
PORT=${2:-24349}

# 输入验证
validate_arguments() {
    # 验证命令参数
    case "$COMMAND" in
    start | stop | reload) ;;
    *)
        echo "错误：无效命令，支持 start|stop|reload"
        exit 1
        ;;
    esac

    # 验证端口范围
    if ! [[ "$PORT" =~ ^[0-9]+$ ]] || [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
        echo "错误：端口号需在1-65535之间"
        exit 2
    fi
}

# 发送信号函数
send_signal() {
    local msg=$1
    local port=$2

    # 尝试多种发送方式
    if command -v nc &>/dev/null; then
        echo -n "$msg" | nc -w 1 127.0.0.1 "$port" 2>/dev/null
    elif command -v bash &>/dev/null; then
        echo -n "$msg" >"/dev/tcp/127.0.0.1/$port" 2>/dev/null
    else
        echo "错误：需要安装nc或使用支持/dev/tcp的shell"
        return 1
    fi

    [ $? -eq 0 ] || {
        echo "无法连接到端口 $port"
        return 1
    }
}

# 检查进程状态
check_running() {
    if pgrep -f "$(basename "$APP_PATH")" >/dev/null; then
        return 0
    fi
    return 1
}

# 主逻辑
case "$COMMAND" in
start)
    check_running && {
        echo "错误：应用已在运行中"
        exit 3
    }

    echo "正在启动应用..."
    if nohup "$APP_PATH" >/dev/null 2>&1 & then
        sleep 1 # 等待进程启动
        check_running || {
            echo "启动失败：进程未成功运行"
            exit 4
        }
        echo "应用已成功启动（PID: $(pgrep -f "$(basename "$APP_PATH")")）"
    else
        echo "启动命令执行失败"
        exit 5
    fi
    ;;

stop)
    send_signal "stop" "$PORT" || exit 6
    echo "已发送停止信号到端口 $PORT"
    ;;

reload)
    send_signal "reload" "$PORT" || exit 7
    echo "已发送重载信号到端口 $PORT"
    ;;
esac

exit 0
