//! 会话账号绑定的 Redis 版本快照、原子认领与续期

use std::time::Duration;

use gateway_core::account::ProviderAccountId;
use gateway_core::provider_ports::{
    ProviderSessionAffinityKey, ProviderSessionAffinityPort, ProviderSessionBinding,
    ProviderStoreError, ProviderStoreErrorKind,
};
use gateway_core::routing::ProviderKind;
use redis::aio::ConnectionManager;

use crate::StoreResult;

use super::{namespace, resource_fingerprint};

const MAX_SESSION_AFFINITY_TTL: Duration =
    Duration::from_secs(gateway_core::account::MAX_SESSION_AFFINITY_TTL_HOURS as u64 * 3600);

// The 3.19 string key is canonical. A cleared/expired legacy key cannot be
// resurrected by the companion. Legacy A-B-A writes have no fencing guarantee.
const LOAD_SCRIPT: &str = r#"
local account = redis.call('GET', KEYS[1])
if not account then
  redis.call('DEL', KEYS[2])
  return nil
end
local raw = redis.call('GET', KEYS[2])
local record = raw and cjson.decode(raw) or nil
if not record or record.accountId ~= account then
  raw = '{"accountId":' .. cjson.encode(account) .. ',"revision":' .. cjson.encode(ARGV[1]) .. '}'
end
local ttl = redis.call('PTTL', KEYS[1])
if ttl < 0 then redis.call('SET', KEYS[2], raw)
elseif ttl > 0 then redis.call('PSETEX', KEYS[2], ttl, raw)
else redis.call('DEL', KEYS[2]); return nil end
return raw
"#;

const COMPARE_AND_BIND_SCRIPT: &str = r#"
local account = redis.call('GET', KEYS[1])
local current = redis.call('GET', KEYS[2])
if not account then
  redis.call('DEL', KEYS[2])
  if ARGV[1] ~= '' then return 0 end
elseif ARGV[1] == '' or account ~= ARGV[4] or current ~= ARGV[1] then
  return 0
end
redis.call('PSETEX', KEYS[1], tonumber(ARGV[3]), ARGV[5])
redis.call('PSETEX', KEYS[2], tonumber(ARGV[3]), ARGV[2])
return 1
"#;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BindingRecord {
    account_id: String,
    revision: String,
}

impl BindingRecord {
    fn encode(binding: &ProviderSessionBinding) -> Result<String, ProviderStoreError> {
        serde_json::to_string(&Self {
            account_id: binding.account_id().as_str().to_owned(),
            revision: binding.revision().to_owned(),
        })
        .map_err(|_| provider_invalid("encode provider session binding"))
    }

