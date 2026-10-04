<script setup lang="ts">
import type { Account, AccountGroup, ApiKey, RuntimeSettings } from '@/api'
import { BaseCard, BasePageHeader } from '@codex-proxy/ui'
import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from 'vue'
import { RouterLink } from 'vue-router'
import { getAccountGroups, getAccountModels, getAccounts, getApiKeys, getSettings } from '@/api'

interface Catalog {
  models: string[]
  state: 'loaded' | 'failed'
  loadedAt: string
}
interface Snapshot {
  accounts: Account[]
  groups: AccountGroup[]
  keys: ApiKey[]
  settings: RuntimeSettings
  loadedAt: string
}

const snapshot = shallowRef<Snapshot>()
const loading = ref(false)
const catalogs = shallowRef(new Map<string, Catalog>())
const catalogAccountId = ref('')
const loadingCatalog = ref(false)
let catalogController: AbortController | undefined
const error = ref('')
const search = ref('')
const showPolicyDenied = ref(false)
const groupId = ref('')
const keyId = ref('')
const page = ref(1)
const pageSize = 50
let controller: AbortController | undefined
let disposed = false

async function reload() {
  if (loading.value || disposed)
    return
  loading.value = true
  error.value = ''
  controller = new AbortController()
  const options = { signal: controller.signal, silent: true }
  try {
    const [firstAccounts, firstGroups, firstKeys, settings] = await Promise.all([
      getAccounts({ page: 1, pageSize: 200 }, options),
      getAccountGroups({ page: 1, pageSize: 200 }, options),
      getApiKeys({ limit: 200 }, options),
      getSettings(options),
    ])
    const accounts = [...firstAccounts.items]
    const groups = [...firstGroups.items]
    const keys = [...firstKeys.items]
    for (let next = 2; next <= firstAccounts.page.totalPages; next++) {
      const result = await getAccounts({ page: next, pageSize: firstAccounts.page.pageSize }, options)
      accounts.push(...result.items)
    }
    for (let next = 2; next <= firstGroups.page.totalPages; next++) {
      const result = await getAccountGroups({ page: next, pageSize: firstGroups.page.pageSize }, options)
      groups.push(...result.items)
    }
    let cursor = firstKeys.nextCursor
    while (cursor) {
      const result = await getApiKeys({ limit: 200, cursor }, options)
      keys.push(...result.items)
      cursor = result.nextCursor
    }
    if (!options.signal.aborted) {
      snapshot.value = { accounts, groups, keys, settings, loadedAt: new Date().toLocaleString() }
    }
  }
  catch {
    if (!options.signal.aborted)
      error.value = '读取管理 API 失败。下方如有数据，仍为上次成功读取结果，请刷新重试。'
  }
  finally {
    loading.value = false
  }
}

async function loadCatalog() {
  const account = snapshot.value?.accounts.find(item => item.id === catalogAccountId.value)
  if (!account?.enabled || loadingCatalog.value || disposed)
    return
  loadingCatalog.value = true
  catalogController = new AbortController()
  const signal = catalogController.signal
  try {
    const result = await getAccountModels({ accountId: account.id }, { signal, silent: true, timeout: 15000 })
    if (!signal.aborted) {
      catalogs.value = new Map(catalogs.value).set(account.id, {
        models: result.models.map(model => model.id),
        state: 'loaded',
        loadedAt: new Date().toLocaleString(),
      })
    }
  }
  catch {
    if (!signal.aborted)
      catalogs.value = new Map(catalogs.value).set(account.id, { models: [], state: 'failed', loadedAt: '' })
  }
  finally {
    loadingCatalog.value = false
  }
}

