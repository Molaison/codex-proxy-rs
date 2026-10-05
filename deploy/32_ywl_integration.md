# ywl 部署集成边界

CPR 镜像只更新网关与前端，不包含独立 Web QA、Full、Pool、MCP 或浏览器服务。

- 宿主继续使用已部署的 `12_stack_selfheal_watchdog_20260929.sh` 和 `26_release_stale_client_slots_20261003.sh`。本目录保留现役脚本，不在构建或候选启动时执行它们。
- 配置文件保持容器 userns 所需的 10001 组读权限。网关切换由唯一运维 owner 沿现役容器定义执行，不运行整栈 compose 重建数据库。
- 生产数据库保留实时数据和最新账号/Key配置；不可用隔离验收数据库覆盖。ywl 迁移20/21沿用历史，新迁移追加22。
- Web PR5 的原名纯聊天与 `-tools` 工具路线、两种元数据、两个工具Key的QA授权必须保留。代码合入 Web main `f79e5fc`；部分已部署 QA/项目/论文实现仍在独立工作树和任务补丁内，不可用干净 Web main 整树覆盖现役QA。
- `/v1/live` 路由应随旧版保留，但原账号被上游拒绝语音会话，恢复路由不代表语音收发验收通过。
- 密文、EOF及其他已通过单位不作为新故障重复重试；新变更只验证受影响边界。

统一验收状态与具体镜像以管理项目 `12_upstream_acceptance` 的 README 和 integration.json 为准。

## Full 网络桥接遗漏

现役 watchdog 的网络空间桥接表不含后来新增的 `chatgpt-web-full-bridge-1/2`。本候选已补入，但未部署宿主脚本。切流前须同步此脚本；网关切换后仅重建这两个桥接服务以跟随新 netns，不重启 Full provider、MCP 或浏览器。

隔离候选使用独立 `cpr-upstream30-full-bridge-1/2`，在宿主回环17971/17972转发既有鉴权Unix socket。它们不替换生产桥接；结束隔离验收后由owner按记录移除。
