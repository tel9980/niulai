//! 类型化表的投影（datasets.yaml 中的 sink）。
//!
//! 每个 sink 接收一批记录（JSON 数组），用一条或几条 SQL 写入目标表。写入都是幂等的 UPSERT。
//! 记录在 SQL 中的形状：{"thscode", "obs_date", "seq", "p": 供应商原始字段}。

use anyhow::bail;
use chrono::NaiveDate;
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgPool};

use super::records::RecordRow;

/// sink 需要的任务信息
pub struct SinkContext<'a> {
    pub dataset_id: &'a str,
    pub run_id: i64,
    /// 运行的交易日：快照类数据的日期，代码表、指数成分的观测日
    pub trade_date: Option<NaiveDate>,
    /// 任务的分区变量
    pub vars: &'a Map<String, Value>,
    pub sink_mode: Option<&'a str>,
}

fn rows_json(rows: &[RecordRow]) -> Value {
    Value::Array(rows.iter().map(|r| json!({"thscode": r.thscode, "obs_date": r.obs_date, "seq": r.seq, "p": r.payload})).collect())
}

/// 按 (sink, 数据集) 选择投影；同一张表的不同来源（例如历史接口与快照）写法不同
pub async fn apply(
    pool: &PgPool,
    sink: &str,
    ctx: &SinkContext<'_>,
    rows: &[RecordRow],
) -> anyhow::Result<usize> {
    let mut tx = pool.begin().await?;
    let n = apply_in_tx(&mut tx, sink, ctx, rows).await?;
    tx.commit().await?;
    Ok(n)
}

pub async fn apply_in_tx(
    conn: &mut PgConnection,
    sink: &str,
    ctx: &SinkContext<'_>,
    rows: &[RecordRow],
) -> anyhow::Result<usize> {
    if matches!(
        (sink, ctx.dataset_id),
        ("mkt.stock_bar_1d", "a_share.bar_1d")
            | ("mkt.stock_bar_1d", "a_share.snapshot")
            | ("mkt.index_bar_1d", "index.bar_1d")
            | ("mkt.index_bar_1d", "index.snapshot")
    ) {
        log_bad_bar_rows(ctx, rows);
    }
    let data = rows_json(rows);
    let run_id = ctx.run_id;
    let trade_date = ctx.trade_date;
    let ds = ctx.dataset_id;
    let mode = ctx.sink_mode;
    let var = |k: &str| ctx.vars.get(k).and_then(Value::as_str).map(str::to_string);
    let n = match (sink, ds) {
        ("ref.security", _) => {
            security(conn, &data, trade_date, var("asset_type").as_deref()).await?
        }
        ("ref.etf_security", _) => {
            etf_security(conn, &data, trade_date, var("asset_type").as_deref()).await?
        }
        ("ref.index_info", _) => index_info(conn, &data, trade_date, var("tag").as_deref()).await?,
        ("ref.trading_day", _) => sqlx::query(TRADING_DAY)
            .bind(&data)
            .execute(&mut *conn)
            .await?
            .rows_affected() as usize,
        ("mkt.stock_bar_1d", "a_share.bar_dump") => {
            exec(conn, STOCK_BAR_DUMP, &data, run_id).await?
        }
        ("mkt.stock_bar_1d", _) if mode != Some("snapshot") => {
            exec(conn, STOCK_BAR_HIST, &data, run_id).await?
        }
        ("mkt.stock_bar_1d", _) => {
            exec_dated(conn, STOCK_BAR_SNAPSHOT, &data, run_id, trade_date).await?
        }
        ("mkt.etf_bar_1d", _) if mode != Some("snapshot") => {
            exec(conn, ETF_BAR_HIST, &data, run_id).await?
        }
        ("mkt.etf_bar_1d", _) => {
            exec_dated(conn, ETF_BAR_SNAPSHOT, &data, run_id, trade_date).await?
        }
        ("mkt.index_bar_1d", _) if mode != Some("snapshot") => {
            exec(conn, INDEX_BAR_HIST, &data, run_id).await?
        }
        ("mkt.index_bar_1d", _) => {
            exec_dated(conn, INDEX_BAR_SNAPSHOT, &data, run_id, trade_date).await?
        }
        ("mkt.corp_action", _) => corp_action(conn, &data, run_id).await?,
        ("ref.index_member", _) => {
            index_member(conn, &data, trade_date, var("thscode").as_deref()).await?
        }
        ("fin.valuation_1d", _) if mode != Some("float_cap") => {
            exec_dated(conn, VALUATION, &data, run_id, trade_date).await?
        }
        ("fin.valuation_1d", _) => exec_dated(conn, FLOAT_CAP, &data, run_id, trade_date).await?,
        (s, d) => bail!("没有为数据集 {d} 实现 sink {s}"),
    };
    Ok(n)
}

