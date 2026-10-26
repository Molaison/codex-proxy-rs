//! PostgreSQL 迁移屏障、连接池与会话级资源预算

use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

use crate::{
    POSTGRES_IDLE_TRANSACTION_TIMEOUT, POSTGRES_LOCK_TIMEOUT, POSTGRES_STATEMENT_TIMEOUT,
    StoreError, StorePoolConfig, StoreResult, postgres_unavailable,
};



/// 建立 PostgreSQL pool 并只执行冻结的 migration 集
pub async fn connect_and_migrate(
    database_url: &str,
    pool_config: StorePoolConfig,
) -> StoreResult<PgPool> {
    if std::env::var("CPR_SCHEMA_PROFILE").as_deref() != Ok("ywl-hot-319-328-v1") {
        return Err(hot_schema_error("this dedicated image requires CPR_SCHEMA_PROFILE=ywl-hot-319-328-v1"));
    }
    if database_url.trim().is_empty() {
        return Err(StoreError::InvalidData {
            source: None,
            entity: "PostgreSQL configuration",
            message: "database URL is empty".to_owned(),
        });
    }
    pool_config.validate()?;
    let connect_options = database_url
        .parse::<PgConnectOptions>()
        .map_err(|source| postgres_unavailable("parse PostgreSQL connection options", source))?;
    let migration_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            connect_options
                .clone()
                .application_name("codex-proxy-rs:migration"),
        )
        .await
        .map_err(|source| postgres_unavailable("connect PostgreSQL for migrations", source))?;
    validate_hot_schema(&migration_pool).await?;
    migration_pool.close().await;

    connect_pool(connect_options, pool_config, false).await
}

/// 帮助查询不执行迁移，并用连接默认只读事务阻止意外业务写入
pub(crate) async fn connect_read_only(
    database_url: &str,
    pool_config: StorePoolConfig,
) -> StoreResult<PgPool> {
    pool_config.validate()?;
    let options = database_url
        .parse::<PgConnectOptions>()
        .map_err(|source| postgres_unavailable("parse PostgreSQL connection options", source))?;
    connect_pool(options, pool_config, true).await
}

async fn connect_pool(
    connect_options: PgConnectOptions,
    pool_config: StorePoolConfig,
    read_only: bool,
) -> StoreResult<PgPool> {
    let statement_timeout = postgres_duration_setting(POSTGRES_STATEMENT_TIMEOUT);
    let lock_timeout = postgres_duration_setting(POSTGRES_LOCK_TIMEOUT);
    let idle_in_transaction_session_timeout =
        postgres_duration_setting(POSTGRES_IDLE_TRANSACTION_TIMEOUT);
    let pool = PgPoolOptions::new()
        .max_connections(pool_config.max_connections)
        .acquire_timeout(std::time::Duration::from_secs(
            pool_config.acquire_timeout_seconds,
        ))
        .after_connect(move |connection, _metadata| {
            let statement_timeout = statement_timeout.clone();
            let lock_timeout = lock_timeout.clone();
            let idle_in_transaction_session_timeout = idle_in_transaction_session_timeout.clone();
            Box::pin(async move {
                sqlx::query(
                    "select set_config('statement_timeout', $1, false),
                            set_config('lock_timeout', $2, false),
                            set_config('idle_in_transaction_session_timeout', $3, false),
                            set_config('default_transaction_read_only', $4, false),
                            set_config('TimeZone', 'UTC', false)",
                )
                .bind(statement_timeout)
                .bind(lock_timeout)
                .bind(idle_in_transaction_session_timeout)
                .bind(if read_only { "on" } else { "off" })
                .execute(connection)
                .await?;
                Ok(())
            })
        })
        .connect_with(connect_options.application_name("codex-proxy-rs"))
        .await
        .map_err(|source| postgres_unavailable("connect PostgreSQL", source))?;
    Ok(pool)
}

fn postgres_duration_setting(duration: std::time::Duration) -> String {
    format!("{}ms", duration.as_millis())
}

fn hot_schema_error(message: &str) -> StoreError {
    StoreError::InvalidData { source: None, entity: "hot schema compatibility", message: message.to_owned() }
}

