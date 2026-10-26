//! Redis 共享读取缓存。
//!
//! Redis 只保存可重建的页面结果，PostgreSQL 仍是唯一事实来源。未配置或暂时连不上 Redis
//! 时，调用方应继续走进程内缓存或数据库查询，不能因为缓存故障影响页面请求。

use std::sync::Arc;

use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::Serialize;
use serde::de::DeserializeOwned;

const DEFAULT_TTL_SECONDS: u64 = 15 * 60;
// 本地 Redis 的正常响应在 1 毫秒以内；留 1 秒容忍容器网络抖动和瞬时排队，
// 只有真的连不上才回退到数据库查询，避免抖动时整页都走数据库。
const CACHE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone)]
pub struct Cache {
    client: Option<redis::Client>,
    connection: Arc<tokio::sync::Mutex<Option<ConnectionManager>>>,
}

impl Cache {
    pub fn new(url: Option<&str>) -> anyhow::Result<Self> {
        let client = url
            .map(redis::Client::open)
            .transpose()
            .map_err(|e| anyhow::anyhow!("Redis URL 无效：{e}"))?;
        if client.is_some() {
            tracing::info!("Redis 共享读取缓存已配置");
        }
        Ok(Self {
            client,
            connection: Arc::default(),
        })
    }

    pub fn enabled(&self) -> bool {
        self.client.is_some()
    }

    async fn connection(&self) -> anyhow::Result<Option<ConnectionManager>> {
        let Some(client) = &self.client else {
            return Ok(None);
        };
        let mut guard = self.connection.lock().await;
        if let Some(connection) = guard.as_ref() {
            return Ok(Some(connection.clone()));
        }
        let connection = ConnectionManager::new(client.clone())
            .await
            .map_err(|e| anyhow::anyhow!("连接 Redis 失败：{e}"))?;
        *guard = Some(connection.clone());
        Ok(Some(connection))
    }

    async fn timed_connection(&self) -> anyhow::Result<Option<ConnectionManager>> {
        tokio::time::timeout(CACHE_TIMEOUT, self.connection())
            .await
            .map_err(|_| anyhow::anyhow!("连接 Redis 超时"))?
    }

    pub async fn get_json<T: DeserializeOwned>(&self, key: &str) -> anyhow::Result<Option<T>> {
        let Some(mut connection) = self.timed_connection().await? else {
            return Ok(None);
        };
        let raw: Option<String> = tokio::time::timeout(CACHE_TIMEOUT, connection.get(key))
            .await
            .map_err(|_| anyhow::anyhow!("读取 Redis 缓存超时"))?
            .map_err(|e| anyhow::anyhow!("读取 Redis 缓存失败：{e}"))?;
        raw.map(|value| {
            serde_json::from_str(&value).map_err(|e| anyhow::anyhow!("解析 Redis 缓存失败：{e}"))
        })
        .transpose()
    }

    pub async fn set_json<T: Serialize>(&self, key: &str, value: &T) -> anyhow::Result<()> {
        self.set_json_with_ttl(key, value, DEFAULT_TTL_SECONDS)
            .await
    }

    pub async fn set_json_with_ttl<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl_seconds: u64,
    ) -> anyhow::Result<()> {
        let Some(mut connection) = self.timed_connection().await? else {
            return Ok(());
        };
        let raw = serde_json::to_string(value)?;
        tokio::time::timeout(
            CACHE_TIMEOUT,
            connection.set_ex::<_, _, ()>(key, raw, ttl_seconds),
        )
        .await
        .map_err(|_| anyhow::anyhow!("写入 Redis 缓存超时"))?
        .map_err(|e| anyhow::anyhow!("写入 Redis 缓存失败：{e}"))?;
        Ok(())
    }
}

/// 依赖派生数据的页面结果的键：`version` 是数据版本（例如 `CacheVersions::market_token`），
/// 版本变了自然换成新键，旧键由 TTL 回收，不做删除
pub fn derived_key(name: &str, version: &str, suffix: &str) -> String {
    format!("niulai:v1:{name}:{version}:{suffix}")
}

impl Cache {
    /// 读缓存，没有就计算并回填。缓存读写失败只记调试日志、照常返回计算结果：
    /// 页面请求和后台预热都走这一个入口，键格式、TTL 和降级只在这里
    pub async fn cached_json<T, E, F, Fut>(&self, key: &str, compute: F) -> Result<T, E>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        match self.get_json(key).await {
            Ok(Some(hit)) => {
                tracing::debug!(cache = "redis", key, "命中共享缓存");
                return Ok(hit);
            }
            Ok(None) => {}
            Err(e) => tracing::debug!(error = %e, key, "读取共享缓存失败，继续计算"),
        }
        let value = compute().await?;
        if let Err(e) = self.set_json(key, &value).await {
            tracing::debug!(error = %e, key, "写入共享缓存失败，继续返回计算结果");
        }
        Ok(value)
    }

    /// 预热：键已经存在时不计算，返回是否写入了新结果
    pub async fn warm_json<T, F, Fut>(&self, key: &str, compute: F) -> anyhow::Result<bool>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<Option<T>>>,
    {
        if self.get_json::<serde_json::Value>(key).await?.is_some() {
            return Ok(false);
        }
        let Some(value) = compute().await? else {
            return Ok(false);
        };
        self.set_json(key, &value).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_cache_still_computes() {
        let cache = Cache::new(None).unwrap();
        let v: Result<i32, std::convert::Infallible> =
            cache.cached_json("niulai:test", || async { Ok(7) }).await;
        assert_eq!(v.unwrap(), 7);
        assert_eq!(
            derived_key("market:hot", "adj=None:limit=None", "2026-10-16"),
            "niulai:v1:market:hot:adj=None:limit=None:2026-10-16"
        );
    }

    #[test]
    fn cache_without_url_is_disabled_and_safe() {
        let cache = Cache::new(None).unwrap();
        assert!(!cache.enabled());
    }

    #[test]
    fn invalid_url_is_rejected_without_logging_credentials() {
        let result = Cache::new(Some("not a redis url"));
        assert!(result.is_err());
        assert!(
            result
                .err()
                .is_some_and(|error| error.to_string().contains("Redis URL 无效"))
        );
    }

    #[tokio::test]
    #[ignore = "需要显式配置 NIULAI_REDIS_TEST_URL"]
    async fn redis_round_trip() {
        let url = std::env::var("NIULAI_REDIS_TEST_URL").expect("NIULAI_REDIS_TEST_URL");
        let cache = Cache::new(Some(&url)).unwrap();
        let key = format!("niulai:test:cache:{}", std::process::id());
        let value = serde_json::json!({"status": "ok", "n": 1});
        cache.set_json_with_ttl(&key, &value, 30).await.unwrap();
        assert_eq!(
            cache.get_json::<serde_json::Value>(&key).await.unwrap(),
            Some(value)
        );
    }
}
