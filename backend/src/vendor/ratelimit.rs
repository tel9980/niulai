//! 供应商接口限速：1 秒滑动窗口，在进程内计数。
//!
//! 免费 API Key 全局每秒只能请求约 2 次，多开进程也不会更快，所以同一时刻只让一个进程执行同步任务
//! （`Engine::lock_executor`），限速只需在进程内做，不需要 Redis。理由见 docs/decisions/in_process_rate_limit.md。
//!
//! 限速值会自适应：供应商返回 429（HTTP 429 或错误码 4001）时立刻把速率压到当前的一半，
//! 之后每连续成功 `RECOVER_AFTER_SUCCESSES` 次就回调 1，直到涨回配置的上限。这样配置的
//! `NIULAI_RATE_LIMIT_RPS` 是「上限」而不是「死值」——限额比预期宽松时能自动跑快，
//! 比预期严格时又不会一直撞 429。全程不写 Redis、不落盘，重启后从配置值重新开始。
//!
//! 只有真正「成功」（HTTP 200 且业务 code 为 0）才计入成功次数；网络错误、非 JSON、
//! 服务端 5xx 这类与被限速无关的失败既不加也不减，避免把网络抖动当成限流信号。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// 连续这么多次成功后，限速值 +1。
const RECOVER_AFTER_SUCCESSES: u64 = 40;
/// 自适应速率的下限：再低也不低于这个值，否则回补会慢到没有意义。
const MIN_RPS: u32 = 1;

// @project-doc docs/architecture/sync_pipeline.md#rate_limit
pub struct RateLimiter {
    /// 配置的上限，由 `NIULAI_RATE_LIMIT_RPS` 决定，只读。
    ceiling: u32,
    /// 当前生效的速率，会在 1..=ceiling 之间浮动。
    rps: AtomicU32,
    /// 距离下次提速还差多少次连续成功。
    successes: AtomicU64,
    window: Mutex<VecDeque<Instant>>,
}

impl RateLimiter {
    pub fn new(rps: u32) -> Self {
        let ceiling = rps.max(MIN_RPS);
        RateLimiter {
            ceiling,
            rps: AtomicU32::new(ceiling),
            successes: AtomicU64::new(0),
            window: Mutex::new(VecDeque::new()),
        }
    }

    /// 配置的上限
    pub fn ceiling(&self) -> u32 {
        self.ceiling
    }

    /// 当前生效的速率
    pub fn current(&self) -> u32 {
        self.rps.load(Ordering::Relaxed)
    }

    /// 等待直到允许发出一次请求
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut q = self.window.lock().await;
                let now = Instant::now();
                while q
                    .front()
                    .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1))
                {
                    q.pop_front();
                }
                let limit = self.current() as usize;
                if q.len() < limit {
                    q.push_back(now);
                    return;
                }
                Duration::from_secs(1) - now.duration_since(*q.front().unwrap())
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// 供应商回报被限速。把当前速率减半（不低于 `MIN_RPS`），并清空成功计数。
    /// 已经被压到下限时保持不动，避免反复写同一个值。
    pub fn on_throttled(&self) {
        self.successes.store(0, Ordering::Relaxed);
        let current = self.current();
        let next = (current / 2).max(MIN_RPS);
        if next < current {
            self.rps.store(next, Ordering::Relaxed);
            tracing::warn!(from = current, to = next, ceiling = self.ceiling, "供应商限流，已降低请求速率");
        }
    }

    /// 一次请求成功。连续成功满 `RECOVER_AFTER_SUCCESSES` 次就把速率 +1（不超过上限）。
    pub fn on_success(&self) {
        let current = self.current();
        if current >= self.ceiling {
            // 已在上限，不必再累积成功计数
            self.successes.store(0, Ordering::Relaxed);
            return;
        }
        let streak = self.successes.fetch_add(1, Ordering::Relaxed) + 1;
        if streak >= RECOVER_AFTER_SUCCESSES {
            self.successes.store(0, Ordering::Relaxed);
            let next = (current + 1).min(self.ceiling);
            if next > current {
                self.rps.store(next, Ordering::Relaxed);
                tracing::info!(from = current, to = next, ceiling = self.ceiling, "连续请求成功，已提升请求速率");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_ceiling_and_never_exceeds_it() {
        let l = RateLimiter::new(4);
        assert_eq!(l.current(), 4);
        assert_eq!(l.ceiling(), 4);
        for _ in 0..10_000 {
            l.on_success();
        }
        assert_eq!(l.current(), 4, "成功不会超过上限");
    }

    #[test]
    fn throttle_halves_but_not_below_min() {
        let l = RateLimiter::new(8);
        l.on_throttled();
        assert_eq!(l.current(), 4);
        l.on_throttled();
        assert_eq!(l.current(), 2);
        l.on_throttled();
        assert_eq!(l.current(), 1);
        l.on_throttled();
        assert_eq!(l.current(), 1, "已经在下限就不再降低");
    }

    #[test]
    fn recovers_one_step_after_enough_successes() {
        let l = RateLimiter::new(8);
        l.on_throttled(); // 4
        assert_eq!(l.current(), 4);
        for _ in 0..(RECOVER_AFTER_SUCCESSES - 1) {
            l.on_success();
        }
        assert_eq!(l.current(), 4, "成功次数还不够，不提速");
        l.on_success();
        assert_eq!(l.current(), 5, "满一次阈值 +1");
    }

    #[test]
    fn throttle_resets_recovery_streak() {
        let l = RateLimiter::new(8);
        l.on_throttled(); // 4
        for _ in 0..(RECOVER_AFTER_SUCCESSES - 1) {
            l.on_success();
        }
        l.on_throttled(); // 掉到 2，计数清零
        assert_eq!(l.current(), 2);
        l.on_success();
        assert_eq!(l.current(), 2, "清零后要重新累积");
    }

    #[test]
    fn ceiling_of_zero_is_clamped_to_min() {
        let l = RateLimiter::new(0);
        assert_eq!(l.ceiling(), MIN_RPS);
        assert_eq!(l.current(), MIN_RPS);
    }
}
