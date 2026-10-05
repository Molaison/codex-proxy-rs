//! 原生实时语音 HTTP/WS 传输复用所选账号的 HTTP、TLS 和代理客户端。
use super::{CodexBackendClient, CodexRequestContext};
use bytes::Bytes;
use gateway_core::{
    error::{ClientVisibleUpstreamResponse, ProviderError, ProviderErrorKind, RawUpstreamError},
    upstream::UpstreamSendState,
};
use reqwest::{
    Response,
    header::{HeaderMap, HeaderValue},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        handshake::{client::generate_key, derive_accept_key},
        protocol::Role,
    },
};

pub(crate) fn failure(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Unavailable, UpstreamSendState::Sent)
        .with_raw_upstream_error(RawUpstreamError::new(message))
}
async fn reject(response: Response) -> ProviderError {
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.as_bytes().to_vec());
    let body = response.bytes().await.unwrap_or_default();
    let kind = match status.as_u16() {
        400 | 404 | 422 => ProviderErrorKind::InvalidRequest,
        401 | 403 => ProviderErrorKind::Unauthorized,
        429 => ProviderErrorKind::RateLimited,
        _ => ProviderErrorKind::Unavailable,
    };
    ProviderError::new(kind, UpstreamSendState::Sent)
        .with_status(status.as_u16())
        .with_raw_upstream_error(RawUpstreamError::new(
            String::from_utf8_lossy(&body).into_owned(),
        ))
        .with_client_visible_upstream_response(ClientVisibleUpstreamResponse::new(
            status.as_u16(),
            content_type,
            body,
        ))
}
impl CodexBackendClient {
    pub(crate) fn live_headers(
        &self,
        context: CodexRequestContext<'_>,
        extra: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<HeaderMap, ProviderError> {
        let mut headers = self
            .account_request_headers(context)
            .map_err(|e| failure(e.to_string()))?;
        headers.insert("openai-alpha", HeaderValue::from_static("quicksilver=v2"));
        // 证明字段原样传递，不补造证明，也不复制客户端认证或 cookie。
        for name in [
            "x-oai-attestation",
            "x-oai-attestation-token",
            "x-session-id",
            "session-id",
            "thread-id",
            "originator",
        ] {
            if let Some(value) = extra.get(name).and_then(serde_json::Value::as_str) {
                headers.insert(
                    reqwest::header::HeaderName::from_static(name),
                    HeaderValue::from_str(value).map_err(|_| failure("invalid live header"))?,
                );
            }
        }
        Ok(headers)
    }
    pub(crate) async fn live_create(
        &self,
        body: Bytes,
        headers: HeaderMap,
    ) -> Result<(u16, String, Bytes), ProviderError> {
        let url = live_create_url(&self.base_url);
        let response = self
            .client
            .post(url)
            .headers(headers)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| failure(e.to_string()))?;
        if !response.status().is_success() {
            return Err(reject(response).await);
        }
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get("location")
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| failure("Live create response has no Location"))?;
        let call_id = location
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        if !call_id.starts_with("rtc_")
            || !call_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
            return Err(failure("invalid Live call identifier"));
        }
        let body = response.bytes().await.map_err(|e| failure(e.to_string()))?;
        Ok((status, call_id, body))
    }
    pub(crate) async fn live_connect(
        &self,
        call_id: Option<&str>,
        model: Option<&str>,
        mut headers: HeaderMap,
    ) -> Result<WebSocketStream<reqwest::Upgraded>, ProviderError> {
        // Codex OAuth 的 WebRTC 创建与 sideband 按原生协议使用不同的可信主机。
        let mut url = url::Url::parse("https://api.openai.com/v1/live")
            .map_err(|e| failure(e.to_string()))?;
        if let Some(call_id) = call_id {
            url.path_segments_mut()
                .map_err(|_| failure("invalid Live URL"))?
                .push(call_id);
        } else if let Some(model) = model {
            url.query_pairs_mut().append_pair("model", model);
        }
        headers.remove("cookie");
        let key = generate_key();
        let expected = derive_accept_key(key.as_bytes());
        let response = self
            .client
            .get(url)
            .version(reqwest::Version::HTTP_11)
            .headers(headers)
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", key)
            .send()
            .await
            .map_err(|e| failure(e.to_string()))?;
        if response.status().as_u16() != 101 {
            return Err(reject(response).await);
        }
        if response
            .headers()
            .get("sec-websocket-accept")
            .and_then(|v| v.to_str().ok())
            != Some(expected.as_str())
        {
            return Err(failure("invalid Live WebSocket accept"));
        }
        let upgraded = response
            .upgrade()
            .await
            .map_err(|e| failure(e.to_string()))?;
        Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await)
    }
}

fn live_create_url(base_url: &str) -> String {
    super::endpoints::endpoint_url(
        base_url,
        "/codex/realtime/calls?intent=quicksilver&architecture=avas",
    )
}
#[cfg(test)]
mod tests {
    #[test]
    fn live_create_preserves_codex_backend_prefix() {
        assert_eq!(
            super::live_create_url("https://chatgpt.com/backend-api/"),
            "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas"
        );
    }
}