const selectedCatalogAccount = computed(() => snapshot.value?.accounts.find(account => account.id === catalogAccountId.value))
const selectedCatalog = computed(() => catalogs.value.get(catalogAccountId.value))
const selectedKey = computed(() => snapshot.value?.keys.find(key => key.id === keyId.value))
const mappings = computed(() => Object.entries(snapshot.value?.settings.modelMappings ?? {}))
const catalogFailures = computed(() => snapshot.value?.accounts.filter(account => catalogs.value.get(account.id)?.state === 'failed') ?? [])
const rows = computed(() => {
  const data = snapshot.value
  if (!data)
    return []
  const models = new Set(mappings.value.flatMap(([alias, target]) => [alias, target]))
  for (const account of data.accounts) {
    for (const model of account.modelAccess.models)
      models.add(model)
    for (const model of catalogs.value.get(account.id)?.models ?? [])
      models.add(model)
    for (const usage of account.usage.models)
      models.add(usage.model)
  }
  const key = selectedKey.value
  const query = search.value.trim().toLowerCase()
  const result = []
  for (const model of [...models].sort()) {
    const targetModel = data.settings.modelMappings[model] ?? model
    for (const account of data.accounts) {
      const memberGroups = data.groups.filter(group => account.groups.some(member => member.id === group.id))
      if (groupId.value && !memberGroups.some(group => group.id === groupId.value))
        continue
      if (keyId.value && (!key || (key.routingScope === 'groups' && !memberGroups.some(group => key.groups.some(bound => bound.id === group.id)))))
        continue
      if (query && !`${model} ${targetModel} ${account.name} ${account.provider} ${account.authenticationKind}`.toLowerCase().includes(query))
        continue
      const listed = account.modelAccess.models.includes(targetModel)
      const policyAllowed = account.modelAccess.mode === 'all' || (account.modelAccess.mode === 'allowlist' ? listed : !listed)
      if (!policyAllowed && !showPolicyDenied.value)
        continue
      const availableGroups = memberGroups.filter(group => group.enabled && account.enabled && policyAllowed)
      const keyAllowed = key
        ? key.enabled && account.enabled && policyAllowed && key.providerKinds.includes(account.provider)
          && (key.routingScope === 'all' || availableGroups.some(group => key.groups.some(bound => bound.id === group.id)))
        : undefined
      const catalog = catalogs.value.get(account.id)
      result.push({
        id: `${account.id}:${model}`,
        model,
        targetModel,
        account,
        memberGroups,
        availableGroups,
        policyAllowed,
        keyAllowed,
        catalogLabel: !catalog ? '目录未加载'
          : catalog.state === 'failed' ? '目录读取失败'
            : catalog.models.includes(targetModel) ? '目录列出目标模型' : '目录未列出目标模型',
      })
    }
  }
  return result
})
const pageCount = computed(() => Math.max(1, Math.ceil(rows.value.length / pageSize)))
const currentPage = computed(() => Math.min(page.value, pageCount.value))
const visibleRows = computed(() => rows.value.slice((currentPage.value - 1) * pageSize, currentPage.value * pageSize))

onMounted(() => { void reload() })
onBeforeUnmount(() => {
  disposed = true
  controller?.abort()
  catalogController?.abort()
})
</script>

