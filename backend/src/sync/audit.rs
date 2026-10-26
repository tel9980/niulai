//! 完整性核对与兜底补偿。
//!
//! - 核对：最近的每个交易日，每个每日数据集是否都同步成功；个股日线是否覆盖了在市的股票。
//! - 补偿：能重新取的缺口自动补（用数据集本身，或配置的 `recover_with`）；重试用尽的临时失败再试一次。
//!   只有当天值、已经错过的数据补不回来，如实列出。
//!
//! 规则见 docs/architecture/sync_pipeline.md#audit。

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;

use super::Engine;
use super::planner::PlanOptions;
use super::run::RunKind;
use super::spec::{DatasetSpec, Registry, Schedule};
use crate::time::now_cn;

/// 核对最近多少个交易日
const DAYS: i64 = 20;
/// 个股日线少于在市股票数的这个比例时，认为不完整（停牌的股票没有日线，通常不到 2%）
const BAR_COVERAGE: f64 = 0.97;

#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum GapKind {
    /// 这一天没有同步过，或被跳过了
    NotRun,
    /// 只有当天值的数据已经错过
    Missed,
    /// 任务失败
    Failed,
    /// 同步了，但数据不全
    Incomplete,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct Gap {
    pub trade_date: NaiveDate,
    pub dataset_id: String,
    pub title: String,
    pub kind: GapKind,
    pub message: String,
    /// 能否补回：有历史的数据可以重新取，只有当天值的数据错过了就没有了
    pub recoverable: bool,
}

