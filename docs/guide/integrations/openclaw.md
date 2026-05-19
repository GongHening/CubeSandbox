---
title: OpenClaw Integration Guide
author: GongHening
date: 2026-05-19
tags:
  - integration
  - openclaw
lang: en-US
---

# OpenClaw Integration Guide

## Integration Target and Version

- **Framework:** [OpenClaw](https://github.com/openclaw/openclaw) — personal AI assistant you run on your own devices, with multi-channel support (WeChat, QQ, WhatsApp, Telegram, Discord, etc.)
- **Cube Sandbox:** any deployment (single-node, multi-node cluster)
- **Runtime:** Node.js 24 (recommended) or 22.16+; Python 3.8+ with `e2b-code-interpreter`
- **Integration type:** Run your agent inside Cube — the OpenClaw Gateway runs directly inside a Cube MicroVM, giving you kernel-level isolation for the entire agent runtime

## Architecture

```
User → Messaging channels (WeChat/QQ/WhatsApp/Telegram/Discord/...)
          │
          ▼
Cube MicroVM (KVM-isolated)
    │
    ├── Node.js 24 + OpenClaw Gateway (agent runtime)
    │       │
    │       ├── LLM API calls (to external model providers)
    │       ├── Skills & tools (execute natively inside the sandbox)
    │       ├── Multi-channel connections (managed by the gateway)
    │       └── File I/O, package installs, shell commands
    │
    └── sandbox-code image (Ubuntu + Python + Jupyter)
```

Key benefits of running OpenClaw inside Cube:

- **Kernel-level isolation** — the agent process, its tools, and all executed code are contained in a dedicated KVM MicroVM
- **Ephemeral by default** — start from a clean snapshot every session, auto-destroyed on exit
- **Reproducible** — the agent always runs in the same known-good environment
- **Network control** — decide whether the agent can reach the internet, and which hosts
- **Pause & resume** — freeze the entire agent mid-session and restore it later, in-memory state intact

## Prerequisites

- A running Cube Sandbox deployment ([Quick Start](https://cube-sandbox.pages.dev/guide/quickstart), [One-Click Deploy](https://cube-sandbox.pages.dev/guide/one-click-deploy), or [Multi-Node Cluster](https://cube-sandbox.pages.dev/guide/multi-node-deploy))
- Python 3.8+ with `e2b-code-interpreter` (for managing the sandbox from outside)

```bash
pip install e2b-code-interpreter
```

## Integration Steps

### 1. Deploy Cube Sandbox

Follow one of the deployment guides above. After deployment you should have:

- **Cube API Server** accessible at `<node-ip>:3000` (HTTP)
- **CubeProxy** running on ports 80 (HTTP) and 443 (HTTPS)

> **China mirror:** If deploying inside mainland China where GitHub is unreachable, use `MIRROR=cn` with the one-click script to pull from `cnb.cool/CubeSandbox/CubeSandbox`.

### 2. Create a Sandbox Template

```bash
cubemastercli tpl create-from-image \
  --image cube-sandbox-int.tencentcloudcr.com/cube-sandbox/sandbox-code:latest \
  --writable-layer-size 2G \
  --expose-port 49999 \
  --expose-port 49983 \
  --probe 49999
```

> **Image registry:** Use `cube-sandbox-int.tencentcloudcr.com/...` for international access; `cube-sandbox-cn.tencentcloudcr.com/...` for mainland China.
>
> **Writable layer must be at least 2G** — the default 1G is insufficient after installing Node.js + OpenClaw + dependencies.
>
> **Template version matters:** Cube enforces that template versions match the running CubeSandbox code version. If you upgrade Cube, you must recreate your templates.

Copy the `template_id` from the command output.

### 3. Configure Environment Variables

```bash
export CUBE_TEMPLATE_ID="<your-template-id>"  # from step 2
export E2B_API_URL="http://127.0.0.1:3000"
export E2B_API_KEY="dummy"
```

If your Cube API uses a self-signed certificate (e.g., via mkcert), also set:

```bash
export SSL_CERT_FILE="/root/.local/share/mkcert/rootCA.pem"
```

### 4. Launch OpenClaw Inside a Cube Sandbox

The `sandbox-code` image is Ubuntu-based with Python and Jupyter pre-installed. OpenClaw is a **Node.js** application — we need to install Node.js first, then configure and start the gateway inside the sandbox.

Save the following script as `run_openclaw.py` and execute it with `python3 -u run_openclaw.py`:

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
    print("Shutting down...")

signal.signal(signal.SIGINT, shutdown)
signal.signal(signal.SIGTERM, shutdown)

print(f"Creating sandbox (timeout=24h)...")
with Sandbox.create(template=TEMPLATE_ID, timeout=86400) as sb:
    print(f"Sandbox: {sb.sandbox_id}")

    # Step 1: Install Node.js 24 (OpenClaw requirement)
    print("Installing Node.js 24...")
    sb.commands.run(
        "curl -fsSL https://deb.nodesource.com/setup_24.x | bash - "
        "&& apt-get install -y nodejs",
        user="root",
        timeout=0,
    )

    # Step 2: Install OpenClaw globally via npm
    print("Installing OpenClaw...")
    sb.commands.run("npm install -g openclaw@latest", user="root", timeout=0)

    # Step 3: Write a minimal config and let doctor --fix expand it
    print("Configuring...")
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

    # Step 4: Create a startup script (avoids nohup SDK timeout issues)
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

    # Step 5: Start the gateway in background
    print("Starting Gateway...")
    try:
        sb.commands.run(
            "nohup /tmp/start_gateway.sh < /dev/null > /dev/null 2>&1 &",
            user="root",
            timeout=10,
        )
    except Exception:
        pass
    time.sleep(5)

    # Step 6: Verify the gateway is running
    result = sb.commands.run(
        "ps ax | grep -v grep | grep openclaw", user="root"
    )
    if "openclaw" in result.stdout:
        print("OpenClaw Gateway is running!")
        print(f"Sandbox ID: {sb.sandbox_id}")
        logs = sb.commands.run("tail -15 /tmp/openclaw.log", user="root")
        print(f"Gateway logs:\n{logs.stdout}")
        print("Keeping alive for 24h. Ctrl+C to stop.")

        # Health-check loop
        while running:
            try:
                r = sb.commands.run(
                    "pgrep -x openclaw || echo DEAD", user="root"
                )
                if "DEAD" in r.stdout:
                    print("Gateway died! Restarting...")
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
                        print(f"Restart failed. Logs:\n{logs.stdout}")
                        break
                    print("Restarted successfully.")
                time.sleep(30)
            except Exception as e:
                print(f"Health check error: {e}")
                break
    else:
        print("Gateway failed to start.")
        logs = sb.commands.run("cat /tmp/openclaw.log", user="root")
        print(f"Logs:\n{logs.stdout}")

print("Sandbox destroyed.")
```

> **Important:** Do NOT paste this into `python -i` — the `with` block will break across lines. Save as a `.py` file and run it.
>
> **The `-u` flag** on the shebang line disables Python output buffering so you see logs in real time.
>
> **Why a startup script?** Calling `nohup openclaw gateway ... &` directly in `sb.commands.run()` often triggers an SDK timeout because the command doesn't return cleanly. Wrapping it in a script file + nohup avoids this.

## Key Code Snippets

### Integration Diff — Native vs. Cube-Sandboxed Agent

The difference between running OpenClaw natively and running it inside Cube is how you provision the runtime — same OpenClaw, same npm install, different environment:

**Before (native — agent runs on your host):**
```bash
# Directly on your laptop or server — full host access
curl -fsSL https://deb.nodesource.com/setup_24.x | bash -
apt-get install -y nodejs
npm install -g openclaw@latest

mkdir -p ~/.openclaw
echo '{"agent":{"model":"openai/gpt-4o"}}' > ~/.openclaw/openclaw.json

openclaw gateway --port 18789
```

**After (Cube-sandboxed — agent runs inside a MicroVM):**
```python
from e2b_code_interpreter import Sandbox

sb = Sandbox.create(template=os.environ["CUBE_TEMPLATE_ID"], timeout=86400)

# Same setup commands — executed inside the sandbox instead of on the host
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
# ... agent now fully isolated in a KVM MicroVM
```

### Gateway Startup — The Correct Config Sequence

Getting OpenClaw Gateway to start in a headless sandbox requires a specific sequence. These were discovered through repeated trial and error:

```python
# 1. Write a minimal config first (bare minimum that passes validation)
sb.files.write(
    "/root/.openclaw/openclaw.json",
    '{"agent":{"model":"openai/gpt-4o"}}',
)

# 2. Run doctor --fix to auto-generate the rest (gateway.mode, auth, etc.)
sb.commands.run(
    "PATH=/usr/local/bin:/usr/bin:/bin openclaw doctor --fix 2>&1 || true",
    user="root",
    timeout=30,
)

# 3. Start with --bind loopback (required in headless sandbox; "auto" fails)
#    and --allow-unconfigured (bypasses default config checks)
sb.commands.run(
    "nohup /tmp/start_gateway.sh < /dev/null > /dev/null 2>&1 &",
    user="root",
    timeout=10,
)
```

Key flags explained:

| Flag | Why it's needed |
|------|-----------------|
| `--bind loopback` | In a headless sandbox, the default `auto` bind mode refuses to start because it can't detect a suitable network interface. Use `loopback` to bind to `127.0.0.1`. |
| `--allow-unconfigured` | Without this, OpenClaw requires fully configured channels before starting the gateway. This flag lets the gateway start with just the core agent config. |
| `openclaw doctor --fix` | Takes a minimal config and auto-generates all required fields (gateway mode, auth token, channel stubs). Always run this after writing the initial config. |
| `PATH=/usr/local/bin:/usr/bin:/bin` | npm global installs may not be on the default PATH inside the sandbox. Explicitly setting PATH avoids `openclaw: command not found`. |

### Reliable Process Checking

Standard `ps aux | grep` patterns fail in sandbox environments because the shell command itself appears in the output:

```python
# WRONG — "grep" appears in the command string, so this is always True
result = sb.commands.run("ps aux | grep openclaw", user="root")
if "openclaw" in result.stdout:  # False positive from the grep command itself

# RIGHT — filter out grep, then check
result = sb.commands.run(
    "ps ax | grep -v grep | grep openclaw", user="root"
)
if "openclaw" in result.stdout:  # Only matches the actual process

# Health check: pgrep with a fallback sentinel
result = sb.commands.run("pgrep -x openclaw || echo DEAD", user="root")
if "DEAD" in result.stdout:
    print("Gateway is down, restarting...")
```

### Keeping the Sandbox Alive

A critical detail: `with Sandbox.create(...) as sb:` destroys the sandbox when the block exits. Your script must stay inside the `with` block for the entire time you want the sandbox to live.

```python
with Sandbox.create(template=TEMPLATE_ID, timeout=86400) as sb:
    # ... setup and start gateway ...

    # Stay in the with block — keep the sandbox alive
    while running:
        # periodic health checks, restart if needed
        time.sleep(30)

# Sandbox destroyed here — only after the loop exits
print("Sandbox destroyed.")
```

### Send a Message Through the Agent

Once the gateway is running on `127.0.0.1:18789` inside the sandbox:

```python
sb.commands.run(
    'openclaw agent --message "Summarize the current system status" --thinking high'
)
```

## Going Further

### Network Isolation — Control What the Agent Can Reach

By default the agent has full internet access. Lock it down:

```python
# Air-gap mode — agent can't reach any external network
sb = Sandbox.create(template=template_id, allow_internet_access=False)

# Allowlist — only permit specific hosts (e.g., your LLM API endpoint)
sb = Sandbox.create(
    template=template_id,
    allow_internet_access=False,
    network={"allow_out": ["api.openai.com/32"]},
)
```

### Timeout — Auto-Destroy the Agent

```python
# Agent sandbox self-destructs after 1 hour
sb = Sandbox.create(template=template_id, timeout=3600)

# For long-running agents, use 24 hours
sb = Sandbox.create(template=template_id, timeout=86400)
```

### Host Directory Mounts

Mount host directories into the agent's sandbox for persistent data:

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

### Pause and Resume — Preserve Agent State

Freeze the running agent and restore it later — installed packages, config, and in-memory state are preserved:

```python
sb = Sandbox.create(template=template_id, timeout=3600)
# ... install Node.js, OpenClaw, configure, start gateway ...

sb.pause()      # memory snapshot — compute resources released

# Hours later, restore the exact same state
sb.connect()    # agent resumes right where it left off
sb.commands.run("ps ax | grep -v grep | grep openclaw")
```

### Pre-Build a Custom Image (for Frequent Use)

Bake Node.js and OpenClaw into a custom Cube image to eliminate the 2–3 minute setup time on every sandbox:

```Dockerfile
# Dockerfile.cube-openclaw
FROM cube-sandbox-int.tencentcloudcr.com/cube-sandbox/sandbox-code:latest

RUN curl -fsSL https://deb.nodesource.com/setup_24.x | bash - \
    && apt-get install -y nodejs \
    && npm install -g openclaw@latest

RUN mkdir -p /root/.openclaw
```

Build, push to your registry, and create a template from the image — `Sandbox.create()` with that template ID starts instantly with OpenClaw pre-installed.

## Caveats

| Issue | Mitigation |
|-------|------------|
| **`/usr/bin/env: 'node': No such file or directory`** | The `sandbox-code` image does not include Node.js. Install it via `curl -fsSL https://deb.nodesource.com/setup_24.x \| bash - && apt-get install -y nodejs`. |
| **`openclaw: command not found`** | npm global bin may not be on PATH. Set `PATH=/usr/local/bin:/usr/bin:/bin` explicitly, or use `$(npm root -g)/../bin/openclaw`. |
| **Gateway refuses to bind (`auto` mode)** | In a headless sandbox, use `--bind loopback` instead of the default `auto`. Do NOT pass an IP address — the flag only accepts named values (`loopback`, `lan`, `tailnet`, `auto`, `custom`). |
| **Gateway fails validation ("missing gateway.mode")** | Write a minimal config first (`{"agent":{"model":"..."}}`), then run `openclaw doctor --fix` to auto-populate all required fields. |
| **`TimeoutException: context deadline exceeded`** | `npm install` or `apt-get` can take minutes. Add `timeout=0` to disable the timeout for long-running commands. Also applies to `nohup ... &` commands — wrap them in a startup script file. |
| **`Duplicate entry 'CREATE'` job error** | A previously failed sandbox creation left a stale job. Wait for services to settle, or restart Cube services. |
| **Template version mismatch** | "image version not eq" means your template was created with an older Cube version. Recreate the template after upgrading Cube. |
| **Disk space exhausted** | Node.js + OpenClaw + dependencies need significant space. Use `--writable-layer-size 2G` (or larger) when creating the template. |
| **DNS resolution fails** (`[Errno 8]`) | Cube has built-in DNS. If unavailable, add entries to `/etc/hosts` manually. |
| **SSL certificate errors** | When accessing Cube API over HTTPS with a self-signed cert, set `SSL_CERT_FILE` to the Cube CA cert path. |
| **`python3` output is empty / buffered** | Use `python3 -u` or set `PYTHONUNBUFFERED=1` to disable output buffering and see logs in real time. |
| **`ps aux \| grep` false positives** | The shell command string itself contains the search term. Use `ps ax \| grep -v grep \| grep <name>` or `pgrep -x <name>` instead. |

## References

- Example code: [`examples/openclaw-integration/`](https://github.com/cube-sandbox/CubeSandbox/tree/main/examples/openclaw-integration)
- Cube Sandbox deployment: [cube-sandbox.pages.dev](https://cube-sandbox.pages.dev)
- OpenClaw docs: [docs.openclaw.ai](https://docs.openclaw.ai)
- OpenClaw GitHub: [github.com/openclaw/openclaw](https://github.com/openclaw/openclaw)
- OpenClaw Getting Started: [docs.openclaw.ai/start/getting-started](https://docs.openclaw.ai/start/getting-started)