/// 供应商历史行情偶尔有成交量或成交额数量级异常。写入 SQL 会跳过这些行，
/// 这里把具体证券、日期和原始数值写进日志，便于复核而不让整段窗口失败。
fn log_bad_bar_rows(ctx: &SinkContext<'_>, rows: &[RecordRow]) {
    for row in rows {
        let volume = row.payload.get("volume").and_then(number);
        let turnover = row.payload.get("turnover").and_then(number);
        if volume.is_some_and(|v| v >= 1e13) || turnover.is_some_and(|v| v >= 1e13) {
            tracing::warn!(
                dataset = ctx.dataset_id,
                thscode = ?row.thscode,
                obs_date = ?row.obs_date,
                volume = ?volume,
                turnover = ?turnover,
                "过滤供应商异常成交量或成交额"
            );
        }
    }
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse::<f64>().ok())
        .filter(|n| n.is_finite())
}

async fn exec(
    conn: &mut PgConnection,
    sql: &'static str,
    data: &Value,
    run_id: i64,
) -> anyhow::Result<usize> {
    Ok(sqlx::query(sql)
        .bind(data)
        .bind(run_id)
        .execute(&mut *conn)
        .await?
        .rows_affected() as usize)
}

async fn exec_dated(
    conn: &mut PgConnection,
    sql: &'static str,
    data: &Value,
    run_id: i64,
    trade_date: Option<NaiveDate>,
) -> anyhow::Result<usize> {
    let Some(d) = trade_date else {
        bail!("快照类数据需要运行的交易日")
    };
    Ok(sqlx::query(sql)
        .bind(data)
        .bind(run_id)
        .bind(d)
        .execute(&mut *conn)
        .await?
        .rows_affected() as usize)
}

const TRADING_DAY: &str = "
INSERT INTO ref.trading_day (cal_date, source)
SELECT DISTINCT to_date(e->'p'->>'date', 'YYYYMMDD'), 'api' FROM jsonb_array_elements($1) e
ON CONFLICT (cal_date) DO UPDATE SET source = 'api' WHERE ref.trading_day.source <> 'api'";

// 供应商偶尔返回明显错误的一行（例如 000066.SZ 2006-09-25 的成交额是 9.2e18，接近 64 位整数上限），
// 写入会溢出、让整个窗口的几千行都失败。成交量或成交额达到 1e13 的行直接丢掉，不影响其他日子，见 docs/domains/market_data_rules.md#bad_rows

// 全市场导出的日线（a_share.bar_dump）：一次下载覆盖约 10 年，与逐只拉的 STOCK_BAR_HIST 写同一张表，
// UPSERT 幂等；source 记为 'dump'，便于在同步页和核对里区分来源。导出里没有成交额时留空。
const STOCK_BAR_DUMP: &str = "
INSERT INTO mkt.stock_bar_1d AS t (thscode, trade_date, open, high, low, close, volume, amount, source, run_id)
SELECT e->>'thscode', (e->>'obs_date')::date,
       coalesce((e->'p'->>'open_price')::numeric, (e->'p'->>'open')::numeric),
       coalesce((e->'p'->>'high_price')::numeric, (e->'p'->>'high')::numeric),
       coalesce((e->'p'->>'low_price')::numeric, (e->'p'->>'low')::numeric),
       coalesce((e->'p'->>'close_price')::numeric, (e->'p'->>'close')::numeric),
       round(coalesce((e->'p'->>'volume')::numeric, 0))::bigint,
       coalesce((e->'p'->>'turnover')::numeric, (e->'p'->>'amount')::numeric),
       'dump', $2
