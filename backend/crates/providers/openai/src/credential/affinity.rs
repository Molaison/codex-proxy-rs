//! 逻辑会话账号绑定与线程传输隔离键的单向派生

use gateway_core::operation::RawJsonPayload;
use gateway_core::policy::ClientApiKeyId;
use gateway_core::provider_ports::{ProviderSessionAffinityKey, ProviderSessionAlias};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::request_identity::{account_session_with_headers, follows_session_with_headers};
use crate::transport::protocol::responses::CodexResponsesRequest;
use crate::transport::request::derive_conversation_anchor;

const AFFINITY_KEY_HASH_LENGTH: usize = 12;

/// Full（chatgpt-web）工具模型族的独立会话亲和命名空间标记。
///
/// 该族模型由 Full 账号族承载。它与原生账号共用同一 client session 时，普通模型的
/// 请求会把整个会话绑定迁移到原生账号，使工具续轮落到不支持该模型的上游；独立
/// 命名空间让两个账号族的绑定互不覆盖。
pub(crate) const FULL_TOOLS_AFFINITY_NAMESPACE: &str = "full-tools";
/// Full 账号族发布工具模型时使用的 scope 前缀。
const FULL_TOOLS_MODEL_SCOPE: &str = "chatgpt-web";
/// Full 账号族工具变体的模型名后缀。
const FULL_TOOLS_MODEL_SUFFIX: &str = "-tools";

/// 一次请求派生出的账号亲和键及其结构化日志上下文。
pub(crate) struct CodexSessionAffinity {
    key: ProviderSessionAffinityKey,
    root_key: Option<ProviderSessionAffinityKey>,
    turn_alias: Option<(ProviderSessionAffinityKey, ProviderSessionAlias)>,
    key_hash: String,
    anchor_source: &'static str,
    anchor: String,
    session_id: Option<String>,
    follow_only: bool,
}

impl CodexSessionAffinity {
    pub(crate) fn root_key(&self) -> Option<&ProviderSessionAffinityKey> {
        self.root_key.as_ref()
    }

    /// 子线程首次选号继承根会话偏好，之后仅更新自己的绑定。
    fn with_thread(mut self, thread_id: Option<&str>) -> Option<Self> {
        if let Some(thread_id) = non_empty(thread_id)
            && self
                .session_id
                .as_deref()
                .is_some_and(|root| root != thread_id)
        {
            let child_key = opaque_affinity_key(
                "child-thread",
                &format!("{}\0{thread_id}", self.key.expose_to_store()),
            )?;
            self.root_key = Some(std::mem::replace(&mut self.key, child_key));
            self.key_hash = short_key_hash(&self.key);
            self.anchor_source = "child-thread";
            self.anchor = thread_id.to_owned();
        }
        Some(self)
    }


    pub(crate) fn from_turn_alias(
        turn: ProviderSessionAffinityKey,
        alias: ProviderSessionAlias,
    ) -> Self {
        // 旧轮次可能登记在线程绑定下，统一还原为会话身份并保留原记录用于续期
        let key = alias.session_key.clone();
        Self {
            key_hash: short_key_hash(&key),
            key,
            root_key: None,
            anchor_source: "turn-session",
            anchor: String::new(),
            session_id: None,
            follow_only: alias.follow_only,
            turn_alias: Some((turn, alias)),
        }
    }

    pub(crate) fn turn_alias(&self) -> Option<&ProviderSessionAffinityKey> {
        self.turn_alias.as_ref().map(|(key, _)| key)
    }

    pub(crate) fn alias_record(&self) -> ProviderSessionAlias {
        // 当前请求可加严跟随约束，续期仍保留轮次最初登记的身份，不能改写关联
        self.turn_alias
            .as_ref()
            .map(|(_, alias)| alias.clone())
            .unwrap_or_else(|| ProviderSessionAlias {
                session_key: self.key.clone(),
                follow_only: self.follow_only,
                root_session_key: None,
            })
    }

    pub(crate) const fn follow_only(&self) -> bool {
        self.follow_only
    }

    pub(crate) fn with_follow_only(mut self, follow_only: bool) -> Self {
        self.follow_only |= follow_only;
        self
    }