async fn validate_hot_schema(pool: &PgPool) -> StoreResult<()> {
    let ready: bool = sqlx::query_scalar(
        "select exists(select 1 from public.cpr_schema_compat where singleton = true and profile = $1 and ready = true)")
        .bind("ywl-hot-319-328-v1").fetch_one(pool).await
        .map_err(|source| postgres_unavailable("validate hot compatibility marker", source))?;
    if !ready { return Err(hot_schema_error("hot compatibility marker is not ready")); }
    let actual: Vec<(i64, String, bool)> = sqlx::query_as(
        "select version, encode(checksum, 'hex'), success from public._sqlx_migrations order by version")
        .fetch_all(pool).await
        .map_err(|source| postgres_unavailable("validate preserved SQLx history", source))?;
    let expected: &[(i64, &str)] = &[
        (1, "66daf12a89a1ec11a81110c1ce6289f110bf65beaa432f2668a2bddc6fd246933138dba2d70a60a3d29c6aa6b2e631c1"),
        (2, "9c29e6c73340926f312b58af3b58b5be43b021ccc77797b4ebbf016cac6da802948d99f8e4a324cd1113184698e7ae8a"),
        (3, "8a0ed3ad9db3132dfefe7bc60dc78558c6e1fc687575ec8f21c0a0e27c5aa0866fd1fbdaaa66a6ad82be0816ae41efc8"),
        (4, "5379ecf2efdcc4c68c48a083284f38cef3be3e27cba46bf86d919649ab1622b4f86e4bbdae031758da1db56b2dd73860"),
        (5, "a0691a7cb853a9e03abc94b0e3564384dedecfe8e6b68deaba4d059a7a9863764b28daa6dadacf24b53775c8cdeaa9b6"),
        (6, "1318527338be047862132d597ea2cb4e241a4de71eb408325635a33f7f8bd1a547b7acfef1fd896edbf239bf890449af"),
        (7, "c4833587df037fbd3475b14431c6159404fe315953c7c0bdc35da8d359922259417e3a998454327ea079f399e19c9a9f"),
        (8, "1bb51e09f04ddecf09072359e162e0cc464b966573aa263c550e1b3f455e4d3c1123d75d5804bdfc24d6d39914a7a103"),
        (9, "975ccd8651930dc68ab1880023700358b38788d6a6ff7e47c0ee655e043bd6b39ada118ae11b8df15ec1bfbb501ac8f7"),
        (10, "65239a803454864d6c3bc9cd12ad8a144119a1d3e66701c91a5f39a3b310060b830cf6d7eebc2882db12a72d1c05762e"),
        (11, "53f2e8ed90475d91e06f47f42b7c0b255758d6d87ea97eb7d9922ca947f132249951ec0d329c4a2967111e275200e2e2"),
        (12, "56c34c788d3ce23da2db8f2f8075806d7412c3d5c1f575e55a926c2570b918762cbe3a22487a7b0aadca9093dc3421ad"),
        (13, "80700f493b474ea4680dcd0c1743b25b5d26bc88ef66ecbb9fdc73e4fa15d1bcf9ba576c77e1660437530cfc489bc94a"),
        (14, "3237650cf0b58bd3fd3ae4d406ff669e392c24480b04ef890a39e9d1027952cb46d128606487adf43f8c21d32a88351b"),
        (15, "51b9b0f1a0a393bbcb85ed2247356be4c3fb3156abc60cbb6fd67bac31bbd329b027cb49117467d12aa3871d368e41e6"),
        (16, "6cb43e8906126bef733dc30049560d070b3516bf45aaddb0db5301351f73c0b75091cb979c028482d5880c6e24b6eb87"),
        (17, "ef9237262fa8af0ceba54d25a0cd40eb48fb9a2b74176c0c61d5c7b58a0fbfb3735ffc18509dcfce045f7595383eb1ac"),
        (18, "fcd1405df2f2e5178ad363a9bf0e9b249ee418af7c29f8c20a8992f52eb15895f7b025036bf73510aadb9ba433c5af2e"),
        (19, "1d564b11efda148812d6582b50e7c34722f28fe9e2ae8ad40ba5051e246f7e63344fdb40aa5043556abddcc4a44036ea"),
        (20, "b02df8bc9f468b18c74e188b165414fbd55341d0b185fb1cbc44460ed5c1ce9af33ee1b76dd3086978380f36c9c76be7"),
        (21, "0e674b705b653e49384d51fb8e998696cbac58e8cc6e10bd5c86df970c2bfcc4e9eb0be45cd86502915ef46520f4005c"),
        (22, "0fbcde03f0607aa735e0273444d443bac8dc6584ad73a21602635c2460a66a2be37721d3f79b69c1282cef5720797271")
    ];
    if actual.len() != expected.len() || actual.iter().zip(expected).any(|((v, h, ok), (ev, eh))| v != ev || h != eh || !ok) {
        return Err(hot_schema_error("SQLx history does not match the original 3.19 migrations 1..22"));
    }

    let extension_ready: bool = sqlx::query_scalar(
        "select count(*) = 5 from information_schema.columns where table_schema = 'public' and
         ((table_name = 'outbound_proxies' and column_name = 'max_websocket_connections' and data_type = 'bigint') or
          (table_name = 'runtime_settings' and column_name in ('reset_credit_lead_minutes', 'reset_credit_max_weekly_remaining_percent') and data_type = 'integer') or
          (table_name = 'provider_accounts' and column_name = 'reset_credit_automation_json' and data_type = 'jsonb') or
          (table_name = 'provider_accounts' and column_name = 'reset_credit_next_run_at' and data_type = 'timestamp with time zone'))")
        .fetch_one(pool).await
        .map_err(|source| postgres_unavailable("validate 3.29 schema extension columns", source))?;
    if !extension_ready {
        return Err(hot_schema_error("3.29 schema extension columns are missing or have unexpected types; apply compatible additive SQL before starting"));
    }
    Ok(())
}