    fn decode(raw: &str) -> Result<ProviderSessionBinding, ProviderStoreError> {
        let record: Self = serde_json::from_str(raw)
            .map_err(|_| provider_invalid("decode provider session binding"))?;
        let account = ProviderAccountId::new(record.account_id)
            .map_err(|_| provider_invalid("decode provider session binding account"))?;
        ProviderSessionBinding::new(account, record.revision)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AliasRecord {
    session_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_session_key: Option<String>,
    follow_only: bool,
}

#[derive(Clone)]
pub struct RedisProviderSessionAffinityRepository {
    connection: ConnectionManager,
    namespace: String,
}

impl RedisProviderSessionAffinityRepository {
    pub fn new(connection: ConnectionManager, key_namespace: &str) -> StoreResult<Self> {
        Ok(Self {
            connection,
            namespace: namespace(key_namespace)?,
        })
    }

    fn key(
        &self,
        provider_kind: &ProviderKind,
        affinity_key: &ProviderSessionAffinityKey,
    ) -> Result<String, ProviderStoreError> {
        let scope = format!(
            "{}\0{}",
            provider_kind.as_str(),
            affinity_key.expose_to_store()
        );
        let fingerprint = resource_fingerprint("provider session affinity", &scope)
            .map_err(|_| provider_invalid("encode provider session affinity key"))?;
        Ok(format!(
            "{}:scheduler:affinity:{{{fingerprint}}}",
            self.namespace
        ))
    }
}

impl ProviderSessionAffinityPort for RedisProviderSessionAffinityRepository {
    fn load<'a>(
        &'a self,
        provider_kind: &'a ProviderKind,
        key: &'a ProviderSessionAffinityKey,
    ) -> futures::future::BoxFuture<'a, Result<Option<ProviderSessionBinding>, ProviderStoreError>>
    {
        Box::pin(async move {
            let legacy_key = self.key(provider_kind, key)?;
            let raw = redis::Script::new(LOAD_SCRIPT)
                .key(&legacy_key)
                .key(format!("{legacy_key}:binding"))
                .arg(uuid::Uuid::new_v4().simple().to_string())
                .invoke_async::<Option<String>>(&mut self.connection.clone())
                .await
                .map_err(|source| {
                    crate::provider_unavailable("load provider session binding", source)
                })?;
            raw.as_deref().map(BindingRecord::decode).transpose()
        })
    }

    fn compare_and_bind<'a>(
        &'a self,
        provider_kind: &'a ProviderKind,
        key: &'a ProviderSessionAffinityKey,
        expected: Option<&'a ProviderSessionBinding>,
        account_id: &'a ProviderAccountId,
        ttl: Duration,
    ) -> futures::future::BoxFuture<'a, Result<Option<ProviderSessionBinding>, ProviderStoreError>>
    {
        Box::pin(async move {
            let ttl_millis = session_affinity_ttl_millis(ttl)?;
            let expected_raw = expected
                .map(BindingRecord::encode)
                .transpose()?
                .unwrap_or_default();
            // 同账号命中仅续期；初次认领及换号生成新版本
            let binding = match expected.filter(|binding| binding.account_id() == account_id) {
                Some(binding) => binding.clone(),
                None => ProviderSessionBinding::new(
                    account_id.clone(),
                    uuid::Uuid::new_v4().simple().to_string(),
                )?,
            };
            let legacy_key = self.key(provider_kind, key)?;
            let applied = redis::Script::new(COMPARE_AND_BIND_SCRIPT)
                .key(&legacy_key)
                .key(format!("{legacy_key}:binding"))
                .arg(expected_raw)
                .arg(BindingRecord::encode(&binding)?)
                .arg(ttl_millis)
                .arg(expected.map(|binding| binding.account_id().as_str()).unwrap_or_default())
                .arg(account_id.as_str())
                .invoke_async::<bool>(&mut self.connection.clone())
                .await
                .map_err(|source| {
                    crate::provider_unavailable("admit provider session binding", source)
                })?;
            Ok(applied.then_some(binding))
        })
    }
    fn load_alias<'a>(
        &'a self,
        provider: &'a ProviderKind,
        alias: &'a ProviderSessionAffinityKey,
    ) -> futures::future::BoxFuture<
        'a,
        Result<Option<gateway_core::provider_ports::ProviderSessionAlias>, ProviderStoreError>,
    > {
        Box::pin(async move {
            let value = redis::cmd("GET")
                .arg(format!("{}:alias", self.key(provider, alias)?))
                .query_async::<Option<String>>(&mut self.connection.clone())
                .await
                .map_err(|source| crate::provider_unavailable("load session alias", source))?;
            value
                .map(|raw| {
                    let record: AliasRecord = serde_json::from_str(&raw)
                        .map_err(|_| provider_invalid("decode session alias"))?;
                    Ok(gateway_core::provider_ports::ProviderSessionAlias {
                        session_key: ProviderSessionAffinityKey::try_new(record.session_key)?,
                        follow_only: record.follow_only,
                        root_session_key: record
                            .root_session_key
                            .map(ProviderSessionAffinityKey::try_new)
                            .transpose()?,
                    })
                })
                .transpose()
        })
    }

    fn bind_alias<'a>(
        &'a self,
        provider: &'a ProviderKind,
        alias: &'a ProviderSessionAffinityKey,
        session: &'a gateway_core::provider_ports::ProviderSessionAlias,
        ttl: Duration,
    ) -> futures::future::BoxFuture<'a, Result<bool, ProviderStoreError>> {
        Box::pin(async move {
            let record = serde_json::to_string(&AliasRecord {
                session_key: session.session_key.expose_to_store().to_owned(),
                follow_only: session.follow_only,
                root_session_key: session
                    .root_session_key
                    .as_ref()
                    .map(|key| key.expose_to_store().to_owned()),
            })
            .map_err(|_| provider_invalid("encode session alias"))?;
            redis::Script::new("local current = redis.call('GET', KEYS[1]); if not current or current == ARGV[1] then redis.call('PSETEX', KEYS[1], tonumber(ARGV[3]), ARGV[2]); return 1 end; return 0")
                .key(format!("{}:alias", self.key(provider, alias)?))
                // 缺失与同目标均可写入，冲突不能覆盖另一会话
                .arg(&record)
                .arg(&record)
                .arg(session_affinity_ttl_millis(ttl)?)
                .invoke_async::<bool>(&mut self.connection.clone()).await
                .map_err(|source| crate::provider_unavailable("bind session alias", source))
        })
    }
}