    #[must_use]
    pub(crate) const fn key(&self) -> &ProviderSessionAffinityKey {
        &self.key
    }

    #[must_use]
    pub(crate) fn key_hash(&self) -> &str {
        &self.key_hash
    }

    /// 返回可持久化的客户端作用域不透明会话关联值
    #[must_use]
    pub(crate) fn persistence_hash(&self) -> &str {
        self.key.expose_to_store()
    }

    #[must_use]
    pub(crate) const fn anchor_source(&self) -> &'static str {
        self.anchor_source
    }

    #[must_use]
    pub(crate) fn anchor(&self) -> &str {
        &self.anchor
    }

    #[must_use]
    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    #[must_use]
    pub(crate) const fn session_id_present(&self) -> bool {
        self.session_id.is_some()
    }

    #[must_use]
    pub(crate) fn into_key(self) -> ProviderSessionAffinityKey {
        self.key
    }
}

/// 将原始 response ID 投影为客户端作用域的不可逆关联值
#[must_use]
pub(crate) fn derive_previous_response_id_hash(
    previous_response_id: &str,
    client_api_key_id: &ClientApiKeyId,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codex-previous-response-observation-v1\0");
    hasher.update(client_api_key_id.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(previous_response_id.as_bytes());
    hex::encode(hasher.finalize())
}

pub(crate) fn derive_codex_session_affinity(
    request: &CodexResponsesRequest,
    client_api_key_id: &ClientApiKeyId,
) -> Option<CodexSessionAffinity> {
    let session_id = non_empty(request.client_session_id.as_deref()).map(str::to_owned);
    let (anchor_source, anchor) = derive_account_affinity_anchor(request)?;
    // Full 工具族使用独立命名空间，普通模型的会话绑定迁移不能覆盖它。
    let namespace = full_tools_affinity_namespace(request.model());
    session_affinity(
        anchor_source,
        anchor,
        session_id,
        client_api_key_id,
        namespace,
    )?
    .with_thread(request.client_thread_id.as_deref())
}

/// 原生 JSON 端点与 Responses 共用绑定命名空间，Search id 是逻辑会话身份
pub(crate) fn derive_codex_endpoint_session_affinity(
    payload: &RawJsonPayload,
    client_api_key_id: &ClientApiKeyId,
    body_session_field: &str,
) -> Option<CodexSessionAffinity> {
    derive_endpoint_affinity_with_headers(payload, client_api_key_id, body_session_field, &[])
}

pub(crate) fn derive_endpoint_affinity_with_headers(
    payload: &RawJsonPayload,
    client_api_key_id: &ClientApiKeyId,
    body_session_field: &str,
    headers: &[gateway_core::engine::middleware::MiddlewareHeader],
) -> Option<CodexSessionAffinity> {
    let body = serde_json::from_slice::<Map<String, Value>>(payload.body()).unwrap_or_default();
    let mut context = payload.context().clone();
    for (header, field) in [("session-id", "session_id"), ("thread-id", "thread_id"), ("x-codex-turn-metadata", "turn_metadata")] {
        if let Some(value) = headers.iter().find(|h| h.name().eq_ignore_ascii_case(header))
            .and_then(|h| std::str::from_utf8(h.value()).ok()) {
            context.insert(field.to_owned(), Value::String(value.to_owned()));
        }
    }
    let session_id = gateway_protocol::openai::codex_session_id(&body, &context)
        .or_else(|| non_empty(body.get(body_session_field).and_then(Value::as_str)).map(str::to_owned))?;
    session_affinity("root-session", session_id.clone(), Some(session_id), client_api_key_id, None)?
        .with_thread(gateway_protocol::openai::codex_thread_id(&body, &context).as_deref())
}

pub(crate) fn derive_live_session_affinity(
    request: &gateway_core::operation::ProviderHttpRequest,
    headers: &[gateway_core::engine::middleware::MiddlewareHeader],
    client_api_key_id: &ClientApiKeyId,
) -> Option<CodexSessionAffinity> {
    let mut effective = headers.to_vec();
    effective.extend(
        request
            .headers()
            .iter()
            .filter(|h| {
                !headers
                    .iter()
                    .any(|override_header| override_header.name().eq_ignore_ascii_case(h.name()))
            })
            .map(|h| {
                gateway_core::engine::middleware::MiddlewareHeader::new(h.name(), h.value().clone())
            }),
    );
    // x-session-id 是实时通话身份，不代表根对话；只消费官方发送的 session-id
    let session_id = account_session_with_headers(&Map::new(), &Map::new(), &effective)?;
    session_affinity(
        "root-session",
        session_id.clone(),
        Some(session_id),
        client_api_key_id,
        None,
    )
    .map(|affinity| {
        affinity.with_follow_only(follows_session_with_headers(
            &Map::new(),
            &Map::new(),
            &effective,
        ))
    })
}

/// HTTP 降级等传输状态仍按线程隔离，不能跟随整个对话的账号绑定扩大范围
pub(crate) fn derive_codex_transport_key(
    request: &CodexResponsesRequest,
    client_api_key_id: &ClientApiKeyId,
) -> Option<ProviderSessionAffinityKey> {
    let (source, anchor) = non_empty(request.client_thread_id.as_deref())
        .map(|thread| ("thread-transport", thread.to_owned()))
        .or_else(|| derive_conversation_anchor(request))?;
    session_affinity(source, anchor, None, client_api_key_id, None).map(CodexSessionAffinity::into_key)
}

/// 判断请求模型是否属于 Full 工具族，并返回其独立会话亲和命名空间。
///
/// 只按 Full 账号族公布的 `<scope>/<模型>-tools` 形状识别；其他 scope 的同名后缀
/// 与同一 scope 的非工具变体都沿用默认命名空间。
fn full_tools_affinity_namespace(model: &str) -> Option<&'static str> {
    let (scope, local) = model.split_once('/')?;
    if scope != FULL_TOOLS_MODEL_SCOPE {
        return None;
    }
    let stem = local.strip_suffix(FULL_TOOLS_MODEL_SUFFIX)?;
    (!stem.is_empty()).then_some(FULL_TOOLS_AFFINITY_NAMESPACE)
}

