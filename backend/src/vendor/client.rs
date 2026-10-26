//! 同花顺金融数据服务客户端。每次调用只发一次请求；重试由执行器负责，以便每次尝试都写入调用日志。
//!
//! 响应约定（官方文档）：HTTP 200 且 `code == 0` 才算成功；`data` 在错误时为 null。

use std::sync::RwLock;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::Value;

use super::RateLimiter;

pub struct HithinkClient {
    http: reqwest::Client,
    base_url: String,
    /// 可以在运行中设置或更换（首次登录引导中配置），见 docs/decisions/api_key_setup.md
    api_key: RwLock<Option<String>>,
    limiter: RateLimiter,
}

/// 一次成功调用的结果
#[derive(Debug)]
pub struct CallResult {
    pub http_status: u16,
    pub body: Value,
    pub bytes: usize,
    pub latency_ms: i64,
}

impl CallResult {
    pub fn code(&self) -> Option<i64> {
        self.body.get("code").and_then(Value::as_i64)
    }
    pub fn request_id(&self) -> Option<&str> {
        self.body.get("request_id").and_then(Value::as_str)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// 可以退避后重试：限流、服务端异常、数据未就绪、网络错误、非 JSON 响应
    #[error("可重试错误（code={code:?}，http={http_status:?}）：{message}")]
    Retryable {
        code: Option<i64>,
        http_status: Option<u16>,
        message: String,
        latency_ms: i64,
    },
    /// 调用方错误或无权限，重试无意义：1xxx、2xxx、3001、3004 等
    #[error("不可重试错误（code={code:?}，http={http_status:?}）：{message}")]
    Fatal {
        code: Option<i64>,
        http_status: Option<u16>,
        message: String,
        request_id: Option<String>,
        latency_ms: i64,
    },
}

impl CallError {
    pub fn code(&self) -> Option<i64> {
        match self {
            CallError::Retryable { code, .. } | CallError::Fatal { code, .. } => *code,
        }
    }
    pub fn http_status(&self) -> Option<u16> {
        match self {
            CallError::Retryable { http_status, .. } | CallError::Fatal { http_status, .. } => {
                *http_status
            }
        }
    }
    pub fn latency_ms(&self) -> i64 {
        match self {
            CallError::Retryable { latency_ms, .. } | CallError::Fatal { latency_ms, .. } => {
                *latency_ms
            }
        }
    }
    pub fn is_retryable(&self) -> bool {
        matches!(self, CallError::Retryable { .. })
    }
}

// @project-doc docs/interfaces/hithink_api.md#error_handling
/// 官方错误码 4001：被限速。命中时让限速器降速，其余可重试错误不调整速率。
const THROTTLE_CODE: i64 = 4001;

/// 官方错误码表：4001 限流、5001–5003 服务端或上游异常、3002 / 4040（导出文件）数据未就绪 → 可重试；其余不可重试。
fn retryable_code(code: i64) -> bool {
    matches!(code, 3002 | 4001 | 4040 | 5001 | 5002 | 5003)
}

impl HithinkClient {
    pub fn new(
        base_url: &str,
        api_key: Option<&str>,
        timeout: Duration,
        limiter: RateLimiter,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("niulai/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let api_key = RwLock::new(api_key.map(str::to_string));
        Ok(HithinkClient {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            limiter,
        })
    }

    pub fn has_api_key(&self) -> bool {
        self.api_key
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub fn set_api_key(&self, key: Option<String>) {
        *self.api_key.write().unwrap_or_else(|e| e.into_inner()) = key;
    }

    pub fn limiter(&self) -> &RateLimiter {
        &self.limiter
    }

    /// GET JSON 端点，返回完整响应信封
    pub async fn get(
        &self,
        endpoint: &str,
        query: &[(String, String)],
    ) -> Result<CallResult, CallError> {
        let Some(api_key) = self
            .api_key
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return Err(CallError::Fatal {
                code: None,
                http_status: None,
                message: "还没有配置 API Key".into(),
                request_id: None,
                latency_ms: 0,
            });
        };
        self.request(&api_key, endpoint, query).await
    }

    /// 用一个候选 Key 请求一次接口，不改变当前使用的 Key。用于保存前验证
    pub async fn verify_key(&self, api_key: &str, endpoint: &str) -> Result<CallResult, CallError> {
        self.request(api_key, endpoint, &[]).await
    }

    async fn request(
        &self,
        api_key: &str,
        endpoint: &str,
        query: &[(String, String)],
    ) -> Result<CallResult, CallError> {
        self.limiter.acquire().await;
        let started = Instant::now();
        let elapsed = |s: Instant| s.elapsed().as_millis() as i64;
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, endpoint))
            .header("X-api-key", api_key)
            .query(query)
            .send()
            .await
            .map_err(|e| CallError::Retryable {
                code: None,
                http_status: None,
                message: format!("网络错误：{e}"),
                latency_ms: elapsed(started),
            })?;
        let status = resp.status().as_u16();
        let raw = resp.bytes().await.map_err(|e| CallError::Retryable {
            code: None,
            http_status: Some(status),
            message: format!("读取响应失败：{e}"),
            latency_ms: elapsed(started),
        })?;
        let latency_ms = elapsed(started);
        if status == 429 {
            self.limiter.on_throttled();
            return Err(CallError::Retryable {
                code: Some(4001),
                http_status: Some(status),
                message: "HTTP 429 限流".into(),
                latency_ms,
            });
        }
        let body: Value = serde_json::from_slice(&raw).map_err(|_| CallError::Retryable {
            code: None,
            http_status: Some(status),
            message: format!("非 JSON 响应（{} 字节）", raw.len()),
            latency_ms,
        })?;
        let code = body.get("code").and_then(Value::as_i64);
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match code {
            Some(0) if status == 200 => {
                self.limiter.on_success();
                Ok(CallResult {
                    http_status: status,
                    body,
                    bytes: raw.len(),
                    latency_ms,
                })
            }
            Some(c) if retryable_code(c) => {
                // 只有 4001 是真的被限速；5xx 那类服务端异常与限速无关，不调整速率
                if c == THROTTLE_CODE {
                    self.limiter.on_throttled();
                }
                Err(CallError::Retryable {
                    code,
                    http_status: Some(status),
                    message,
                    latency_ms,
                })
            }
            _ if status >= 500 => Err(CallError::Retryable {
                code,
                http_status: Some(status),
                message,
                latency_ms,
            }),
            _ => Err(CallError::Fatal {
                code,
                http_status: Some(status),
                message,
                request_id: body
                    .get("request_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                latency_ms,
            }),
        }
    }

    /// 下载预签名链接指向的文件（全市场导出）。链接约 5 分钟有效，拿到后立即下载。
    pub async fn download(&self, url: &str) -> Result<Bytes, CallError> {
        let started = Instant::now();
        let resp = self
            .http
            .get(url)
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .map_err(|e| CallError::Retryable {
                code: None,
                http_status: None,
                message: format!("下载失败：{e}"),
                latency_ms: started.elapsed().as_millis() as i64,
            })?;
        let status = resp.status().as_u16();
        if status != 200 {
            // 403 一般是预签名链接过期，重新签名即可
            return Err(CallError::Retryable {
                code: None,
                http_status: Some(status),
                message: format!("下载返回 HTTP {status}"),
                latency_ms: started.elapsed().as_millis() as i64,
            });
        }
        resp.bytes().await.map_err(|e| CallError::Retryable {
            code: None,
            http_status: Some(status),
            message: format!("下载中断：{e}"),
            latency_ms: started.elapsed().as_millis() as i64,
        })
    }
}