#[derive(Serialize, ToSchema, Debug)]
pub struct FailureGroup {
    pub dataset_id: String,
    pub title: String,
    pub tasks: i64,
    /// 白话说明：为什么失败、会不会自动处理
    pub reason: String,
    /// 其中一条原始错误
    pub sample_error: String,
    pub latest_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum DailyState {
    /// 最近一个交易日的数据都已就绪
    Ready,
    /// 正在同步
    Running,
    /// 有缺口
    Incomplete,
    /// 还没有开始每日同步（例如刚部署）
    NotStarted,
}

#[derive(Serialize, ToSchema, Debug)]
pub struct BackfillProgress {
    pub done: i64,
    pub total: i64,
    /// 按最近一小时的速度估算的剩余时间
    pub eta_hours: Option<f64>,
}

/// 供应商接口的当前限速。`current` 会因为遇到 429 自动降低、连续成功后再逐步回升，
/// 所以它可能小于配置的上限 `ceiling`；两者不相等说明正在被限流。
#[derive(Serialize, ToSchema, Debug, Clone, Copy)]
pub struct RateStatus {
    /// 当前生效的每秒请求数
    pub current: u32,
    /// 配置的上限（NIULAI_RATE_LIMIT_RPS）
    pub ceiling: u32,
}

#[derive(Serialize, ToSchema, Debug)]
pub struct SyncHealth {
    /// 最近一个应该已经有数据的交易日
    pub latest_trade_date: Option<NaiveDate>,
    pub daily_state: DailyState,
    /// 一句话说明当前状态
    pub message: String,
    /// 最近 20 个交易日的缺口，新的在前
    pub gaps: Vec<Gap>,
    /// 失败后不再自动重试、之后也没有成功过的任务，按数据集分组
    pub failures: Vec<FailureGroup>,
    pub backfill: Option<BackfillProgress>,
    /// 当前限速；自适应降速后 current 会小于 ceiling
    pub rate: RateStatus,
    /// 最近一次兜底补偿的时间
    pub last_repair_at: Option<DateTime<Utc>>,
}

/// 应该已经有数据的交易日（新的在后）：从开始每日同步的那天起，到最近一个已收盘并到了同步时间的交易日
async fn due_days(engine: &Engine) -> anyhow::Result<Vec<NaiveDate>> {
    let now = now_cn();
    let at = super::scheduler::load_times(engine).await?.daily_at()?;
    let last = if now.time() >= at {
        now.date()
    } else {
        now.date().pred_opt().unwrap_or(now.date())
    };
    let first: Option<NaiveDate> =
        sqlx::query_scalar("SELECT min(trade_date) FROM ops.run WHERE kind = 'incremental'")
            .fetch_one(&engine.pool)
            .await?;
    let Some(first) = first else {
        return Ok(vec![]);
    };
    let mut days: Vec<NaiveDate> = sqlx::query_scalar(
        "SELECT cal_date FROM ref.trading_day WHERE cal_date BETWEEN $1 AND $2 ORDER BY cal_date DESC LIMIT $3",
    )
    .bind(first)
    .bind(last)
    .bind(DAYS)
    .fetch_all(&engine.pool)
    .await?;
    days.reverse();
    Ok(days)
}

fn daily_specs(registry: &Registry) -> Vec<&DatasetSpec> {
    registry
        .datasets
        .iter()
        .filter(|d| d.enabled && d.schedule == Schedule::Daily)
        .collect()
}

/// 每日数据集每天的任务结果，按分区汇总：同一个分区在任何一次运行中成功过就算成功
type DayRow = (String, NaiveDate, i64, i64, i64, i64, Option<String>);

// @project-doc docs/architecture/sync_pipeline.md#audit
/// 核对缺口
pub async fn gaps(
    engine: &Engine,
) -> anyhow::Result<(Vec<NaiveDate>, Vec<Gap>, BTreeMap<NaiveDate, Vec<String>>)> {
    let days = due_days(engine).await?;
    let specs = daily_specs(&engine.registry);
    let ids: Vec<String> = specs.iter().map(|s| s.id.clone()).collect();
    let rows: Vec<DayRow> = sqlx::query_as(
        "WITH p AS (
           SELECT t.dataset_id, r.trade_date, t.part_key,
                  bool_or(t.status IN ('done', 'empty')) AS ok,
                  bool_or(t.status IN ('pending', 'running', 'failed')) AS open,
                  bool_or(t.status = 'dead') AS dead,
                  max(t.last_error) FILTER (WHERE t.status IN ('dead', 'skipped')) AS err
           FROM ops.task t JOIN ops.run r USING (run_id)
           WHERE r.trade_date = ANY($1) AND t.dataset_id = ANY($2)
             -- 回补运行里按天积累的快照（分区键带 |day=）也算：首次部署时回补和每日同步同时规划，任务按分区去重只有一个
             AND (r.kind IN ('incremental', 'repair') OR t.part_key LIKE '%|day=' || r.trade_date)
           GROUP BY 1, 2, 3)
         SELECT dataset_id, trade_date,
                count(*) FILTER (WHERE ok), count(*) FILTER (WHERE NOT ok AND open),
                count(*) FILTER (WHERE NOT ok AND NOT open AND dead), count(*) FILTER (WHERE NOT ok AND NOT open AND NOT dead),
                max(err) FILTER (WHERE NOT ok)
         FROM p GROUP BY 1, 2",
    )
    .bind(&days)
    .bind(&ids)
    .fetch_all(&engine.pool)
    .await?;
    let found: BTreeMap<(String, NaiveDate), DayRow> =
        rows.into_iter().map(|r| ((r.0.clone(), r.1), r)).collect();

    // 只有当天值的数据，在供应商还停在这个交易日时（例如周末、下一个交易日开盘前）仍然取得到
    // 查询和同步任务共用限速，同步繁忙时可能要排队：最多等 2 秒，没等到就当作不知道（后台查完会进缓存）
    let client = engine.client.clone();
    let lookup =
        tokio::spawn(async move { super::calendar::latest_vendor_day(client.as_ref()).await });
    let vendor_day = match tokio::time::timeout(std::time::Duration::from_secs(2), lookup).await {
        Ok(Ok(Ok(d))) => Some(d),
        _ => None,
    };
    let current = days.last().copied().filter(|d| vendor_day == Some(*d));
    let mut out = vec![];
    for &d in &days {
        for s in &specs {
            let recoverable = !s.trade_date_guard || s.recover_with.is_some() || current == Some(d);
            let gap = |kind, message: String| Gap {
                trade_date: d,
                dataset_id: s.id.clone(),
                title: s.title.clone(),
                kind,
                message,
                recoverable,
            };
            match found.get(&(s.id.clone(), d)) {
                None => out.push(gap(GapKind::NotRun, "这一天没有同步".into())),
                Some(&(_, _, _, _, dead, skipped, ref err)) => {
                    let err = err.clone().unwrap_or_default();
                    if dead > 0 {
                        let kind = if err.starts_with("已错过") {
                            GapKind::Missed
                        } else {
                            GapKind::Failed
                        };
                        out.push(gap(kind, format!("{dead} 个任务失败：{err}")));
                    } else if skipped > 0 && !err.contains("不是交易日") {
                        out.push(gap(GapKind::NotRun, format!("被跳过：{err}")));
                    }
                }
            }
        }
    }

