<#
.SYNOPSIS
应用程序管理脚本（Windows PowerShell版）

.DESCRIPTION
实现应用的启动、停止、重载功能，支持自定义控制端口

.PARAMETER Command
必填参数，支持命令：start | stop | reload

.PARAMETER Port
可选参数，控制端口号（默认24349）

.EXAMPLE
PS> .\manage.ps1 start
使用默认端口24349启动应用

.EXAMPLE
PS> .\manage.ps1 stop -Port 8080
向8080端口发送停止信号

.EXAMPLE
PS> .\manage.ps1 reload 24350
向24350端口发送重载信号
#>

param(
    [Parameter(Mandatory=$true, Position=0)]
    [ValidateSet("start", "stop", "reload")]
    [string]$Command,

    [Parameter(Position=1)]
    [ValidateRange(1, 65535)]
    [int]$Port = 24349
)

# 应用配置
$appPath = ".\danmaku.exe"  # 修改为实际可执行文件路径

function Send-Signal {
    param(
        [string]$Message,
        [int]$Port
    )

    try {
        $client = New-Object System.Net.Sockets.TcpClient("localhost", $Port)
        $stream = $client.GetStream()
        $encoder = [System.Text.Encoding]::ASCII
        $buffer = $encoder.GetBytes($Message)
        $stream.Write($buffer, 0, $buffer.Length)
        $stream.Flush()
        $client.Close()
        return $true
    }
    catch {
        Write-Error "无法连接到端口 $Port : $_"
        return $false
    }
}

switch ($Command) {
    "start" {
        # 检查现有进程
        if (Get-Process -Name "danmaku" -ErrorAction SilentlyContinue) {
            Write-Error "错误：应用已在运行中"
            exit 1
        }

        try {
            Start-Process -FilePath $appPath -WindowStyle Hidden
            Write-Host "应用已成功启动（PID: $((Get-Process -Name "app").Id)）"
        }
        catch {
            Write-Error "启动失败: $_"
            exit 2
        }
    }

    "stop" {
        if (-not (Send-Signal -Message "stop" -Port $Port)) {
            exit 3
        }
        Write-Host "已发送停止信号到端口 $Port"
    }

    "reload" {
        if (-not (Send-Signal -Message "reload" -Port $Port)) {
            exit 4
        }
        Write-Host "已发送重载信号到端口 $Port"
    }
}

exit 0