fn session_affinity(
    anchor_source: &'static str,
    anchor: String,
    session_id: Option<String>,
    client_api_key_id: &ClientApiKeyId,
    namespace: Option<&'static str>,
) -> Option<CodexSessionAffinity> {
    let session_key = opaque_affinity_key(anchor_source, &anchor)?;
    // 默认命名空间保持既有键字节不变；只有 Full 工具族追加独立命名空间段。
    let scoped = match namespace {
        None => format!(
            "{}\0{}",
            client_api_key_id.as_str(),
            session_key.expose_to_store()
        ),
        Some(namespace) => format!(
            "{}\0{}\0{}",
            namespace,
            client_api_key_id.as_str(),
            session_key.expose_to_store()
        ),
    };
    let key = opaque_affinity_key("client-session", &scoped)?;
    let key_hash = short_key_hash(&key);
    Some(CodexSessionAffinity {
        key,
        root_key: None,
        turn_alias: None,
        key_hash,
        anchor_source,
        anchor,
        session_id,
        follow_only: false,
    })
}

fn short_key_hash(key: &ProviderSessionAffinityKey) -> String {
    // 亲和键本身已经是 SHA-256；日志沿用 WebSocket 诊断的 12 位短哈希长度
    key.expose_to_store()
        .chars()
        .take(AFFINITY_KEY_HASH_LENGTH)
        .collect()
}

fn derive_account_affinity_anchor(
    request: &CodexResponsesRequest,
) -> Option<(&'static str, String)> {
    non_empty(request.client_session_id.as_deref())
        .map(|value| ("root-session", value.to_owned()))
        .or_else(|| {
            non_empty(request.client_conversation_id.as_deref())
                .map(|value| ("root-conversation", value.to_owned()))
        })
        .or_else(|| {
            request
                .explicit_prompt_cache_key
                .then(|| request.prompt_cache_key())
                .flatten()
                .and_then(|value| non_empty(Some(value)))
                .map(|value| ("root-prompt-cache", value.to_owned()))
        })
        .or_else(|| {
            non_empty(request.local_conversation_id.as_deref())
                .map(|value| ("local-conversation", value.to_owned()))
        })
        .or_else(|| derive_conversation_anchor(request))
}