    // 个股日线：每个交易日应覆盖在市的股票（停牌的没有日线）
    let bars: Vec<(NaiveDate, i64, i64)> = sqlx::query_as(
        "SELECT d, (SELECT count(*) FROM mkt.stock_bar_1d b WHERE b.trade_date = d),
                (SELECT count(*) FROM ref.security s WHERE s.list_date <= d AND (s.delisted_on IS NULL OR s.delisted_on > d))
         FROM unnest($1::date[]) AS d",
    )
    .bind(&days)
    .fetch_all(&engine.pool)
    .await?;
    let mut missing_bars = BTreeMap::new();
    let bar_spec = engine.registry.stock_bar_history();
    for (d, have, expected) in bars {
        if expected > 0 && (have as f64) < expected as f64 * BAR_COVERAGE {
            let codes: Vec<String> = sqlx::query_scalar(
                "SELECT s.thscode FROM ref.security s WHERE s.list_date <= $1 AND (s.delisted_on IS NULL OR s.delisted_on > $1)
                   AND NOT EXISTS (SELECT 1 FROM mkt.stock_bar_1d b WHERE b.thscode = s.thscode AND b.trade_date = $1)",
            )
            .bind(d)
            .fetch_all(&engine.pool)
            .await?;
            out.push(Gap {
                trade_date: d,
                dataset_id: bar_spec.map_or_else(String::new, |s| s.id.clone()),
                title: bar_spec.map_or("个股日线".into(), |s| s.title.clone()),
                kind: GapKind::Incomplete,
                message: format!("只有 {have} 只股票有日线，在市的有 {expected} 只"),
                recoverable: true,
            });
            missing_bars.insert(d, codes);
        }
    }
    out.sort_by(|a, b| {
        b.trade_date
            .cmp(&a.trade_date)
            .then_with(|| a.dataset_id.cmp(&b.dataset_id))
    });
    Ok((days, out, missing_bars))
}

/// 白话说明失败原因
fn explain(err: &str) -> String {
    if err.starts_with("已错过") {
        "错过了当天的数据，这类数据只有当天的值，无法补回".into()
    } else if err.contains("trading day") {
        "请求的日期不是交易日（第一次同步时交易日历还没取到），这一天本来就没有数据".into()
    } else if err.contains("within one year") {
        "日期超出了接口的历史范围（只能查最近一年），这部分数据已经取不到".into()
    } else if err.contains("Unknown thscode") || err.contains("1002") {
        "接口不支持这个代码（例如可转债），这部分数据本来就没有".into()
    } else if err.starts_with("写入") {
        "数据写入数据库失败，是程序的问题，需要修复".into()
    } else if err.starts_with("可重试错误") {
        "供应商暂时不可用，多次重试仍失败；兜底补偿时会再试一次".into()
    } else {
        "接口返回错误".into()
    }
}

/// 失败后之后也没有成功过的任务
pub async fn failures(pool: &PgPool, registry: &Registry) -> anyhow::Result<Vec<FailureGroup>> {
    let rows: Vec<(String, i64, String, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT t.dataset_id, count(*), min(t.last_error), max(t.finished_at)
         FROM ops.v_unresolved_dead t
         GROUP BY t.dataset_id, left(coalesce(t.last_error, ''), 24)
         ORDER BY 2 DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, n, err, at)| FailureGroup {
            title: registry.get(&id).map_or(id.clone(), |s| s.title.clone()),
            dataset_id: id,
            tasks: n,
            reason: explain(&err),
            sample_error: err,
            latest_at: at,
        })
        .collect())
}

