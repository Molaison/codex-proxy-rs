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

## 当前唯一隔离入口与生产冻结

用户指定唯一候选验证场所18382，PG18332/Redis18379。入口`/home/zzp/cpr-iso-prodata-20261005/README.md`。生产compose14:08:37改为5fe6391f确由本线程34执行，用户随后冻结，未经新窗口批准不回退、不提交、不再修改生产文件或重启生产服务。

客户端0.160静态musl目录限制1→8MiB已在F102与ywl真实profile/key上通过：原生目录2015014B/40模型，client显示28，非隐藏缺项0，9tools与QA元数据保留。只交付候选，未安装默认binary、未改共享配置/AppServer。客户端第三方依赖版本未变，原链接失败保留。

18382副本已禁用OAuth刷新、warmup、auto-freeze-probe、计划备份；保留访问token和43账号/37Key/38组/22迁移。35脚本是实际00_setup_iso_instance.py的脱敏修正版：运行时私读key、不输出PG口令、真实表名严格验收、restore/up先禁副作用。旧脚本硬编码key曾在源码读取中暴露，用户已被告知协调轮换，未自动换生产凭据。

三个额外pool/full桥接只添加在隔离网关netns，连接现役鉴权Unix socket，只做目录查询；不是Full runtime/browser隔离。没有Full生成候选验收，BiggerContext需另建独立Full环境。

目录漏刷新43aca028修复`refresh_account_catalog`成功后使进程聚合/原生目录缓存失效，触发已有runtime重编译。原生目录40而普通目录缺Web的失败保留。扩展既有回归1/1通过；release149秒。18382现为3.19.0-acceptance-43aca028，双Full key普通18项/9QA+9tools及原生元数据全部通过。新binary SHA256 f20f6ad50e69c94c803db7039c568987bb3bfac4aa371bad833a9ff23c337c52。

```sh
# 后续相关修复使用；已成功单位不重复。不得指向生产或旧18140。
python3 /home/zzp/cpr-iso-prodata-20261005/00_setup_iso_instance.py verify
python3 ~/codex-proxy-rs/.runtime/30_upstream_acceptance/source/deploy/32_integrate.py catalog_iso_prodata
# 只有新候选构建完、18382所有验收owner释放后才能执行；已完成时拒绝重投。
python3 ~/codex-proxy-rs/.runtime/30_upstream_acceptance/source/deploy/34_integrated_release.py upgrade-isolated
```

34候选入口已移除全部生产与旧18140操作实现，原参数会在路径/服务访问前失败。只支持18382网关及六桥接，同镜像前端保留、PG/Redis身份与启动时间和配置字节不变，已实际完成。14:47旧18140原生依赖演练仅作为历史证据保留，不是当前可运行入口。所有发布失败、原始dump、生产冻结前验收继续保留，不能用隔离成功宣称生产完成本次新增修复。

证据：`full-repair.json.catalog_refresh_fix`、`iso-release.json`、`integration.json.prodata_tool_catalog`及原失败数组。入口诊断6591、选择性清槽91f、daemon健康5fe、原整合896均沿各自source与已验收单位，不重复无关请求。
