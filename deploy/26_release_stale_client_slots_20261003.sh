#!/usr/bin/env bash
# CPR 重建后释放残留的 Key 并发槽位（client admission lease）
#
# 解决什么故障
#   Key 的并发槽位是 Redis 里的租约：codex-proxy-rs:client:{sha256(key_ref)}:active，
#   成员是在途 request id，分数是租约到期时间。网关进程重启/重建后，重启前在途的请求已经
#   死了，但它们的槽位要等各自 deadline（最长约 40 分钟）才释放；该 Key 的新请求于是全部
#   卡在排队层（只回 `: keep-alive`），客户端表现为“CPR 死了”。网关 healthz、回环上游探针
#   和看门狗都发现不了这种故障（2026-10-03 16:40–17:15 anyrouter_gpt 事故）。
#
# 做什么
#   1) 只在网关容器自上次检查以来被重建过（StartedAt 变化）时才动作。没重建就直接退出，
#      避免清掉仍在途请求的槽位（否则会短暂超过该 Key 的 max_concurrency）。
#   2) 等网关 healthz 就绪（此时应用启动期的准入恢复已经跑完）再清理。
#   3) 只删 codex-proxy-rs:client:*:active；:requests 是每分钟计数窗口、TTL 很短，不动它。
#
# 由 12_stack_selfheal_watchdog_20260929.sh 每分钟调用；也可手工运行。
# 日志：.runtime/restart-slot-cleanup.log（同时输出到 stdout，看门狗会落到 watchdog.log）
set -uo pipefail

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
PODMAN=/usr/bin/podman
CURL=/usr/bin/curl
CONTAINER=${CPR_SLOT_CLEANUP_CONTAINER:-codex-proxy-rs_codex-proxy-rs_1}
REDIS_CONTAINER=${CPR_SLOT_CLEANUP_REDIS_CONTAINER:-codex-proxy-rs_redis_1}
GATEWAY_URL=${CPR_SLOT_CLEANUP_GATEWAY_URL:-http://127.0.0.1:18082/healthz}
PATTERN=${CPR_SLOT_CLEANUP_PATTERN:-codex-proxy-rs:client:*:active}
# 容器已运行超过该秒数即判定为“不是刚重启”：即使状态文件缺失也不清槽位。
MAX_AGE_SECONDS=${CPR_SLOT_CLEANUP_MAX_AGE_SECONDS:-900}
WAIT_SECONDS=${CPR_SLOT_CLEANUP_WAIT_SECONDS:-180}
STATE=/run/user/$(id -u)/cpr-gateway-started-at
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
LOG=${CPR_SLOT_CLEANUP_LOG:-$SCRIPT_DIR/../.runtime/restart-slot-cleanup.log}

log() {
  local line
  line="$(date '+%F %T') cpr-slot-cleanup: $*"
  printf '%s\n' "$line" >>"$LOG" 2>/dev/null || true
  printf '%s\n' "$line"
}

started_at() { $PODMAN inspect --format '{{.State.StartedAt}}' "$CONTAINER" 2>/dev/null || true; }
# 只取到秒，避免 Go 时间格式的小数位与尾部时区名影响 date 解析。
started_at_epoch() { date -d "${1:0:19}" +%s 2>/dev/null || true; }
healthz_code() { $CURL -s -m 5 -o /dev/null -w '%{http_code}' "$GATEWAY_URL" 2>/dev/null || true; }

redis_password() {
  $PODMAN inspect --format '{{json .Config.Cmd}}' "$REDIS_CONTAINER" 2>/dev/null |
    python3 -c 'import json,sys; cmd=json.load(sys.stdin); print(cmd[cmd.index("--requirepass")+1])'
}

redis_cli() {
  $PODMAN exec -e CPR_REDIS_PASSWORD="$1" "$REDIS_CONTAINER" \
    sh -c 'redis-cli -a "$CPR_REDIS_PASSWORD" --no-auth-warning '"$2" 2>/dev/null
}

clear_stale_client_slots() {
  local password keys key members released=0
  password=$(redis_password)
  if [ -z "${password:-}" ]; then log "取不到 Redis 口令，跳过清理"; return 1; fi
  keys=$(redis_cli "$password" "--scan --pattern '$PATTERN'")
  if [ -z "${keys:-}" ]; then log "没有残留 Key 并发槽位"; return 0; fi
  for key in $keys; do
    members=$(redis_cli "$password" "ZCARD '$key'")
    redis_cli "$password" "DEL '$key'" >/dev/null
    log "释放 $key（成员 ${members:-0} 个）"
    released=$((released + 1))
  done
  log "清理完成：释放 ${released} 个 Key 的并发槽位"
}

main() {
  local current previous age now code waited
  current=$(started_at)
  if [ -z "${current:-}" ]; then log "取不到网关容器 $CONTAINER 的启动时间，跳过"; exit 0; fi
  previous=$(cat "$STATE" 2>/dev/null || true)
  [ "$current" = "$previous" ] && exit 0
  now=$(date +%s)
  age=$(( now - $(started_at_epoch "$current") ))
  printf '%s\n' "$current" >"$STATE" 2>/dev/null || true
  if [ "$age" -gt "$MAX_AGE_SECONDS" ]; then
    log "网关容器启动于 ${age}s 前（> ${MAX_AGE_SECONDS}s），不是刚重建，只记录状态"
    exit 0
  fi
  log "检测到网关容器重建（启动于 ${age}s 前），等待 healthz 后清理残留槽位"
  waited=0
  while [ "$waited" -lt "$WAIT_SECONDS" ]; do
    code=$(healthz_code)
    case "$code" in 2??) break ;; esac
    sleep 5
    waited=$((waited + 5))
  done
  case "${code:-000}" in
    2??) ;;
    *) log "等待 healthz 超时（最后状态码 ${code:-000}），本轮不清理"; exit 0 ;;
  esac
  clear_stale_client_slots || true
}

main "$@"