FROM jsonb_array_elements($1) e
WHERE coalesce((e->'p'->>'open_price')::numeric, (e->'p'->>'open')::numeric) IS NOT NULL
  AND (e->>'obs_date') IS NOT NULL
  AND coalesce((e->'p'->>'volume')::numeric, 0) < 1e13
  AND coalesce((e->'p'->>'turnover')::numeric, (e->'p'->>'amount')::numeric, 0) < 1e13
ON CONFLICT (thscode, trade_date) DO UPDATE
  SET open = EXCLUDED.open, high = EXCLUDED.high, low = EXCLUDED.low, close = EXCLUDED.close,
      volume = EXCLUDED.volume, amount = coalesce(EXCLUDED.amount, t.amount),
      source = 'dump', run_id = EXCLUDED.run_id, updated_at = now()
  WHERE (t.open, t.high, t.low, t.close, t.volume, t.amount)
        IS DISTINCT FROM (EXCLUDED.open, EXCLUDED.high, EXCLUDED.low, EXCLUDED.close, EXCLUDED.volume, coalesce(EXCLUDED.amount, t.amount))";

const STOCK_BAR_HIST: &str = "
INSERT INTO mkt.stock_bar_1d AS t (thscode, trade_date, open, high, low, close, volume, amount, source, run_id)
SELECT e->>'thscode', (e->>'obs_date')::date,
       (e->'p'->>'open_price')::numeric, (e->'p'->>'high_price')::numeric, (e->'p'->>'low_price')::numeric,
       (e->'p'->>'close_price')::numeric, round((e->'p'->>'volume')::numeric)::bigint, (e->'p'->>'turnover')::numeric, 'hist', $2
FROM jsonb_array_elements($1) e
WHERE e->'p'->>'open_price' IS NOT NULL
  AND coalesce((e->'p'->>'volume')::numeric, 0) < 1e13 AND coalesce((e->'p'->>'turnover')::numeric, 0) < 1e13
ON CONFLICT (thscode, trade_date) DO UPDATE
  SET open = EXCLUDED.open, high = EXCLUDED.high, low = EXCLUDED.low, close = EXCLUDED.close,
      volume = EXCLUDED.volume, amount = EXCLUDED.amount, source = 'hist', run_id = EXCLUDED.run_id, updated_at = now()
  WHERE (t.open, t.high, t.low, t.close, t.volume, t.amount, t.source)
        IS DISTINCT FROM (EXCLUDED.open, EXCLUDED.high, EXCLUDED.low, EXCLUDED.close, EXCLUDED.volume, EXCLUDED.amount, 'hist')";

// 快照：价格精确，成交量和成交额只有约 9 位有效数字。已有历史接口的精确值时只补 pre_close；停牌（成交量为 0）不写。
const STOCK_BAR_SNAPSHOT: &str = "
INSERT INTO mkt.stock_bar_1d AS t (thscode, trade_date, open, high, low, close, volume, amount, pre_close, source, run_id)
SELECT e->>'thscode', $3::date,
       (e->'p'->>'open_price')::numeric, (e->'p'->>'high_price')::numeric, (e->'p'->>'low_price')::numeric,
       (e->'p'->>'last_price')::numeric, round((e->'p'->>'volume')::numeric)::bigint, (e->'p'->>'turnover')::numeric,
       (e->'p'->>'prev_price')::numeric, 'snapshot', $2
FROM jsonb_array_elements($1) e
WHERE coalesce((e->'p'->>'volume')::numeric, 0) > 0 AND coalesce((e->'p'->>'open_price')::numeric, 0) > 0
  AND (e->'p'->>'volume')::numeric < 1e13 AND coalesce((e->'p'->>'turnover')::numeric, 0) < 1e13