/// 普通用户也能看的数据状态：核心数据齐没齐、首次回补的进度
#[derive(Serialize, ToSchema, Debug)]
pub struct DataStatus {
    /// 核心数据（个股日线、公司行动）已经齐了
    pub core_ready: bool,
    /// 核心数据还剩多少个任务
    pub core_left: i64,
    /// 全部数据的首次回补进度；没有在回补时为空
    pub backfill: Option<BackfillProgress>,
    /// 一句话说明
    pub message: String,
}

pub async fn data_status(pool: &PgPool) -> anyhow::Result<DataStatus> {
    // 核心数据（个股日线、公司行动）的首次回补没完成时，日线只有一部分股票、一部分年份，运营建议先不生成
    let core_left = crate::readiness::core_backfill_left(pool).await?;
    let backfill = backfill(pool).await?;
    let per_hour: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ops.api_call WHERE called_at > now() - interval '1 hour'",
    )
    .fetch_one(pool)
    .await?;
    let message = if core_left > 0 {
        let eta = if per_hour > 0 {
            format!(
                "，按现在的速度大约还要 {:.1} 小时",
                core_left as f64 / per_hour as f64
            )
        } else {
            String::new()
        };
        format!(
            "正在准备核心数据（全市场的历史日线），还有 {core_left} 批{eta}。准备好之前，首页的大盘统计只包括已经下载的股票，运营建议先不生成"
        )
    } else if let Some(b) = &backfill {
        format!(
            "核心数据已就绪；其他数据还在补（{} / {}），不影响运营建议",
            b.done, b.total
        )
    } else {
        "数据已就绪".to_string()
    };
    Ok(DataStatus {
        core_ready: core_left == 0,
        core_left,
        backfill,
        message,
    })
}

async fn backfill(pool: &PgPool) -> anyhow::Result<Option<BackfillProgress>> {
    let (total, done): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT sum(tasks_total)::bigint, sum(tasks_done + tasks_failed)::bigint FROM ops.run WHERE kind = 'backfill' AND status = 'running'",
    )
    .fetch_one(pool)
    .await?;
    let (Some(total), Some(done)) = (total, done) else {
        return Ok(None);
    };
    // 同时看「最近一小时完成了多少任务」和「最近一小时打了多少次请求」，按两者的较小值估：
    // 前者对一次请求干很多活的数据集（全市场导出、分页快照）更准，后者对单任务单请求的更准；
    // 多任务共用一次请求时取 min 不会高估速度，避免 ETA 乐观到 0 小时。
    let (tasks_per_hour, calls_per_hour): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM ops.task WHERE finished_at > now() - interval '1 hour'),
                (SELECT count(*) FROM ops.api_call WHERE called_at > now() - interval '1 hour')",
    )
    .fetch_one(pool)
    .await?;
    let left = (total - done).max(0);
    let per_hour = match (tasks_per_hour, calls_per_hour) {
        (0, 0) => 0,
        (t, 0) => t,
        (0, c) => c,
        (t, c) => t.min(c),
    };
    Ok(Some(BackfillProgress {
        done,
        total,
        eta_hours: (per_hour > 0).then(|| left as f64 / per_hour as f64),
    }))
}