fn session_affinity_ttl_millis(ttl: Duration) -> Result<u64, ProviderStoreError> {
    if ttl.is_zero() || ttl > MAX_SESSION_AFFINITY_TTL {
        return Err(provider_invalid("validate provider session affinity TTL"));
    }
    u64::try_from(ttl.as_millis())
        .map_err(|_| provider_invalid("validate provider session affinity TTL"))
}

fn provider_invalid(operation: &'static str) -> ProviderStoreError {
    ProviderStoreError::new(ProviderStoreErrorKind::InvalidData, operation)
}


#[cfg(test)]
mod hot_profile_tests {
    use super::*;

    #[tokio::test]
    async fn hot_profile_legacy_bridge() {
        let url = std::env::var("CPR_TEST_REDIS_URL").expect("isolated Redis required");
        let mut connection = redis::Client::open(url).unwrap().get_connection_manager().await.unwrap();
        let namespace = format!("hot-bridge-{}", uuid::Uuid::new_v4());
        let repo = RedisProviderSessionAffinityRepository::new(connection.clone(), &namespace).unwrap();
        let provider = ProviderKind::new("openai").unwrap();
        let key = ProviderSessionAffinityKey::try_new("legacy-session").unwrap();
        let a = ProviderAccountId::new("acct_a").unwrap();
        let b = ProviderAccountId::new("acct_b").unwrap();
        let legacy = repo.key(&provider, &key).unwrap();
        let companion = format!("{legacy}:binding");
        let ttl = Duration::from_secs(60);
        let _: () = redis::cmd("PSETEX").arg(&legacy).arg(60_000).arg(a.as_str()).query_async(&mut connection).await.unwrap();
        let first = repo.load(&provider, &key).await.unwrap().unwrap();
        assert_eq!(first.account_id(), &a);
        assert_eq!(repo.load(&provider, &key).await.unwrap(), Some(first.clone()));
        let legacy_ttl: i64 = redis::cmd("PTTL").arg(&legacy).query_async(&mut connection).await.unwrap();
        let companion_ttl: i64 = redis::cmd("PTTL").arg(&companion).query_async(&mut connection).await.unwrap();
        assert!((legacy_ttl - companion_ttl).abs() < 100);
        let renewed = repo.compare_and_bind(&provider, &key, Some(&first), &a, ttl).await.unwrap().unwrap();
        assert_eq!(renewed, first);
        // An old writer's account change invalidates the new reader's snapshot.
        let _: () = redis::cmd("PSETEX").arg(&legacy).arg(20_000).arg(b.as_str()).query_async(&mut connection).await.unwrap();
        assert!(repo.compare_and_bind(&provider, &key, Some(&first), &a, ttl).await.unwrap().is_none());
        let second = repo.load(&provider, &key).await.unwrap().unwrap();
        assert_eq!(second.account_id(), &b);
        assert_ne!(first.revision(), second.revision());
        let legacy_ttl: i64 = redis::cmd("PTTL").arg(&legacy).query_async(&mut connection).await.unwrap();
        assert!((1..=20_000).contains(&legacy_ttl));
        // Old clear wins; stale companion/expected record cannot resurrect it.
        let _: u64 = redis::cmd("DEL").arg(&legacy).query_async(&mut connection).await.unwrap();
        assert!(repo.compare_and_bind(&provider, &key, Some(&second), &b, ttl).await.unwrap().is_none());
        assert!(repo.load(&provider, &key).await.unwrap().is_none());
        let exists: bool = redis::cmd("EXISTS").arg(&companion).query_async(&mut connection).await.unwrap();
        assert!(!exists);
        let (one, two) = tokio::join!(
            repo.compare_and_bind(&provider, &key, None, &a, ttl),
            repo.compare_and_bind(&provider, &key, None, &b, ttl)
        );
        let winners: Vec<_> = [one.unwrap(), two.unwrap()].into_iter().flatten().collect();
        assert_eq!(winners.len(), 1);
        let raw: String = redis::cmd("GET").arg(&legacy).query_async(&mut connection).await.unwrap();
        assert_eq!(raw, winners[0].account_id().as_str());
        assert_eq!(repo.load(&provider, &key).await.unwrap().as_ref(), winners.first());
        // Expiry of the canonical key is equivalent to old DEL.
        let _: bool = redis::cmd("PEXPIRE").arg(&legacy).arg(0).query_async(&mut connection).await.unwrap();
        assert!(repo.load(&provider, &key).await.unwrap().is_none());
        let exists: bool = redis::cmd("EXISTS").arg(&companion).query_async(&mut connection).await.unwrap();
        assert!(!exists);
    }
}