ON CONFLICT (thscode, trade_date) DO UPDATE
  SET pre_close = EXCLUDED.pre_close,
      open   = CASE WHEN t.source = 'snapshot' THEN EXCLUDED.open   ELSE t.open   END,
      high   = CASE WHEN t.source = 'snapshot' THEN EXCLUDED.high   ELSE t.high   END,
      low    = CASE WHEN t.source = 'snapshot' THEN EXCLUDED.low    ELSE t.low    END,
      close  = CASE WHEN t.source = 'snapshot' THEN EXCLUDED.close  ELSE t.close  END,
      volume = CASE WHEN t.source = 'snapshot' THEN EXCLUDED.volume ELSE t.volume END,
      amount = CASE WHEN t.source = 'snapshot' THEN EXCLUDED.amount ELSE t.amount END,
      updated_at = now()";

const ETF_BAR_HIST: &str = "
INSERT INTO mkt.etf_bar_1d AS t (thscode, trade_date, open, high, low, close, volume, amount, source, price_basis, run_id)
SELECT e->>'thscode', (e->>'obs_date')::date,
       coalesce((e->'p'->>'open_price')::numeric, (e->'p'->>'open')::numeric),
       coalesce((e->'p'->>'high_price')::numeric, (e->'p'->>'high')::numeric),
       coalesce((e->'p'->>'low_price')::numeric, (e->'p'->>'low')::numeric),
       coalesce((e->'p'->>'close_price')::numeric, (e->'p'->>'close')::numeric),
       round(coalesce((e->'p'->>'volume')::numeric, 0))::bigint,
       coalesce((e->'p'->>'turnover')::numeric, (e->'p'->>'amount')::numeric, 0),
       'hist', 'vendor_forward_adjusted', $2
FROM jsonb_array_elements($1) e
WHERE coalesce(e->'p'->>'open_price', e->'p'->>'open') IS NOT NULL
ON CONFLICT (thscode, trade_date) DO UPDATE
 SET open = EXCLUDED.open, high = EXCLUDED.high, low = EXCLUDED.low, close = EXCLUDED.close,
     volume = EXCLUDED.volume, amount = EXCLUDED.amount, source = 'hist',
     price_basis = 'vendor_forward_adjusted', run_id = EXCLUDED.run_id, updated_at = now()";

const ETF_BAR_SNAPSHOT: &str = "
INSERT INTO mkt.etf_bar_1d AS t (thscode, trade_date, open, high, low, close, volume, amount, pre_close, source, price_basis, run_id)
SELECT e->>'thscode', $3::date,
       coalesce((e->'p'->>'open_price')::numeric, (e->'p'->>'open')::numeric),
       coalesce((e->'p'->>'high_price')::numeric, (e->'p'->>'high')::numeric),
       coalesce((e->'p'->>'low_price')::numeric, (e->'p'->>'low')::numeric),
       coalesce((e->'p'->>'last_price')::numeric, (e->'p'->>'close_price')::numeric, (e->'p'->>'close')::numeric),
       round(coalesce((e->'p'->>'volume')::numeric, 0))::bigint,
       coalesce((e->'p'->>'turnover')::numeric, (e->'p'->>'amount')::numeric, 0),
       coalesce((e->'p'->>'prev_price')::numeric, (e->'p'->>'pre_close')::numeric),
       'snapshot', 'market_snapshot', $2
FROM jsonb_array_elements($1) e
WHERE coalesce((e->'p'->>'open_price')::numeric, (e->'p'->>'open')::numeric, 0) > 0
ON CONFLICT (thscode, trade_date) DO UPDATE
 SET pre_close = EXCLUDED.pre_close,
     open = CASE WHEN t.source = 'snapshot' THEN EXCLUDED.open ELSE t.open END,
     high = CASE WHEN t.source = 'snapshot' THEN EXCLUDED.high ELSE t.high END,
     low = CASE WHEN t.source = 'snapshot' THEN EXCLUDED.low ELSE t.low END,
     close = CASE WHEN t.source = 'snapshot' THEN EXCLUDED.close ELSE t.close END,
     volume = CASE WHEN t.source = 'snapshot' THEN EXCLUDED.volume ELSE t.volume END,
     amount = CASE WHEN t.source = 'snapshot' THEN EXCLUDED.amount ELSE t.amount END,
     source = CASE WHEN t.source = 'snapshot' THEN 'snapshot' ELSE t.source END,
     price_basis = CASE WHEN t.source = 'snapshot' THEN 'market_snapshot' ELSE t.price_basis END,
     run_id = EXCLUDED.run_id, updated_at = now()";