fn opaque_affinity_key(domain: &str, value: &str) -> Option<ProviderSessionAffinityKey> {
    let mut hasher = Sha256::new();
    hasher.update(b"codex-session-affinity-v1\0");
    hasher.update(domain.as_bytes());
    hasher.update(b"\0");
    hasher.update(value.as_bytes());
    ProviderSessionAffinityKey::try_new(hex::encode(hasher.finalize())).ok()
}

/// 恢复旧版 `cyber_policy` 的会话隔离键
///
/// 它只接受显式 session/conversation 或客户端明确给出的 prompt cache key，避免将
/// 请求内容哈希误当成长会话；`previous_response_id` 续写不参与该策略
pub(crate) fn derive_codex_cyber_policy_session_key(
    request: &CodexResponsesRequest,
    client_api_key_id: &ClientApiKeyId,
) -> Option<ProviderSessionAffinityKey> {
    if request.previous_response_id().is_some() {
        return None;
    }
    let session_id = non_empty(request.client_session_id.as_deref())
        .or_else(|| non_empty(request.client_conversation_id.as_deref()))
        .or_else(|| {
            request
                .explicit_prompt_cache_key
                .then(|| request.prompt_cache_key())
                .flatten()
                .and_then(|value| non_empty(Some(value)))
        })?;
    let mut hasher = Sha256::new();
    hasher.update(b"cyber-policy-session\0");
    hasher.update(client_api_key_id.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(session_id.as_bytes());
    ProviderSessionAffinityKey::try_new(hex::encode(hasher.finalize())).ok()
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// 官方 Images 的轮次 ID 与 Responses turn_id 对应，关联必须隔离客户端 API Key
pub(crate) fn derive_turn_alias(
    turn_id: &str,
    client: &ClientApiKeyId,
) -> Option<ProviderSessionAffinityKey> {
    let turn_id = non_empty(Some(turn_id))?;
    opaque_affinity_key("client-turn", &format!("{}\0{turn_id}", client.as_str()))
}


#[cfg(test)]
mod hot_profile_tests {
    use super::*;

    #[test]
    fn hot_profile_legacy_identity() {
        let client = ClientApiKeyId::new("client-test").unwrap();
        let mut request = CodexResponsesRequest::from_body(serde_json::from_value(serde_json::json!({"model":"gpt-test"})).unwrap());
        request.client_session_id = Some("root-test".to_owned());
        request.client_account_session_id = Some("new-root-must-not-override-old".to_owned());
        let root = derive_codex_session_affinity(&request, &client).unwrap();
        let expected = session_affinity("root-session", "root-test".to_owned(), Some("root-test".to_owned()), &client, None).unwrap();
        assert_eq!(root.key(), expected.key());
        request.client_thread_id = Some("child-test".to_owned());
        let child = derive_codex_session_affinity(&request, &client).unwrap();
        assert_eq!(child.root_key(), Some(root.key()));
        let old_child = opaque_affinity_key("child-thread", &format!("{}\0child-test", root.key().expose_to_store())).unwrap();
        assert_eq!(child.key(), &old_child);
        assert!(!child.follow_only());
        let full = session_affinity("root-session", "root-test".to_owned(), Some("root-test".to_owned()), &client, Some("full-tools")).unwrap().with_thread(Some("child-test")).unwrap();
        assert_ne!(child.key(), full.key());
        let turn = derive_turn_alias("turn-test", &client).unwrap();
        let mut alias = child.alias_record();
        alias.root_session_key = Some(root.key().clone());
        let restored = CodexSessionAffinity::from_turn_alias(turn, alias);
        assert_eq!(restored.key(), child.key());
        request.client_session_id = None;
        request.client_thread_id = None;
        request.client_conversation_id = Some("conversation-test".to_owned());
        let conversation = derive_codex_session_affinity(&request, &client).unwrap();
        assert_eq!(conversation.anchor_source(), "root-conversation");
        assert_ne!(conversation.key(), root.key());
    }
}