/// 同步页顶部的状态：最近一个交易日的数据是否就绪、缺口、失败、回补进度
pub async fn health(engine: &Engine) -> anyhow::Result<SyncHealth> {
    let (days, gaps, _) = gaps(engine).await?;
    let latest = days.last().copied();
    // 每日数据还在同步：定时同步中每日数据集的任务还没结束；补偿任务单独说明
    let daily: Vec<String> = daily_specs(&engine.registry)
        .iter()
        .map(|s| s.id.clone())
        .collect();
    let (daily_open, repair_open): (bool, i64) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM ops.task t JOIN ops.run r USING (run_id)
                        WHERE r.kind = 'incremental' AND t.dataset_id = ANY($1) AND t.status IN ('pending', 'running', 'failed')),
                (SELECT count(*) FROM ops.task t JOIN ops.run r USING (run_id) WHERE r.kind = 'repair' AND t.status IN ('pending', 'running', 'failed'))",
    )
    .bind(&daily)
    .fetch_one(&engine.pool)
    .await?;
    let latest_gaps: Vec<&Gap> = gaps
        .iter()
        .filter(|g| Some(g.trade_date) == latest)
        .collect();
    let (daily_state, mut message) = match latest {
        None => (
            DailyState::NotStarted,
            "还没有开始每日同步：首次回补期间，每天 18:00 照常同步当天的数据".to_string(),
        ),
        Some(d) if daily_open => (DailyState::Running, format!("正在同步 {d} 的数据")),
        Some(d) if latest_gaps.is_empty() => (DailyState::Ready, format!("{d} 的数据已就绪")),
        Some(d) => (
            DailyState::Incomplete,
            format!("{d} 有 {} 项数据缺失", latest_gaps.len()),
        ),
    };
    if repair_open > 0 {
        message.push_str(&format!("；正在补齐缺口，还有 {repair_open} 个任务"));
    }
    let last_repair_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT fired_at FROM ops.scheduler WHERE job = 'repair'")
            .fetch_optional(&engine.pool)
            .await?
            .flatten();
    Ok(SyncHealth {
        latest_trade_date: latest,
        daily_state,
        message,
        failures: failures(&engine.pool, &engine.registry).await?,
        gaps,
        backfill: backfill(&engine.pool).await?,
        rate: RateStatus {
            current: engine.client.limiter().current(),
            ceiling: engine.client.limiter().ceiling(),
        },
        last_repair_at,
    })
}

#[derive(Serialize, ToSchema, Debug, Default)]
pub struct RepairSummary {
    /// 新规划的补偿运行
    pub runs: Vec<i64>,
    /// 重新排队的失败任务数
    pub retried: u64,
    /// 补不回来的缺口数
    pub unrecoverable: usize,
    /// 日期不是交易日或已超出历史范围、证券已退市，改为跳过的失败任务数
    pub cleared: u64,
}

/// 失败的按交易日分区的任务，日期其实不是交易日或已经超出接口的历史范围（见 calendar::stale_partition_date），
/// 以及已退市证券的失败任务：本来就没有数据，改为跳过，不再作为失败列出
async fn clear_stale(engine: &Engine) -> anyhow::Result<u64> {
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT task_id, dataset_id, params->'vars'->>'date' FROM ops.task
         WHERE status = 'dead' AND params->'vars' ? 'date'",
    )
    .fetch_all(&engine.pool)
    .await?;
    let mut cleared = 0;
    for (id, dataset, date) in rows {
        let (Some(spec), Ok(d)) = (
            engine.registry.get(&dataset),
            NaiveDate::parse_from_str(&date, "%Y-%m-%d"),
        ) else {
            continue;
        };
        if let Some(why) = super::calendar::stale_partition_date(&engine.pool, spec, d).await? {
            cleared += super::queue::skip_dead(&engine.pool, id, &why).await? as u64;
        }
    }
    Ok(cleared + super::queue::skip_dead_delisted(&engine.pool).await?)
}