<template>
  <div class="model-management flex min-w-0 flex-col gap-5 text-cp-text">
    <BasePageHeader title="模型与权限" description="按模型 × 渠道账号查看当前配置。权限可见不代表健康或可执行。" />
    <BaseCard>
      <template #body>
        <div class="space-y-3 p-4 text-sm">
          <p>模型、渠道、聊天/工具能力、effort、session 是五个独立维度。模型名、认证方式和管理能力不能证明工具可用。</p>
          <p class="text-cp-text-secondary">渠道以真实账号名和认证类型展示；provider 是协议分类，不等于供应商。ChatGPT Web 工具入口可能独立运行或经专用 CPR 渠道接入；是否有工具权限取决于渠道与 Key，不能仅凭模型 ID 判断。</p>
          <p>同一模型 ID（例如 gpt-6-astra）可在授权范围内的 OAuth 与 AnyRouter 账号间自动调度。模型映射只转换模型 ID，不锁定渠道；渠道选择仍由账号政策、Key 范围和调度决定。</p>
          <p class="text-cp-text-secondary">本部署约定（非 API 实时能力声明）：既有姓名项目渠道用于纯聊天；工具版使用 codex-web-tools 或专用 CPR 测试渠道，权限需单独授予。临时/持久保存与思考预算是不同设置，仅当前兼容客户端把保存模式后缀附在 effort 上。</p>
          <nav aria-label="编辑配置" class="flex flex-wrap gap-4">
            <RouterLink to="/accounts">编辑账号与模型政策</RouterLink>
            <RouterLink to="/groups">编辑分组</RouterLink>
            <RouterLink to="/keys">编辑 Key</RouterLink>
            <RouterLink to="/settings/upstream">编辑模型映射与上游设置</RouterLink>
          </nav>
        </div>
      </template>
    </BaseCard>

    <BaseCard>
      <template #body>
        <div class="space-y-4 p-4">
          <div class="flex flex-wrap items-end gap-3">
            <label class="grid gap-1 text-sm">
              搜索模型 / 渠道账号
              <input v-model="search" type="search" placeholder="模型 ID、账号名、认证类型" @input="page = 1">
            </label>
            <label class="grid gap-1 text-sm">
              分组
              <select v-model="groupId" @change="page = 1">
                <option value="">全部分组</option>
                <option v-for="group in snapshot?.groups" :key="group.id" :value="group.id">{{ group.name }}{{ group.enabled ? '' : '（已停用）' }}</option>
              </select>
            </label>
            <label class="grid gap-1 text-sm">
              Key（仅名称与 ID）
              <select v-model="keyId" @change="page = 1">
                <option value="">全部 Key</option>
                <option v-for="key in snapshot?.keys" :key="key.id" :value="key.id">{{ key.name || '未命名' }} · {{ key.id }}{{ key.enabled ? '' : '（已停用）' }}</option>
              </select>
            </label>
            <label class="flex items-center gap-2 text-sm">
              <input v-model="showPolicyDenied" type="checkbox" @change="page = 1">
              显示模型政策拒绝项
            </label>
            <button type="button" :disabled="loading" @click="reload">{{ loading ? '读取中…' : '刷新配置' }}</button>
          </div>
          <p class="text-sm text-cp-text-secondary" aria-live="polite">
            {{ snapshot ? `最近完整读取：${snapshot.loadedAt}` : '尚未取得数据' }}。配置仅在进入页面或点击刷新时读取，不自动查询账号目录。
          </p>
          <p v-if="error" role="alert" class="text-sm text-cp-error-text">{{ error }}</p>
          <p v-if="catalogFailures.length" role="status" class="text-sm text-cp-warning-text">
            模型目录读取失败：{{ catalogFailures.map(account => account.name).join('、') }}。仍展示政策和已有记录，不将失败解释为无权限。
          </p>
          <p v-if="selectedKey" class="text-sm">
            当前 Key：{{ selectedKey.name || '未命名' }}，{{ selectedKey.enabled ? '已启用' : '已停用' }}；
            {{ selectedKey.routingScope === 'all' ? '范围为全部账号，不要求组成员关系' : '范围为绑定组，只有启用组参与路由' }}。
            筛选保留绑定但被停用的行以便排查。额度、限流和实时调度不在权限判断内。
          </p>
          <p class="text-sm text-cp-text-secondary">矩阵先展示账号政策、用量记录及映射中的模型 ID，再补充手动加载的目录，不是完整上游能力清单。目录只在点击下方按钮时读取单个账号，缓存缺失时可能访问上游，不发送聊天或工具测试。</p>
          <div class="flex flex-wrap items-end gap-3">
            <label class="grid gap-1 text-sm">
              单账号模型目录
              <select v-model="catalogAccountId" :disabled="loadingCatalog">
                <option value="">选择账号</option>
                <option v-for="account in snapshot?.accounts" :key="account.id" :value="account.id" :disabled="!account.enabled">{{ account.name }} · {{ account.id }}{{ account.enabled ? '' : '（停用）' }}</option>
              </select>
            </label>
            <button type="button" :disabled="loadingCatalog || !selectedCatalogAccount?.enabled" @click="loadCatalog">{{ loadingCatalog ? '加载单账号目录中…' : '加载所选账号目录（可能访问上游）' }}</button>
            <span v-if="selectedCatalog?.state === 'loaded'" class="text-sm text-cp-text-secondary">目录读取于 {{ selectedCatalog.loadedAt }}，不会随配置刷新自动更新。</span>
          </div>
          <div class="overflow-x-auto">
            <table class="w-full text-left text-sm">
              <caption class="sr-only">模型与渠道账号权限矩阵</caption>
              <thead>
                <tr>
                  <th scope="col">模型 ID / 目录</th>
                  <th scope="col">渠道账号</th>
                  <th scope="col">聊天 / 工具</th>
                  <th scope="col">思考强度</th>
                  <th scope="col">会话保存</th>
                  <th scope="col">启用与模型政策</th>
                  <th scope="col">可用组（配置层）</th>
                  <th v-if="keyId" scope="col">所选 Key</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="row in visibleRows" :key="row.id">
                  <th scope="row" class="font-normal">
                    <span class="font-mono break-all">{{ row.model }}</span>
                    <small v-if="row.targetModel !== row.model">映射目标：{{ row.targetModel }}</small>
                    <small>{{ row.catalogLabel }}</small>
                  </th>
                  <td>
                    <span>{{ row.account.name }}</span>
                    <small>{{ row.account.authenticationKind }} · provider: {{ row.account.provider }}</small>
                    <small>账号 ID：{{ row.account.id }}</small>
                  </td>
                  <td>聊天：未声明<small>工具：未声明</small></td>
                  <td>未声明</td>
                  <td>未声明<small>temporary / persistent</small></td>
                  <td>
                    {{ row.account.enabled ? '账号启用' : '账号停用' }}
                    <small>{{ row.policyAllowed ? '模型政策允许' : '模型政策拒绝' }} · {{ row.account.modelAccess.mode }}</small>
                    <small>运行状态：{{ row.account.status }}（非执行保证）</small>
                  </td>
                  <td>
                    {{ row.availableGroups.map(group => group.name).join('、') || '无' }}
                    <small>所属：{{ row.memberGroups.map(group => `${group.name}${group.enabled ? '' : '（停用）'}`).join('、') || '未分组' }}</small>
                  </td>
                  <td v-if="keyId">{{ row.keyAllowed ? '配置允许' : '配置不允许' }}<small>不代表上游接受请求</small></td>
                </tr>
                <tr v-if="!visibleRows.length"><td :colspan="keyId ? 8 : 7" class="text-cp-text-secondary">{{ loading ? '正在读取…' : '没有匹配条目。未列出不代表上游不支持。' }}</td></tr>
              </tbody>
            </table>
          </div>
          <div class="flex flex-wrap items-center gap-3 text-sm">
            <span>{{ rows.length }} 个模型 × 账号条目；第 {{ currentPage }} / {{ pageCount }} 页</span>
            <button type="button" :disabled="currentPage <= 1" @click="page = currentPage - 1">上一页</button>
            <button type="button" :disabled="currentPage >= pageCount" @click="page = currentPage + 1">下一页</button>
          </div>
        </div>
      </template>
    </BaseCard>

    <BaseCard>
      <template #body>
        <details class="p-4 text-sm">
          <summary class="cursor-pointer">全局模型映射（{{ mappings.length }} 条）</summary>
          <p class="my-3 text-cp-text-secondary">映射是请求模型到上游模型的转换，不是新增授权。别名行按映射 target 的精确 ID 检查账号 modelAccess；未映射行使用原 ID。映射不锁定渠道，配置允许也不是执行保证。</p>
          <ul class="space-y-2">
            <li v-for="[alias, target] in mappings" :key="alias"><code>{{ alias }}</code> → <code>{{ target }}</code></li>
          </ul>
          <p v-if="!mappings.length">未配置映射。</p>
        </details>
      </template>
    </BaseCard>
  </div>
</template>

<style scoped>
input, select, button {
  border: 1px solid var(--cp-color-border);
  border-radius: 0.5rem;
  padding: 0.5rem 0.75rem;
  background: var(--cp-color-bg-container);
  color: inherit;
}
select { max-width: min(100%, 24rem); }
button { cursor: pointer; }
button:disabled { cursor: default; opacity: 0.5; }
a { color: var(--cp-color-primary); text-decoration: underline; }
th, td { padding: 0.75rem; vertical-align: top; border-bottom: 1px solid var(--cp-color-border); min-width: 7rem; }
small { display: block; margin-top: 0.35rem; color: var(--cp-color-text-secondary); overflow-wrap: anywhere; }
</style>