// 指数的基日那一行（点位 1000）没有成交量和成交额，按 0 写入
const INDEX_BAR_HIST: &str = "
INSERT INTO mkt.index_bar_1d AS t (thscode, trade_date, open, high, low, close, volume, amount, run_id)
SELECT e->>'thscode', (e->>'obs_date')::date,
       (e->'p'->>'open_price')::numeric, (e->'p'->>'high_price')::numeric, (e->'p'->>'low_price')::numeric,
       (e->'p'->>'close_price')::numeric, coalesce(round((e->'p'->>'volume')::numeric), 0), coalesce((e->'p'->>'turnover')::numeric, 0), $2
FROM jsonb_array_elements($1) e
WHERE e->'p'->>'open_price' IS NOT NULL
ON CONFLICT (thscode, trade_date) DO UPDATE
  SET open = EXCLUDED.open, high = EXCLUDED.high, low = EXCLUDED.low, close = EXCLUDED.close,
      volume = EXCLUDED.volume, amount = EXCLUDED.amount, run_id = EXCLUDED.run_id, updated_at = now()
  WHERE (t.open, t.high, t.low, t.close, t.volume, t.amount) IS DISTINCT FROM (EXCLUDED.open, EXCLUDED.high, EXCLUDED.low, EXCLUDED.close, EXCLUDED.volume, EXCLUDED.amount)";

const INDEX_BAR_SNAPSHOT: &str = "
INSERT INTO mkt.index_bar_1d AS t (thscode, trade_date, open, high, low, close, volume, amount, pre_close, run_id)
SELECT e->>'thscode', $3::date,
       (e->'p'->>'open_price')::numeric, (e->'p'->>'high_price')::numeric, (e->'p'->>'low_price')::numeric,
       (e->'p'->>'last_price')::numeric, coalesce(round((e->'p'->>'volume')::numeric), 0), coalesce((e->'p'->>'turnover')::numeric, 0), (e->'p'->>'prev_price')::numeric, $2
FROM jsonb_array_elements($1) e
WHERE coalesce((e->'p'->>'open_price')::numeric, 0) > 0
ON CONFLICT (thscode, trade_date) DO UPDATE SET pre_close = EXCLUDED.pre_close, updated_at = now()";

const VALUATION: &str = "
INSERT INTO fin.valuation_1d AS t (thscode, trade_date, pe_ttm, pe_mrq, pb_mrq, ps_ttm, pcf_ttm, run_id)
SELECT e->>'thscode', $3::date, (e->'p'->>'pe_ttm')::numeric, (e->'p'->>'pe_mrq')::numeric, (e->'p'->>'pb_mrq')::numeric,
       (e->'p'->>'ps_ttm')::numeric, (e->'p'->>'pcf_ttm')::numeric, $2
FROM jsonb_array_elements($1) e
ON CONFLICT (thscode, trade_date) DO UPDATE
  SET pe_ttm = EXCLUDED.pe_ttm, pe_mrq = EXCLUDED.pe_mrq, pb_mrq = EXCLUDED.pb_mrq, ps_ttm = EXCLUDED.ps_ttm, pcf_ttm = EXCLUDED.pcf_ttm, run_id = EXCLUDED.run_id";

const FLOAT_CAP: &str = "
INSERT INTO fin.valuation_1d AS t (thscode, trade_date, float_market_cap, run_id)
SELECT e->>'thscode', $3::date, (e->'p'->>'float_market_cap')::numeric, $2
FROM jsonb_array_elements($1) e WHERE e->'p'->>'float_market_cap' IS NOT NULL
ON CONFLICT (thscode, trade_date) DO UPDATE SET float_market_cap = EXCLUDED.float_market_cap";