/// 自动兜底补偿：每个交易日最多一次，在这一天的每日同步结束之后执行
pub async fn auto_repair(engine: &Engine, today: NaiveDate) -> anyhow::Result<()> {
    let busy: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM ops.task WHERE status IN ('pending', 'running', 'failed') AND priority < 100)")
        .fetch_one(&engine.pool)
        .await?;
    if busy {
        return Ok(());
    }
    let days = due_days(engine).await?;
    let Some(&latest) = days.last() else {
        return Ok(());
    };
    // 最近一个交易日的每日同步还没发起（例如今天还没到 18:00 就已是今天）时不补，等调度器先跑
    let fired: Option<NaiveDate> =
        sqlx::query_scalar("SELECT last_fired FROM ops.scheduler WHERE job = 'daily'")
            .fetch_optional(&engine.pool)
            .await?;
    if fired.is_none_or(|f| f < latest) || latest > today {
        return Ok(());
    }
    if !super::scheduler::claim(&engine.pool, "repair", latest, "day").await? {
        return Ok(());
    }
    let s = match repair(engine).await {
        Ok(s) => s,
        Err(e) => {
            if let Err(release_error) =
                super::scheduler::release_claim(&engine.pool, "repair", latest).await
            {
                tracing::error!(error = %format!("{release_error:#}"), "撤销补偿调度标记失败");
            }
            return Err(e);
        }
    };
    tracing::info!(trade_date = %latest, runs = ?s.runs, retried = s.retried, unrecoverable = s.unrecoverable, "兜底补偿完成");
    Ok(())
}

/// 同一个任务最多被兜底补偿重新排队几次
const MAX_REPAIRS: i16 = 3;

/// 补偿：能重新取的缺口规划补偿运行；重试用尽的临时失败重新排队
pub async fn repair(engine: &Engine) -> anyhow::Result<RepairSummary> {
    let (_, gaps, missing_bars) = gaps(engine).await?;
    let mut summary = RepairSummary {
        unrecoverable: gaps.iter().filter(|g| !g.recoverable).count(),
        ..Default::default()
    };

    // 每个要补的数据集，补到哪些交易日；个股日线只补缺的股票
    let mut targets: BTreeMap<String, BTreeSet<NaiveDate>> = BTreeMap::new();
    let mut whole_bars = false;
    let bar_id = engine.registry.stock_bar_history().map(|s| s.id.clone());
    for g in gaps.iter().filter(|g| g.recoverable) {
        let spec = engine.registry.get(&g.dataset_id);
        let target = spec
            .and_then(|s| s.recover_with.clone())
            .unwrap_or_else(|| g.dataset_id.clone());
        if Some(&target) == bar_id.as_ref() && g.kind != GapKind::Incomplete {
            whole_bars = true;
        }
        targets.entry(target).or_default().insert(g.trade_date);
    }
    for (id, dates) in targets {
        let Some(spec) = engine.registry.get(&id).filter(|s| s.enabled) else {
            continue;
        };
        let latest = *dates.iter().max().expect("至少一个日期");
        let only_thscodes: Vec<String> = if Some(&id) == bar_id.as_ref() && !whole_bars {
            dates
                .iter()
                .flat_map(|d| missing_bars.get(d).cloned().unwrap_or_default())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            vec![]
        };
        let (run_id, _) = engine
            .planner()
            .plan_run(
                RunKind::Repair,
                &[spec],
                PlanOptions {
                    trade_date: Some(latest),
                    only_thscodes,
                    trigger: Some("repair".into()),
                    ..Default::default()
                },
            )
            .await?;
        summary.runs.push(run_id);
    }

    summary.cleared = clear_stale(engine).await?;

    // 重试用尽的临时失败（供应商暂时不可用），之后也没有成功过：再试一次。每个任务最多补偿 MAX_REPAIRS 次，
    // 供应商一直给不了的分区不会每天被重排、把所属的运行重新打开
    let mut tx = engine.pool.begin().await?;
    // 同一个分区只重新排队最新的 dead 任务，状态迁移由 queue 模块统一执行。
    let retried = super::queue::requeue_dead_for_repair(&mut tx, MAX_REPAIRS).await?;
    // 只有明确的 partial repair 运行允许重新打开；成功运行不会被补偿路径改写。
    let _ = super::run::reopen_partial_repairs(&mut tx).await?;
    tx.commit().await?;
    summary.retried = retried;
    // 只记录时间；last_fired 由 auto_repair 的抢占维护，手动补偿不影响当天的自动补偿
    sqlx::query("INSERT INTO ops.scheduler (job, last_fired, fired_at) VALUES ('repair', DATE '2000-01-01', now()) ON CONFLICT (job) DO UPDATE SET fired_at = now()")
        .execute(&engine.pool)
        .await?;
    Ok(summary)
}
