//! Web 模式别名与目录投影；路由授权仍由原生模型的 owner 判断。

use serde_json::{Map, Value, json};

pub(super) const LOGICAL_MODELS: [&str; 2] = ["chatgpt-web/gpt-5.6", "chatgpt-web/gpt-6"];
const STRENGTHS: [(&str, &str, &str); 3] = [
    ("high", "sol", "high"),
    ("xhigh", "sol", "xhigh"),
    ("pro", "pro", "max"),
];

pub(super) const PUBLIC_MODELS: [&str; 8] = [
    "chatgpt-web/gpt-5.6-chat-persist",
    "chatgpt-web/gpt-5.6-chat-temp",
    "chatgpt-web/gpt-5.6-tools-persist",
    "chatgpt-web/gpt-5.6-tools-temp",
    "chatgpt-web/gpt-6-chat-persist",
    "chatgpt-web/gpt-6-chat-temp",
    "chatgpt-web/gpt-6-tools-persist",
    "chatgpt-web/gpt-6-tools-temp",
];

fn public_mode(model: &str) -> Option<(&str, &str, &str)> {
    if !PUBLIC_MODELS.contains(&model) {
        return None;
    }
    let (model, persistence) = model.rsplit_once('-')?;
    let (model, mode) = model.rsplit_once('-')?;
    Some((model, mode, persistence))
}

pub(super) fn source_models(model: &str) -> Option<[String; 2]> {
    let (model, mode, _) = public_mode(model)?;
    let tools = if mode == "tools" { "-tools" } else { "" };
    Some(["sol", "pro"].map(|backend| format!("{model}-{backend}{tools}")))
}

/// 在生成路由事实前改写正文，使账号、分组和 key 检查消费同一个实际模型。
pub(super) fn normalize_request(object: &mut Map<String, Value>) -> Result<(), &'static str> {
    let Some(model) = object.get("model").and_then(Value::as_str) else {
        return Ok(());
    };
    let alias = public_mode(model);
    if alias.is_none() && !LOGICAL_MODELS.contains(&model) {
        return Ok(());
    }
    let reasoning = match object.get("reasoning") {
        None => None,
        Some(Value::Object(reasoning)) => Some(reasoning),
        Some(_) => return Err("reasoning"),
    };
    let effort = match reasoning.and_then(|reasoning| reasoning.get("effort")) {
        None => {
            if alias.is_some() {
                "xhigh"
            } else {
                "xhigh-tools-persist"
            }
        }
        Some(Value::String(effort)) => effort.as_str(),
        Some(_) => return Err("reasoning.effort"),
    };
    let legacy_effort;
    let (model, effort) = if let Some((model, mode, persistence)) = alias {
        let strength = match effort {
            "high" | "xhigh" => effort,
            "max" => "pro",
            _ => return Err("reasoning.effort"),
        };
        legacy_effort = format!("{strength}-{mode}-{persistence}");
        (model, legacy_effort.as_str())
    } else {
        (model, effort)
    };
    let parts = effort.split('-').collect::<Vec<_>>();
    let [strength, mode, persistence] = parts.as_slice() else {
        return Err("reasoning.effort");
    };
    let Some((_, backend, upstream_effort)) = STRENGTHS.iter().find(|row| row.0 == *strength)
    else {
        return Err("reasoning.effort");
    };
    let tools = match *mode {
        "chat" => "",
        "tools" => "-tools",
        _ => return Err("reasoning.effort"),
    };
    let temporary = match *persistence {
        "persist" => "",
        "temp" => "-temporary",
        _ => return Err("reasoning.effort"),
    };
    let upstream_effort = format!("{upstream_effort}{temporary}");
    object.insert(
        "model".to_owned(),
        json!(format!("{model}-{backend}{tools}")),
    );
    object
        .entry("reasoning")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("reasoning was checked")
        .insert("effort".to_owned(), json!(upstream_effort));
    Ok(())
}

/// 输入只含当前 key 已授权的原生目录；没有来源的模式和强度不进入菜单。
pub(super) fn synthesize_models(native_models: &[&Value]) -> Vec<Value> {
    PUBLIC_MODELS
        .iter()
        .enumerate()
        .filter_map(|(priority, alias)| {
            let (model, mode, persistence) = public_mode(alias).expect("static Web mode");
            let suffix = if mode == "tools" { "-tools" } else { "" };
            let mut levels = Vec::new();
            let mut sources = Vec::new();
            for (_, backend, upstream_effort) in STRENGTHS {
                let slug = format!("{model}-{backend}{suffix}");
                let Some(source) = native_models
                    .iter()
                    .copied()
                    .find(|source| source["slug"] == slug)
                else {
                    continue;
                };
                let supported =
                    source["supported_reasoning_levels"]
                        .as_array()
                        .is_some_and(|levels| {
                            levels
                                .iter()
                                .any(|level| level["effort"] == upstream_effort)
                        });
                if !supported {
                    continue;
                }
                sources.push(source);
                levels.push(json!({
                    "effort": upstream_effort,
                    "description": if upstream_effort == "max" { "Pro" } else { upstream_effort },
                }));
            }
            if levels.is_empty() {
                return None;
            }
            // 每个模式只继承对应聊天或工具来源的能力和预算。
            let mut merged = sources[0].clone();
            merged["slug"] = json!(alias);
            merged["display_name"] = json!(format!(
                "GPT-{} (Web) · {} · {}",
                model.trim_start_matches("chatgpt-web/gpt-"),
                if mode == "tools" { "Tools" } else { "Chat" },
                if persistence == "persist" {
                    "Persist"
                } else {
                    "Temporary"
                }
            ));
            merged["description"] = json!(format!(
                "{mode}; {}; high / xhigh / max (Pro)",
                if persistence == "persist" {
                    "save chat history"
                } else {
                    "temporary chat"
                }
            ));
            merged["visibility"] = json!("list");
            merged["priority"] = json!(priority);
            merged["default_reasoning_level"] = levels
                .iter()
                .find(|level| level["effort"] == "xhigh")
                .unwrap_or(&levels[0])["effort"]
                .clone();
            merged["supported_reasoning_levels"] = json!(levels);
            for field in [
                "context_window",
                "max_context_window",
                "auto_compact_token_limit",
                "effective_context_window_percent",
            ] {
                let minimum = sources
                    .iter()
                    .filter_map(|source| source[field].as_u64())
                    .min();
                if let Some(minimum) = minimum {
                    merged[field] = json!(minimum);
                }
            }
            Some(merged)
        })
        .collect()
}