// @project-doc docs/domains/price_adjustment.md#corp_action_rules
/// 全市场导出文件里的公司行动（含配股字段），按 (证券, 除权日, 源顺序) 覆盖
async fn corp_action(conn: &mut PgConnection, data: &Value, run_id: i64) -> anyhow::Result<usize> {
    let res = sqlx::query(
        "INSERT INTO mkt.corp_action AS t (thscode, ex_date, seq, cash_div, bonus_ratio, rights_ratio, rights_price, source, run_id)
         SELECT e->>'thscode', ops.ms_to_cn_date((e->'p'->>'ex_date_ms')::bigint), (e->>'seq')::smallint,
                coalesce((e->'p'->>'dividend_per_share')::numeric, 0), coalesce((e->'p'->>'per_share_bonus')::numeric, 0),
                coalesce((e->'p'->>'allotment_ratio')::numeric, 0), coalesce((e->'p'->>'allotment_price')::numeric, 0), 'dump', $2
         FROM jsonb_array_elements($1) e
         WHERE e->>'thscode' IS NOT NULL AND e->'p'->>'ex_date_ms' IS NOT NULL
         ON CONFLICT (thscode, ex_date, seq) DO UPDATE
           SET cash_div = EXCLUDED.cash_div, bonus_ratio = EXCLUDED.bonus_ratio, rights_ratio = EXCLUDED.rights_ratio,
               rights_price = EXCLUDED.rights_price, source = EXCLUDED.source, run_id = EXCLUDED.run_id, updated_at = now()
           WHERE (t.cash_div, t.bonus_ratio, t.rights_ratio, t.rights_price, t.source)
                 IS DISTINCT FROM (EXCLUDED.cash_div, EXCLUDED.bonus_ratio, EXCLUDED.rights_ratio, EXCLUDED.rights_price, EXCLUDED.source)",
    )
    .bind(data)
    .bind(run_id)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected() as usize)
}

// @project-doc docs/domains/market_data_rules.md#security_lifecycle
async fn security(
    conn: &mut PgConnection,
    data: &Value,
    trade_date: Option<NaiveDate>,
    asset_type: Option<&str>,
) -> anyhow::Result<usize> {
    if asset_type != Some("a-share") {
        return Ok(0);
    }
    let today = trade_date.unwrap_or_else(crate::time::today_cn);
    // CREATE TABLE AS 不能带绑定参数，所以先建表再插入
    sqlx::query("CREATE TEMP TABLE _sec (thscode TEXT PRIMARY KEY, ticker TEXT, name TEXT, exchange TEXT, list_date DATE) ON COMMIT DROP")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO _sec
         SELECT DISTINCT ON (e->'p'->>'thscode') e->'p'->>'thscode', e->'p'->>'ticker', e->'p'->>'name', e->'p'->>'exchange',
                CASE WHEN e->'p'->>'list_date' ~ '^\\d{4}-\\d{2}-\\d{2}$' THEN (e->'p'->>'list_date')::date END
         FROM jsonb_array_elements($1) e WHERE e->'p'->>'asset_type' = 'a-share' AND e->'p'->>'exchange' IN ('SH', 'SZ', 'BJ')",
    )
    .bind(data)
    .execute(&mut *conn)
    .await?;
    let n = sqlx::query(
        "INSERT INTO ref.security AS t (thscode, ticker, name, exchange, board, list_date, first_seen, last_seen)
         SELECT thscode, ticker, name, exchange,
                CASE WHEN exchange = 'BJ' THEN 'bse' WHEN ticker ~ '^30[01]' THEN 'chinext' WHEN ticker ~ '^68[89]' THEN 'star' ELSE 'main' END,
                list_date, $1, $1
         FROM _sec
         ON CONFLICT (thscode) DO UPDATE
           SET name = EXCLUDED.name, list_date = EXCLUDED.list_date, last_seen = EXCLUDED.last_seen, delisted_on = NULL, updated_at = now()",
    )
    .bind(today)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    sqlx::query(
        "INSERT INTO ref.instrument AS t (thscode, ticker, name, asset_type, exchange, board, list_date, first_seen, last_seen)
         SELECT thscode, ticker, name, 'stock', exchange,
                CASE WHEN exchange = 'BJ' THEN 'bse' WHEN ticker ~ '^30[01]' THEN 'chinext' WHEN ticker ~ '^68[89]' THEN 'star' ELSE 'main' END,
                list_date, $1, $1
         FROM _sec
         ON CONFLICT (thscode) DO UPDATE
           SET ticker = EXCLUDED.ticker, name = EXCLUDED.name, exchange = EXCLUDED.exchange,
               board = EXCLUDED.board, list_date = EXCLUDED.list_date, last_seen = EXCLUDED.last_seen,
               delisted_on = NULL, updated_at = now()",
    )
    .bind(today)
    .execute(&mut *conn)
    .await?;
    // 简称变化（含 ST 戴帽摘帽）：关闭旧区间，打开新区间
    sqlx::query(
        "UPDATE ref.security_name_hist h SET valid_to = $1
         FROM _sec s WHERE s.thscode = h.thscode AND h.valid_to IS NULL AND h.name <> s.name AND h.valid_from < $1",
    )
    .bind(today)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO ref.security_name_hist (thscode, name, valid_from)
         SELECT s.thscode, s.name, $1 FROM _sec s
         WHERE NOT EXISTS (SELECT 1 FROM ref.security_name_hist h WHERE h.thscode = s.thscode AND h.valid_to IS NULL)
         ON CONFLICT (thscode, valid_from) DO UPDATE SET name = EXCLUDED.name, valid_to = NULL",
    )
    .bind(today)
    .execute(&mut *conn)
    .await?;
    // 本次完整列表里不再出现的证券，推断为退市（列表过短时不推断，防止接口异常导致误判）
    let listed: i64 = sqlx::query_scalar("SELECT count(*) FROM _sec")
        .fetch_one(&mut *conn)
        .await?;
    if listed > 3000 {
        sqlx::query("UPDATE ref.security SET delisted_on = last_seen + 1, updated_at = now() WHERE last_seen < $1 AND delisted_on IS NULL")
            .bind(today)
            .execute(&mut *conn)
            .await?;
        sqlx::query("UPDATE ref.instrument SET delisted_on = last_seen + 1, updated_at = now() WHERE asset_type = 'stock' AND last_seen < $1 AND delisted_on IS NULL")
            .bind(today)
            .execute(&mut *conn)
            .await?;
    }
    Ok(n as usize)
}

