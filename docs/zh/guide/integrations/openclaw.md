---
title: OpenClaw 集成指南
author: GongHening
date: 2026-05-19
tags:
  - integration
  - openclaw
lang: zh-CN
---

# OpenClaw 集成指南

## 集成对象与版本

- **框架：** [OpenClaw](https://github.com/openclaw/openclaw) — 个人 AI 助手，运行在你自己的设备上，支持多消息渠道（微信、QQ、WhatsApp、Telegram、Discord 等）
- **Cube Sandbox：** 任意部署方式（单机、多节点集群）
- **运行时：** Node.js 24（推荐）或 22.16+；Python 3.8+ 配合 `e2b-code-interpreter`
- **集成类型：** Run your agent inside Cube — OpenClaw Gateway 直接运行在 Cube MicroVM 内部，为整个 Agent 运行时提供内核级隔离

## 架构

```
用户 → 消息渠道（微信/QQ/WhatsApp/Telegram/Discord/...）
          │
          ▼
Cube MicroVM（KVM 隔离）
    │
    ├── Node.js 24 + OpenClaw Gateway（Agent 运行时）
    │       │
    │       ├── LLM API 调用（访问外部模型服务）
    │       ├── Skills 和工具（在沙箱内原生执行）
    │       ├── 多渠道连接（Gateway 统一管理）
    │       └── 文件 I/O、包安装、Shell 命令
    │
    └── sandbox-code 镜像（Ubuntu + Python + Jupyter）
```

在 Cube 中运行 OpenClaw 的核心价值：

- **内核级隔离** — Agent 进程、工具调用、代码执行全部运行在专用的 KVM MicroVM 中
- **默认即用即毁** — 每次从干净快照启动，退出时自动销毁
- **可复现** — Agent 始终在相同的已知良好环境中运行
- **网络管控** — 由你决定 Agent 是否可以访问互联网，以及可以访问哪些地址
- **暂停与恢复** — 会话中途冻结整个 Agent，之后恢复，内存状态毫发无损

## 前置条件

- 已部署的 Cube Sandbox 环境（[快速开始](https://cube-sandbox.pages.dev/zh/guide/quickstart)、[单机一键部署](https://cube-sandbox.pages.dev/zh/guide/one-click-deploy) 或 [多节点集群部署](https://cube-sandbox.pages.dev/zh/guide/multi-node-deploy)）
- Python 3.8+，已安装 `e2b-code-interpreter`（用于从外部管理沙箱）

```bash
pip install e2b-code-interpreter
```

## 接入步骤

### 1. 部署 Cube Sandbox

按上述部署文档获取一个可用的 Cube 实例。部署完成后应具备：

- **Cube API Server** 监听 `<节点IP>:3000`（HTTP）
- **CubeProxy** 同时监听 80（HTTP）和 443（HTTPS）端口

> **国内镜像：** 如果部署在国内环境、无法访问 GitHub，请使用 `MIRROR=cn` 参数通过 `cnb.cool/CubeSandbox/CubeSandbox` 镜像拉取。

### 2. 创建沙箱模板

```bash
cubemastercli tpl create-from-image \
  --image cube-sandbox-cn.tencentcloudcr.com/cube-sandbox/sandbox-code:latest \
  --writable-layer-size 2G \
  --expose-port 49999 \
  --expose-port 49983 \
  --probe 49999
```

> **镜像仓库说明：** 国内优先使用 `cube-sandbox-cn.tencentcloudcr.com/...`；境外访问推荐 `cube-sandbox-int.tencentcloudcr.com/...`。
>
> **可写层至少设为 2G** — 默认 1G 安装 Node.js + OpenClaw + 依赖后空间不足。
>
> **模板版本必须匹配：** Cube 会校验模板版本与当前运行的 CubeSandbox 代码版本是否一致。升级 Cube 后必须重新创建模板。

记录命令输出的 `template_id`。

### 3. 配置环境变量

```bash
export CUBE_TEMPLATE_ID="<你的模板ID>"  # 第 2 步获取的模板 ID
export E2B_API_URL="http://127.0.0.1:3000"
export E2B_API_KEY="dummy"
```

如果 Cube API 使用了自签名证书（如通过 mkcert），还需要：

```bash
export SSL_CERT_FILE="/root/.local/share/mkcert/rootCA.pem"
```

### 4. 在 Cube 沙箱中启动 OpenClaw

`sandbox-code` 镜像是 Ubuntu 基础环境，预装了 Python 和 Jupyter。OpenClaw 是一个 **Node.js** 应用——需要先安装 Node.js，再在沙箱内部署 Gateway。

将以下脚本保存为 `run_openclaw.py`，用 `python3 -u run_openclaw.py` 执行：

```python
#!/usr/bin/env python3 -u
import os
import time
import signal
from e2b_code_interpreter import Sandbox

os.environ.setdefault("E2B_API_URL", "http://127.0.0.1:3000")
os.environ.setdefault("E2B_API_KEY", "dummy")

TEMPLATE_ID = os.environ["CUBE_TEMPLATE_ID"]
SSL_CERT_FILE = "/root/.local/share/mkcert/rootCA.pem"
if os.path.exists(SSL_CERT_FILE):
    os.environ["SSL_CERT_FILE"] = SSL_CERT_FILE

running = True

def shutdown(sig, frame):
    global running
    running = False
    print("正在关闭...")

signal.signal(signal.SIGINT, shutdown)
signal.signal(signal.SIGTERM, shutdown)

print(f"正在创建沙箱 (timeout=24h)...")
with Sandbox.create(template=TEMPLATE_ID, timeout=86400) as sb:
    print(f"沙箱 ID: {sb.sandbox_id}")

    # 第 1 步：安装 Node.js 24（OpenClaw 的运行要求）
    print("安装 Node.js 24...")
    sb.commands.run(
        "curl -fsSL https://deb.nodesource.com/setup_24.x | bash - "
        "&& apt-get install -y nodejs",
        user="root",
        timeout=0,
    )

    # 第 2 步：通过 npm 全局安装 OpenClaw
    print("安装 OpenClaw...")
    sb.commands.run("npm install -g openclaw@latest", user="root", timeout=0)

    # 第 3 步：写入最小配置，再用 doctor --fix 自动补全
    print("配置中...")
    sb.commands.run("mkdir -p /root/.openclaw/logs", user="root")
    sb.files.write(
        "/root/.openclaw/openclaw.json",
        '{"agent":{"model":"openai/gpt-4o"}}',
    )
    sb.commands.run(
        "PATH=/usr/local/bin:/usr/bin:/bin openclaw doctor --fix 2>&1 || true",
        user="root",
        timeout=30,
    )

    # 第 4 步：创建启动脚本（避免 nohup 直接调用导致的 SDK 超时）
    sb.files.write(
        "/tmp/start_gateway.sh",
        "#!/bin/bash\n"
        "export PATH=/usr/local/bin:/usr/bin:/bin\n"
        "cd /root\n"
        "exec openclaw gateway --port 18789 --bind loopback "
        "--allow-unconfigured --verbose "
        "> /tmp/openclaw.log 2>&1",
    )
    sb.commands.run("chmod +x /tmp/start_gateway.sh", user="root")

    # 第 5 步：后台启动 Gateway
    print("启动 Gateway...")
    try:
        sb.commands.run(
            "nohup /tmp/start_gateway.sh < /dev/null > /dev/null 2>&1 &",
            user="root",
            timeout=10,
        )
    except Exception:
        pass
    time.sleep(5)

    # 第 6 步：验证 Gateway 是否运行
    result = sb.commands.run(
        "ps ax | grep -v grep | grep openclaw", user="root"
    )
    if "openclaw" in result.stdout:
        print("OpenClaw Gateway 正在运行！")
        print(f"沙箱 ID: {sb.sandbox_id}")
        logs = sb.commands.run("tail -15 /tmp/openclaw.log", user="root")
        print(f"Gateway 日志:\n{logs.stdout}")
        print("保持运行 24h。Ctrl+C 停止。")

        # 健康检查循环
        while running:
            try:
                r = sb.commands.run(
                    "pgrep -x openclaw || echo DEAD", user="root"
                )
                if "DEAD" in r.stdout:
                    print("Gateway 已挂！重启中...")
                    try:
                        sb.commands.run(
                            "nohup /tmp/start_gateway.sh "
                            "< /dev/null > /dev/null 2>&1 &",
                            user="root",
                            timeout=10,
                        )
                    except Exception:
                        pass
                    time.sleep(5)
                    r = sb.commands.run(
                        "pgrep -x openclaw || echo DEAD", user="root"
                    )
                    if "DEAD" in r.stdout:
                        logs = sb.commands.run(
                            "tail -30 /tmp/openclaw.log", user="root"
                        )
                        print(f"重启失败。日志:\n{logs.stdout}")
                        break
                    print("重启成功。")
                time.sleep(30)
            except Exception as e:
                print(f"健康检查错误: {e}")
                break
    else:
        print("Gateway 启动失败。")
        logs = sb.commands.run("cat /tmp/openclaw.log", user="root")
        print(f"日志:\n{logs.stdout}")

print("沙箱已销毁。")
```

> **注意：** 不要粘贴到 `python -i` 交互模式——`with` 块会跨行断掉。请保存为 `.py` 文件再运行。
>
> **`-u` 参数**用于禁用 Python 输出缓冲，确保你能实时看到日志输出。
>
> **为什么用启动脚本？** 直接在 `sb.commands.run()` 中调用 `nohup openclaw gateway ... &` 经常触发 SDK 超时，因为命令无法干净返回。将命令写入脚本文件再通过 nohup 调用可以避免此问题。

## 关键代码片段

### 集成差异 — 原生运行 vs. Cube 沙箱化运行

区别在于你如何提供 Agent 运行时——同样是 OpenClaw，同样是 npm 安装，环境不同：

**之前（原生运行 — 直接在宿主机）：**
```bash
# 直接在笔记本或服务器上 — 拥有完整宿主机访问权限
curl -fsSL https://deb.nodesource.com/setup_24.x | bash -
apt-get install -y nodejs
npm install -g openclaw@latest

mkdir -p ~/.openclaw
echo '{"agent":{"model":"openai/gpt-4o"}}' > ~/.openclaw/openclaw.json

openclaw gateway --port 18789
```

**之后（Cube 沙箱化 — 运行在 MicroVM 内）：**
```python
from e2b_code_interpreter import Sandbox

sb = Sandbox.create(template=os.environ["CUBE_TEMPLATE_ID"], timeout=86400)

# 完全相同的安装命令 — 但执行在沙箱内部而非宿主机
sb.commands.run(
    "curl -fsSL https://deb.nodesource.com/setup_24.x | bash - "
    "&& apt-get install -y nodejs",
    user="root",
    timeout=0,
)
sb.commands.run("npm install -g openclaw@latest", user="root", timeout=0)
sb.commands.run("mkdir -p /root/.openclaw", user="root")
sb.files.write(
    "/root/.openclaw/openclaw.json",
    '{"agent":{"model":"openai/gpt-4o"}}',
)
# ... Agent 现在完全隔离在 KVM MicroVM 中
```

### Gateway 启动 — 正确的配置顺序

在无图形界面的沙箱中启动 OpenClaw Gateway 需要特定的配置步骤，这些是经过反复试错总结出的经验：

```python
# 1. 先写入最小配置（仅包含能通过基本校验的字段）
sb.files.write(
    "/root/.openclaw/openclaw.json",
    '{"agent":{"model":"openai/gpt-4o"}}',
)

# 2. 运行 doctor --fix 自动生成其余字段（gateway.mode、auth 等）
sb.commands.run(
    "PATH=/usr/local/bin:/usr/bin:/bin openclaw doctor --fix 2>&1 || true",
    user="root",
    timeout=30,
)

# 3. 使用 --bind loopback 启动（在无图形界面的沙箱中必须指定，默认的 "auto" 会失败）
#    --allow-unconfigured 跳过未配置频道的检查
sb.commands.run(
    "nohup /tmp/start_gateway.sh < /dev/null > /dev/null 2>&1 &",
    user="root",
    timeout=10,
)
```

关键参数说明：

| 参数 | 为什么需要 |
|------|-----------|
| `--bind loopback` | 在无图形界面的沙箱中，默认的 `auto` 模式会因无法检测到合适的网络接口而拒绝启动。使用 `loopback` 绑定到 `127.0.0.1`。 |
| `--allow-unconfigured` | 不加此参数，OpenClaw 要求所有频道配置完整后才允许启动 Gateway。此参数允许仅配置核心 Agent 就启动。 |
| `openclaw doctor --fix` | 接收最小配置，自动生成所有必需字段（gateway 模式、auth token、频道存根）。写入初始配置后务必执行。 |
| `PATH=/usr/local/bin:/usr/bin:/bin` | npm 全局安装的 bin 可能不在沙箱默认 PATH 中。显式设置 PATH 可避免 `openclaw: command not found`。 |

### 可靠的进程检查

标准的 `ps aux | grep` 在沙箱环境中会失败，因为 shell 命令本身会出现在输出中：

```python
# 错误写法 — "grep" 出现在命令字符串中，结果永远为 True
result = sb.commands.run("ps aux | grep openclaw", user="root")
if "openclaw" in result.stdout:  # grep 命令自身导致的误判

# 正确写法 — 先过滤掉 grep，再判断
result = sb.commands.run(
    "ps ax | grep -v grep | grep openclaw", user="root"
)
if "openclaw" in result.stdout:  # 只匹配真正的 openclaw 进程

# 健康检查：pgrep 配合 fallback 哨兵值
result = sb.commands.run("pgrep -x openclaw || echo DEAD", user="root")
if "DEAD" in result.stdout:
    print("Gateway 已挂，重启中...")
```

### 保持沙箱存活

关键细节：`with Sandbox.create(...) as sb:` 在退出代码块时会销毁沙箱。脚本必须在 `with` 块内停留到你想让沙箱存活的全部时间。

```python
with Sandbox.create(template=TEMPLATE_ID, timeout=86400) as sb:
    # ... 安装和启动 Gateway ...

    # 保持在 with 块内 — 沙箱一直存活
    while running:
        # 定期健康检查，挂了就重启
        time.sleep(30)

# 沙箱在此处销毁 — 只有退出循环后才会执行到这里
print("沙箱已销毁。")
```

### 通过 Agent 发送消息

Gateway 在沙箱内 `127.0.0.1:18789` 启动后即可交互：

```python
sb.commands.run(
    'openclaw agent --message "帮我总结当前系统状态" --thinking high'
)
```

## 进阶配置

### 网络隔离 — 控制 Agent 的网络访问范围

默认情况下 Agent 拥有完整网络访问权限。Cube 可让你精细控制：

```python
# 完全断网模式 — Agent 无任何外部网络访问
sb = Sandbox.create(template=template_id, allow_internet_access=False)

# 白名单模式 — 仅允许访问指定主机（例如仅允许 LLM API 端点）
sb = Sandbox.create(
    template=template_id,
    allow_internet_access=False,
    network={"allow_out": ["api.openai.com/32"]},
)
```

### 超时控制 — 自动销毁 Agent

```python
# Agent 沙箱 1 小时后自动销毁
sb = Sandbox.create(template=template_id, timeout=3600)

# 长期运行的 Agent 可使用 24 小时
sb = Sandbox.create(template=template_id, timeout=86400)
```

### 宿主机目录挂载

将宿主机目录挂载到 Agent 沙箱中，持久化数据：

```python
import json

sb = Sandbox.create(
    template=template_id,
    metadata={
        "host-mount": json.dumps([
            {
                "hostPath": "/data/agent-workspace",
                "mountPath": "/mnt/workspace",
                "readOnly": False,
            },
        ])
    },
)
```

### 暂停与恢复 — 保留 Agent 状态

冻结运行中的 Agent，之后恢复——已安装的包、配置、内存状态完好：

```python
sb = Sandbox.create(template=template_id, timeout=3600)
# ... 安装 Node.js、OpenClaw、配置、启动 Gateway ...

sb.pause()      # 内存快照 — 释放计算资源

# 数小时后，恢复完全相同的状态
sb.connect()    # Agent 从上次中断处继续运行
sb.commands.run("ps ax | grep -v grep | grep openclaw")
```

### 预构建自定义镜像（频繁使用场景）

将 Node.js 和 OpenClaw 打包进自定义 Cube 镜像，省去每次创建沙箱时 2-3 分钟的安装时间：

```Dockerfile
# Dockerfile.cube-openclaw
FROM cube-sandbox-cn.tencentcloudcr.com/cube-sandbox/sandbox-code:latest

RUN curl -fsSL https://deb.nodesource.com/setup_24.x | bash - \
    && apt-get install -y nodejs \
    && npm install -g openclaw@latest

RUN mkdir -p /root/.openclaw
```

构建、推送至镜像仓库后，基于此镜像创建模板——`Sandbox.create()` 使用该模板 ID 即可立即获得一个预装 OpenClaw 的沙箱。

## 注意事项

| 问题 | 解决方案 |
|------|---------|
| **`/usr/bin/env: 'node': No such file or directory`** | `sandbox-code` 镜像不包含 Node.js，需执行 `curl -fsSL https://deb.nodesource.com/setup_24.x \| bash - && apt-get install -y nodejs` 安装。 |
| **`openclaw: command not found`** | npm 全局安装的 bin 可能不在 PATH 中。显式设置 `PATH=/usr/local/bin:/usr/bin:/bin`，或使用 `$(npm root -g)/../bin/openclaw`。 |
| **Gateway 拒绝绑定（`auto` 模式）** | 在无图形界面的沙箱中使用 `--bind loopback` 代替默认的 `auto`。注意该参数只接受命名值（`loopback`、`lan`、`tailnet`、`auto`、`custom`），不接受 IP 地址。 |
| **Gateway 校验失败（"missing gateway.mode"）** | 先写入最小配置 `{"agent":{"model":"..."}}`，再运行 `openclaw doctor --fix` 自动补全所有必需字段。 |
| **`TimeoutException: context deadline exceeded`** | `npm install` 或 `apt-get` 耗时可能超过默认命令超时。添加 `timeout=0` 禁用超时。`nohup ... &` 命令也容易触发此问题——将其包装在启动脚本文件中。 |
| **`Duplicate entry 'CREATE'` 任务错误** | 之前失败的沙箱创建留下了残留任务。等待服务稳定后重试，或重启 Cube 服务。 |
| **模板版本不匹配** | "image version not eq" 表示模板是用旧版 Cube 创建的。升级 Cube 后需重新创建模板。 |
| **磁盘空间不足** | Node.js + OpenClaw + 依赖占用较大空间。创建模板时将 `--writable-layer-size` 至少设为 2G。 |
| **域名解析失败**（`[Errno 8]`） | Cube 内置 DNS 服务。如不可用，可手动写入 `/etc/hosts`。 |
| **SSL 证书错误** | 通过 HTTPS 访问 Cube API 时如使用自签名证书，设置 `SSL_CERT_FILE` 指向 Cube CA 证书路径。 |
| **`python3` 输出为空/被缓冲** | 使用 `python3 -u` 或设置 `PYTHONUNBUFFERED=1` 禁用输出缓冲，实时查看日志。 |
| **`ps aux \| grep` 误判** | shell 命令字符串本身包含搜索关键词。改用 `ps ax \| grep -v grep \| grep <名称>` 或 `pgrep -x <名称>`。 |

## 参考资料

- 示例代码：[`examples/openclaw-integration/`](https://github.com/cube-sandbox/CubeSandbox/tree/main/examples/openclaw-integration)
- Cube Sandbox 部署文档：[cube-sandbox.pages.dev](https://cube-sandbox.pages.dev/zh/)
- OpenClaw 官方文档：[docs.openclaw.ai](https://docs.openclaw.ai)
- OpenClaw GitHub：[github.com/openclaw/openclaw](https://github.com/openclaw/openclaw)
- OpenClaw 入门指南：[docs.openclaw.ai/start/getting-started](https://docs.openclaw.ai/start/getting-started)
