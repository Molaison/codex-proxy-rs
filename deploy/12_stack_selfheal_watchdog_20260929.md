# CPR 栈自愈看门狗（2026-09-29）

## 目标
任何一次 CPR 栈重建/重启、以及 `deploy/config.yaml` 被重写之后，无需人工介入即可自动回到
可用状态；不修改 CPR 源码。

## 故障与判据
CPR 网关容器与三个回环桥接容器（cpa-bridge / chatgpt-web-bridge / sub2api-bridge，
network_mode: service:codex-proxy-rs）共用 netns。网关容器一旦“原地重启”（手动
podman restart，或 compose 的 restart: unless-stopped 在崩溃后拉起），它的 netns 被重建，
桥接容器留在旧 netns，CPR 的上游 127.0.0.1:18093 / 17843 / 18084 全部不可达，
模型请求变成 upstream_unavailable；此时网关 /healthz 仍是 204，**只看 healthz 发现不了**。
（2026-09-29 02:16 与 02:23 两次事故均为该形态。）

第三类故障（2026-10-01 23:37 事故）：`deploy/config.yaml` 被重写时丢掉了安装脚本里的
`podman unshare chown 0:10001`（编辑器另存、`cp -a`、脚本重新生成都会这样），文件在容器
userns 中变成 `root:root 0640`，应用以 10001:10001 读不到它，只打印
`configuration document is invalid: /app/deploy/config.yaml` 就退出。网关端口没有后端，
`/healthz` 返回 000，watchdog 只会反复重启而永不自愈——此时故障在文件属性上，与运行状态无关。

## 实现（运维层，仅标准组件）
- `deploy/12_stack_selfheal_watchdog_20260929.sh`：探网关 healthz + 网关容器内三条回环上游
  （任意 HTTP 响应码即算可达，401 也算通），连续 3 次失败才动作，动作只有一个：
  `systemctl --user restart codex-proxy-rs.service`（podman compose up -d；重建时桥接容器
  一起重建，必然回到新 netns）。栈正在 (重)启动时跳过本轮。
- 探针前先校正 `deploy/config.yaml` 在容器 userns 中的可读性：用
  `podman unshare stat -c '%u %g %a'` 判断 10001:10001 是否可读（不启动容器、不碰栈），
  不可读时按安装脚本执行 `chmod 0640` + `podman unshare chown 0:10001`。栈的重启仍只由
  探针阈值触发，该检查不重启任何单元。
- `~/.config/systemd/user/cpr-stack-watchdog.service` + `.timer`：开机 2 分钟后开始，每 60 秒一次；
  输出 append 到 `.runtime/watchdog.log`。
- 用户 linger 已开启（Linger=yes），用户级单元随开机自启。

## 日常操作
- 重启 CPR 一律用：`systemctl --user restart codex-proxy-rs.service`
  （不要 `podman restart` 网关容器，那正是制造 netns 掉队的原因）
- 重写 `deploy/config.yaml`（编辑器另存、`cp -a`、脚本生成）后立刻恢复容器可读性，
  或在同一分钟内等 watchdog 自愈：
  `cd ~/codex-proxy-rs/deploy && chmod 0640 config.yaml && podman unshare chown 0:10001 config.yaml`
- 看状态：`systemctl --user list-timers cpr-stack-watchdog.timer`
- 看历史：`tail -n 50 ~/codex-proxy-rs/.runtime/watchdog.log`
- 立刻探一次：`systemctl --user start cpr-stack-watchdog.service`

## 已知边界
看门狗只恢复“服务可用性”。重启前已在途的请求仍会按 CPR 既有语义被
`restore_client_admission_startup` 重新占住对应 client key 的并发槽，直到各自 deadline
（当前最长约 40 分钟）才由 `recover_expired` 收敛。要消除这段等待需要改 CPR 源码
（启动时直接收敛旧在途请求），本次未做。

## 证据
- 探针判据：正常返回 200/401/200（cpa/chatgpt-web/sub2api），关闭端口返回 000 判失败。
- 恢复动作 `systemctl --user restart codex-proxy-rs.service` 于 2026-09-29 02:36 实际执行过，
  桥接 netns 与新容器对齐、容器内 18093 恢复 200、真实请求返回 200。
- 未做真实断链演练（会再次打断正在跑的 codex 会话）；如需演练可安排空闲窗口。
- 2026-10-02 00:20 配置可读性事故人工按上述步骤修复（`chmod 0640` + `podman unshare chown
  0:10001`）后，网关 `/healthz` 恢复 204，真实 Codex 请求 gpt-6-astra 与 DeepSeek-V4.1-Flash
  分别返回 `F102_CODEX_OK` / `DS_F102_OK`（13,956 / 14,552 tokens）。
- 2026-10-02 00:25 配置可读性自愈已做故障注入演练：把文件改回 `0:0 640` 后执行
  `systemctl --user start cpr-stack-watchdog.service`，日志记录“不可读 → 重写权限 → 已修复：
  0:10001 640”，随后同轮探针仍为健康、`/healthz` 保持 204。演练前的脚本备份：
  `.runtime/12_stack_selfheal_watchdog_before_config_check_20261002.sh`。