async fn etf_security(
    conn: &mut PgConnection,
    data: &Value,
    trade_date: Option<NaiveDate>,
    asset_type: Option<&str>,
) -> anyhow::Result<usize> {
    if asset_type != Some("fund-etf") {
        return Ok(0);
    }
    let today = trade_date.unwrap_or_else(crate::time::today_cn);
    sqlx::query("CREATE TEMP TABLE _etf (thscode TEXT PRIMARY KEY, ticker TEXT, name TEXT, exchange TEXT, list_date DATE) ON COMMIT DROP")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO _etf
         SELECT DISTINCT ON (e->'p'->>'thscode') e->'p'->>'thscode', e->'p'->>'ticker', e->'p'->>'name', e->'p'->>'exchange',
                CASE WHEN e->'p'->>'list_date' ~ '^\\d{4}-\\d{2}-\\d{2}$' THEN (e->'p'->>'list_date')::date END
         FROM jsonb_array_elements($1) e
         WHERE e->'p'->>'asset_type' = 'fund-etf' AND e->'p'->>'exchange' IN ('SH', 'SZ')",
    )
    .bind(data)
    .execute(&mut *conn)
    .await?;
    let n = sqlx::query(
        "INSERT INTO ref.instrument AS t (thscode, ticker, name, asset_type, exchange, board, list_date, first_seen, last_seen)
         SELECT thscode, ticker, name, 'etf', exchange, 'etf', list_date, $1, $1 FROM _etf
         ON CONFLICT (thscode) DO UPDATE
           SET ticker = EXCLUDED.ticker, name = EXCLUDED.name, asset_type = 'etf', exchange = EXCLUDED.exchange,
               board = 'etf', list_date = EXCLUDED.list_date, last_seen = EXCLUDED.last_seen,
               delisted_on = NULL, updated_at = now()",
    )
    .bind(today)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    sqlx::query(
        "INSERT INTO ref.etf_security AS t (thscode, ticker, name, exchange, list_date, first_seen, last_seen)
         SELECT thscode, ticker, name, exchange, list_date, $1, $1 FROM _etf
         ON CONFLICT (thscode) DO UPDATE
           SET ticker = EXCLUDED.ticker, name = EXCLUDED.name, exchange = EXCLUDED.exchange,
               list_date = EXCLUDED.list_date, last_seen = EXCLUDED.last_seen, delisted_on = NULL, updated_at = now()",
    )
    .bind(today)
    .execute(&mut *conn)
    .await?;
    let listed: i64 = sqlx::query_scalar("SELECT count(*) FROM _etf")
        .fetch_one(&mut *conn)
        .await?;
    if listed > 100 {
        sqlx::query("UPDATE ref.etf_security SET delisted_on = last_seen + 1, updated_at = now() WHERE last_seen < $1 AND delisted_on IS NULL")
            .bind(today)
            .execute(&mut *conn)
            .await?;
        sqlx::query("UPDATE ref.instrument SET delisted_on = last_seen + 1, updated_at = now() WHERE asset_type = 'etf' AND last_seen < $1 AND delisted_on IS NULL")
            .bind(today)
            .execute(&mut *conn)
            .await?;
    }
    Ok(n as usize)
}

