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
#   3) 只对数据库证明 started_at 早于这次网关启动的成员做 ZREM。
#      新请求、尚未落库的准入成员保留；不删除整个 active 集合，不动 RPM 窗口。
#
# 由 12_stack_selfheal_watchdog_20260929.sh 每分钟调用；也可手工运行。
# 日志：.runtime/restart-slot-cleanup.log（同时输出到 stdout，看门狗会落到 watchdog.log）
set -uo pipefail

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
PODMAN=/usr/bin/podman
CURL=/usr/bin/curl
CONTAINER=${CPR_SLOT_CLEANUP_CONTAINER:-codex-proxy-rs_codex-proxy-rs_1}
REDIS_CONTAINER=${CPR_SLOT_CLEANUP_REDIS_CONTAINER:-codex-proxy-rs_redis_1}
POSTGRES_CONTAINER=${CPR_SLOT_CLEANUP_POSTGRES_CONTAINER:-codex-proxy-rs_postgres_1}
GATEWAY_URL=${CPR_SLOT_CLEANUP_GATEWAY_URL:-http://127.0.0.1:18082/healthz}
PATTERN=${CPR_SLOT_CLEANUP_PATTERN:-codex-proxy-rs:client:*:active}
# 容器已运行超过该秒数即判定为“不是刚重启”：即使状态文件缺失也不清槽位。
MAX_AGE_SECONDS=${CPR_SLOT_CLEANUP_MAX_AGE_SECONDS:-900}
WAIT_SECONDS=${CPR_SLOT_CLEANUP_WAIT_SECONDS:-180}
STATE=${CPR_SLOT_CLEANUP_STATE:-/run/user/$(id -u)/cpr-gateway-started-at}
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
LOG=${CPR_SLOT_CLEANUP_LOG:-$SCRIPT_DIR/../.runtime/restart-slot-cleanup.log}

log() {
  local line
  line="$(date '+%F %T') cpr-slot-cleanup: $*"
  printf '%s\n' "$line" >>"$LOG" 2>/dev/null || true
  printf '%s\n' "$line"
}

started_at() {
  $PODMAN inspect "$CONTAINER" 2>/dev/null |
    python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["State"]["StartedAt"])' 2>/dev/null || true
}
# 保留时间偏移；Podman StartedAt 与数据库都使用带时区时间。
started_at_epoch() { date -d "$1" +%s 2>/dev/null; }
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
  local current=$1 password keys key members stale member removed released=0
  password=$(redis_password) || return 1
  if [ -z "${password:-}" ]; then log "取不到 Redis 口令，跳过清理"; return 1; fi
  keys=$(redis_cli "$password" "--scan --pattern '$PATTERN'") || return 1
  if [ -z "${keys:-}" ]; then log "没有残留 Key 并发槽位"; return 0; fi
  for key in $keys; do
    members=$(redis_cli "$password" "ZRANGE '$key' 0 -1") || return 1
    [ -n "$members" ] || continue
    stale=$($PODMAN exec -i "$POSTGRES_CONTAINER" sh -c '
      PGPASSWORD="$POSTGRES_PASSWORD" exec psql -XAt -v ON_ERROR_STOP=1 \
        -U "$POSTGRES_USER" -d "$POSTGRES_DB" --set=members="$1" --set=started="$2"
    ' sh "$members" "$current" <<'SQL'
SELECT id FROM model_requests
WHERE started_at < :'started'::timestamptz
  AND id = ANY(string_to_array(:'members', E'\n'));
SQL
    ) || return 1
    # 清理期间若再次重建，留给下一次检查按新启动边界处理。
    if [ "$(started_at)" != "$current" ]; then log "启动边界已变化，停止本轮清理"; return 1; fi
    for member in $stale; do
      removed=$(redis_cli "$password" "ZREM '$key' '$member'") || return 1
      released=$((released + removed))
    done
  done
  log "清理完成：释放 ${released} 个重启前请求的槽位；新请求及未确认成员保留"
}

main() {
  local current previous age now code waited
  current=$(started_at)
  if [ -z "${current:-}" ]; then log "取不到网关容器 $CONTAINER 的启动时间，跳过"; exit 0; fi
  previous=$(cat "$STATE" 2>/dev/null || true)
  [ "$current" = "$previous" ] && exit 0
  now=$(date +%s)
  local epoch
  epoch=$(started_at_epoch "$current") || { log "启动时间无法解析，未清理"; exit 1; }
  age=$(( now - epoch ))
  if [ "$age" -gt "$MAX_AGE_SECONDS" ]; then
    log "网关容器启动于 ${age}s 前（> ${MAX_AGE_SECONDS}s），不是刚重建，只记录状态"
    printf '%s\n' "$current" >"$STATE"
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
  if clear_stale_client_slots "$current"; then
    printf '%s\n' "$current" >"$STATE"
  else
    log "清理失败，未推进启动标记；下次检查重试"
    exit 1
  fi
}

main "$@"
