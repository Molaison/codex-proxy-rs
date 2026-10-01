#!/usr/bin/env bash
# CPR 栈自愈看门狗 —— 由用户级 systemd timer（cpr-stack-watchdog.timer）周期执行
#
# 解决什么故障
#   CPR 网关容器与它的回环桥接容器（cpa-bridge / chatgpt-web-bridge / sub2api-bridge，
#   都是 network_mode: service:codex-proxy-rs）共用网络命名空间。只要网关容器被“原地重启”
#   （手动 podman restart，或 compose 的 restart: unless-stopped 在崩溃后拉起），它的 netns
#   会被重建，而桥接容器仍留在旧 netns —— 于是 CPR 的回环上游
#   127.0.0.1:18093 / 17843 / 18084 全部不可达，所有模型请求变成 upstream_unavailable。
#   这种故障下网关自己的 /healthz 仍是 204，所以只看 healthz 发现不了。
#
#   同一故障还会打掉 compose 之外的 systemd 桥接（chatgpt-web-pool-bridge、
#   chatgpt-web-cpr-bridge-{1,2}）：它们同样以 --network container:网关 运行，但不受
#   codex-proxy-rs.service 管辖，栈重建不会重建它们。它们失联时对应回环端口在网关容器
#   内直接消失（127.0.0.1:17865 / 17861 / 17862），ChatGPT Web 路由随即返回
#   upstream_unavailable。2026-10-01 网关重建后 17865 就这样失联，而 compose 三条桥接
#   仍然正常，所以只看那三条探针发现不了。
#
#   第三类故障（2026-10-01 23:37 事故）：deploy/config.yaml 被重写时丢掉了安装脚本里的
#   `podman unshare chown 0:10001`（编辑器另存、cp -a、脚本重新生成都会这样），文件在
#   容器 userns 中变成 root:root 0640，应用以 10001:10001 读不到它，只打印
#   `configuration document is invalid: /app/deploy/config.yaml` 就退出。网关端口没有
#   后端，/healthz 变成 000，watchdog 只会反复重启而永不自愈。判据与运行状态无关，
#   只有文件属性可修，所以必须在探针前先校正。
#
# 做什么（标准做法：探针 + 编排动作）
#   0) 先校正 deploy/config.yaml 在容器 userns 中的可读性（只改文件属性，不碰栈）
#   1) 探网关 http://127.0.0.1:18082/healthz
#   2) 逐条比对 systemd 桥接与网关容器的 netns inode，掉队者只重启该桥接单元
#      （精确、低影响、不依赖上游健康；重启会切断该桥接在途连接，此时它们本来就已失联）
#   3) 探网关容器内的三条 compose 回环上游（有任意 HTTP 响应即视为可达，401 也算通）
#   4) 连续 FAIL_THRESHOLD 次失败才动手，避免单次抖动触发重启
#   5) compose 上游的修复动作是部署里既有的入口：systemctl --user restart codex-proxy-rs.service
#      （即 podman compose up -d；重建时 compose 桥接会一起重建，必然回到新 netns）
#      重建后 systemd 桥接会再次掉队，因此恢复动作会再跑一次 netns 比对与新 netns 归位
#
# 手工使用
#   systemctl --user start cpr-stack-watchdog.service      # 立刻探一次
#   journalctl --user -u cpr-stack-watchdog.service -n 50  # 看历史
#   systemctl --user list-timers cpr-stack-watchdog.timer  # 看下一次
#
set -uo pipefail

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
PODMAN=/usr/bin/podman
CURL=/usr/bin/curl
STACK_UNIT=codex-proxy-rs.service
GATEWAY_URL=${CPR_WATCHDOG_GATEWAY_URL:-http://127.0.0.1:18082/healthz}
CPR_CONTAINER=codex-proxy-rs_codex-proxy-rs_1
UPSTREAM_PORTS=(${CPR_WATCHDOG_UPSTREAM_PORTS:-18093 17843 18084})
# systemd 管理的 netns 桥接："单元名:容器名"，容器名取自各单元 ExecStart 的 --name。
NETNS_BRIDGES=(${CPR_WATCHDOG_NETNS_BRIDGES:-chatgpt-web-pool-bridge.service:chatgpt-web-pool-bridge chatgpt-web-cpr-bridge-1.service:chatgpt-web-local-1-bridge chatgpt-web-cpr-bridge-2.service:chatgpt-web-local-2-bridge})
FAIL_THRESHOLD=${CPR_WATCHDOG_FAIL_THRESHOLD:-3}
STATE=/run/user/$(id -u)/cpr-stack-watchdog.failures
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# 网关应用在容器内以 10001:10001 运行，bind mount 保留宿主机 UID/GID，
# 因此这里比对的是文件在容器 userns 中的属主与权限，而不是宿主机上的值。
CONFIG_FILE=${CPR_WATCHDOG_CONFIG_FILE:-$SCRIPT_DIR/config.yaml}
APP_UID=${CPR_WATCHDOG_APP_UID:-10001}
APP_GID=${CPR_WATCHDOG_APP_GID:-10001}

log() { printf '%s cpr-watchdog: %s\n' "$(date '+%F %T')" "$*"; }
http_code() { $CURL -s -m 5 -o /dev/null -w '%{http_code}' "$1" 2>/dev/null || true; }

netns_of_pid() { readlink "/proc/$1/ns/net" 2>/dev/null || true; }

# 比对 systemd 桥接与网关容器的 netns inode；掉队的只重启该桥接单元。
# 网关容器 netns 被重建后，桥接若仍指向旧 inode，它监听的端口在网关内不再存在。
heal_netns_bridges() {
  local gateway_pid gateway_ns entry unit container bridge_pid bridge_ns healed=0
  gateway_pid=$($PODMAN inspect --format '{{.State.Pid}}' "$CPR_CONTAINER" 2>/dev/null) || true
  gateway_ns=$(netns_of_pid "${gateway_pid:-0}")
  if [ -z "$gateway_ns" ] || [ "$gateway_ns" = "/proc/0/ns/net" ]; then
    log "网关容器 ${CPR_CONTAINER} 不可用，跳过 systemd 桥接 netns 比对"
    return 0
  fi
  for entry in "${NETNS_BRIDGES[@]}"; do
    unit=${entry%%:*}
    container=${entry#*:}
    if [ "$(systemctl --user is-active "$unit" 2>/dev/null)" != "active" ]; then
      continue
    fi
    bridge_pid=$($PODMAN inspect --format '{{.State.Pid}}' "$container" 2>/dev/null) || true
    bridge_ns=$(netns_of_pid "${bridge_pid:-0}")
    if [ -z "$bridge_ns" ] || [ "$bridge_ns" = "/proc/0/ns/net" ]; then
      log "桥接容器 $container 不在运行，重启 $unit"
    elif [ "$bridge_ns" = "$gateway_ns" ]; then
      continue
    else
      log "systemd 桥接 $container 掉队（${bridge_ns} != ${gateway_ns}），重启 $unit"
    fi
    if systemctl --user restart "$unit"; then
      sleep 2
      bridge_pid=$($PODMAN inspect --format '{{.State.Pid}}' "$container" 2>/dev/null) || true
      bridge_ns=$(netns_of_pid "${bridge_pid:-0}")
      if [ "$bridge_ns" = "$gateway_ns" ]; then
        healed=$((healed + 1))
        log "$unit 已归位到网关 netns"
      else
        log "$unit 重启后仍未归位（${bridge_ns:-unknown}）"
      fi
    else
      log "$unit 重启失败"
    fi
  done
  [ "$healed" -gt 0 ] && log "本轮修复 $healed 个掉队桥接"
  return 0
}

# 按 userns 中看到的 mode 位判断应用能否读该文件；不启动容器。
config_readable_by_app() {
  local owner=$1 group=$2 mode=$3 bits
  bits=$((8#$mode))
  [ $((bits & 4)) -ne 0 ] && return 0
  [ "$owner" = "$APP_UID" ] && [ $((bits & 256)) -ne 0 ] && return 0
  [ "$group" = "$APP_GID" ] && [ $((bits & 32)) -ne 0 ] && return 0
  return 1
}

# 第三类故障的自愈：config.yaml 被重写后容器内变成 root:root 0640，应用读不到配置就退出，
# 反复重启栈也无法恢复。这里把属主与权限改回安装脚本要求的状态（0640 + 0:10001）。
# 栈本身的重启仍只由探针阈值触发，本函数不重启任何单元。
ensure_config_readable() {
  local owner group mode
  read -r owner group mode < <($PODMAN unshare stat -c '%u %g %a' "$CONFIG_FILE" 2>/dev/null)
  if [ -z "${owner:-}" ] || [ -z "${group:-}" ] || [ -z "${mode:-}" ]; then
    log "取不到 $CONFIG_FILE 在容器 userns 中的属主与权限，跳过配置可读性检查"
    return 0
  fi
  if config_readable_by_app "$owner" "$group" "$mode"; then
    return 0
  fi
  log "配置 $CONFIG_FILE 在容器内为 ${owner}:${group} ${mode}，应用 ${APP_UID}:${APP_GID} 不可读，按安装脚本重写权限"
  chmod 0640 "$CONFIG_FILE"
  $PODMAN unshare chown "0:$APP_GID" "$CONFIG_FILE"
  read -r owner group mode < <($PODMAN unshare stat -c '%u %g %a' "$CONFIG_FILE" 2>/dev/null)
  if config_readable_by_app "${owner:-}" "${group:-}" "${mode:-}"; then
    log "配置可读性已修复：${owner}:${group} ${mode}（网关若仍不可达，由探针按既有流程重启栈）"
  else
    log "配置可读性修复后仍不可读：${owner:-?}:${group:-?} ${mode:-?}（需人工介入）"
  fi
  return 0
}

probe() {
  reason=""
  local code
  code=$(http_code "$GATEWAY_URL")
  if [ "$code" = "000" ]; then reason="网关 $GATEWAY_URL 不可达"; return 1; fi
  for port in "${UPSTREAM_PORTS[@]}"; do
    code=$($PODMAN exec "$CPR_CONTAINER" $CURL -s -m 5 -o /dev/null -w '%{http_code}' \
             "http://127.0.0.1:$port/healthz" 2>/dev/null || true)
    if [ "$code" = "000" ]; then
      reason="网关容器内回环上游 127.0.0.1:$port 不可达（桥接 netns 掉队？）"
      return 1
    fi
  done
  return 0
}

if [ "$(systemctl --user is-active "$STACK_UNIT" 2>/dev/null)" = "activating" ] \
   || pgrep -f "podman compose .*compose\.yaml up" >/dev/null 2>&1; then
  log "栈正在(重)启动，跳过本轮"
  exit 0
fi

ensure_config_readable
heal_netns_bridges

if probe; then
  rm -f "$STATE"
  log "健康：网关 + ${#UPSTREAM_PORTS[@]} 条回环上游均可达"
  exit 0
fi

failures=$(cat "$STATE" 2>/dev/null || echo 0)
failures=$((failures + 1))
printf '%s\n' "$failures" > "$STATE"
log "探针失败($failures/$FAIL_THRESHOLD)：$reason"
if [ "$failures" -lt "$FAIL_THRESHOLD" ]; then exit 0; fi

rm -f "$STATE"
log "连续失败达阈值，执行恢复：systemctl --user restart $STACK_UNIT"
systemctl --user restart "$STACK_UNIT"
sleep 15
heal_netns_bridges
if probe; then
  log "恢复成功"
else
  log "恢复后仍失败：$reason（需人工介入）"
  exit 1
fi