async fn index_info(
    conn: &mut PgConnection,
    data: &Value,
    trade_date: Option<NaiveDate>,
    tag: Option<&str>,
) -> anyhow::Result<usize> {
    let today = trade_date.unwrap_or_else(crate::time::today_cn);
    // 同花顺目录带有分类（tag）；代码表里的指数只补齐缺少的条目，不改已有分类
    let res = sqlx::query(
        "INSERT INTO ref.index_info AS t (thscode, name, category, first_seen, last_seen)
         SELECT DISTINCT ON (e->'p'->>'thscode') e->'p'->>'thscode', e->'p'->>'name',
                coalesce($2, CASE WHEN e->'p'->>'thscode' LIKE '%.TI' THEN 'ths' ELSE 'exchange' END), $3, $3
         FROM jsonb_array_elements($1) e
         WHERE e->'p'->>'thscode' IS NOT NULL AND ($2 IS NOT NULL OR e->'p'->>'asset_type' = 'a-share-index')
         ON CONFLICT (thscode) DO UPDATE
           SET name = EXCLUDED.name, last_seen = EXCLUDED.last_seen,
               category = CASE WHEN $2 IS NOT NULL THEN EXCLUDED.category ELSE t.category END",
    )
    .bind(data)
    .bind(tag)
    .bind(today)
        .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected() as usize)
}

async fn index_member(
    conn: &mut PgConnection,
    data: &Value,
    trade_date: Option<NaiveDate>,
    index_code: Option<&str>,
) -> anyhow::Result<usize> {
    let Some(index_code) = index_code else {
        bail!("index.members 任务缺少 thscode")
    };
    let today = trade_date.unwrap_or_else(crate::time::today_cn);
    sqlx::query("INSERT INTO ref.index_info (thscode, name, category, first_seen, last_seen) VALUES ($1, $1, 'unknown', $2, $2) ON CONFLICT DO NOTHING")
        .bind(index_code)
        .bind(today)
        .execute(&mut *conn)
        .await?;
    let codes: Vec<String> = data
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    e.pointer("/p/thscode")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    // 已退出成分的：关闭区间
    let closed = sqlx::query(
        "UPDATE ref.index_member SET valid_to = $2 WHERE index_code = $1 AND valid_to IS NULL AND NOT (thscode = ANY($3)) AND valid_from < $2",
    )
    .bind(index_code)
    .bind(today)
    .bind(&codes)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    // 新进入成分的：打开区间
    let opened = sqlx::query(
        "INSERT INTO ref.index_member (index_code, thscode, valid_from)
         SELECT $1, c, $2 FROM unnest($3::text[]) c
         WHERE NOT EXISTS (SELECT 1 FROM ref.index_member m WHERE m.index_code = $1 AND m.thscode = c AND m.valid_to IS NULL)
         ON CONFLICT (index_code, thscode, valid_from) DO UPDATE SET valid_to = NULL",
    )
    .bind(index_code)
    .bind(today)
    .bind(&codes)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok((closed + opened) as usize)
}
