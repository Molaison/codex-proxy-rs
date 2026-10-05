# ywl 部署集成边界

CPR 镜像只更新网关与前端，不包含独立 Web QA、Full、Pool、MCP 或浏览器服务。

- 宿主继续使用已部署的 `12_stack_selfheal_watchdog_20260929.sh` 和 `26_release_stale_client_slots_20261003.sh`。本目录保留现役脚本，不在构建或候选启动时执行它们。
- 配置文件保持容器 userns 所需的 10001 组读权限。网关切换由唯一运维 owner 沿现役容器定义执行，不运行整栈 compose 重建数据库。
- 生产数据库保留实时数据和最新账号/Key配置；不可用隔离验收数据库覆盖。ywl 迁移20/21沿用历史，新迁移追加22。
- Web PR5 的原名纯聊天与 `-tools` 工具路线、两种元数据、两个工具Key的QA授权必须保留。Web QA/项目/上传/论文代码已整合进干净main `8cff8ff`，24个相关现役文件一致；407定向和16 Node测试通过，28浏览器测试未执行。源码整合不等于替换现役Web runtime，后续Full临时持久增量仍由独立owner管理。
- `/v1/live` 路由应随旧版保留，但原账号被上游拒绝语音会话，恢复路由不代表语音收发验收通过。
- 密文、EOF及其他已通过单位不作为新故障重复重试；新变更只验证受影响边界。

统一验收状态与具体镜像以管理项目 `12_upstream_acceptance` 的 README 和 integration.json 为准。

## Full 网络桥接遗漏

现役 watchdog 的网络空间桥接表不含后来新增的 `chatgpt-web-full-bridge-1/2`。已在冻结前的14:08发布中同步；未来切流仍须核对实际netns和独立owner，不重启Full provider、MCP或浏览器。

隔离候选使用独立 `cpr-upstream30-full-bridge-1/2`，在宿主回环17971/17972转发既有鉴权Unix socket。它们不替换生产桥接；结束隔离验收后由owner按记录移除。

## 生产冻结与原生依赖演练

2026-10-05 14:12用户冻结生产操作。未经新窗口批准，禁止停止/重建/重启生产CPR、桥接、PG/Redis、watchdog，也禁止切候选。冻结前14:08实际上线5fe6391f，生产目录24启用/2停用、双Full各9QA+9tools、DeepSeek两轮2.266/1.489秒和具名forced两轮26.046/13.203秒已验收；这不授权后续生产动作。两次中断与失败证据不删除。

候选 `34_integrated_release.py` 仅开放 `rehearse`；所有原生产stage在访问路径/服务前报PRODUCTION_FROZEN，`--allow-interrupt`不能代替窗口。未替换生产deploy34文件。

```sh
# 本轮已通过，不重复执行。只供未来相关修复；先与目录owner协调18140空闲。
python3 ~/codex-proxy-rs/.runtime/30_upstream_acceptance/source/deploy/34_integrated_release.py rehearse
```

14:47原生隔离演练通过：未知native requires在停网关前拒绝；network=none的真实依赖正确发现；正常自动rm和残留依赖清理后同image/config重建cpr-upstream30；health204，PG/Redis容器身份/启动时间与配置字节不变，生产变更0。两临时依赖容器随演练清理，原gateway创建命令和失败恢复入口在私有release-rehearsal.json。证据full-repair.json.deployment_rehearsal。此演练证明容器生命周期边界，不宣称验证了全部生产systemd或浏览器在途无损。

新增入口诊断6591、选择性清槽91f、daemon健康5fe均保留各自验收source。33脚本提供已执行定点构建/入口诊断的复现命令，不重复已通过业务请求。
