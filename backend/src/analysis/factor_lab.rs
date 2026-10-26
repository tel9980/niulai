//! 因子研究：横截面因子检验（IC、分组收益、换手、衰减、过拟合概率）和多因子组合回测。
//! 读库只在 [`load`]；检验 [`evaluate`] 和组合模拟 [`simulate`] 是纯计算，单元测试直接构造面板。
//! 规则见 docs/domains/factor_research.md。

use std::collections::{HashMap, HashSet};

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;

use super::costs::{Costs, order_blocked};
use super::factors::{self, FactorCategory, FactorId, FundamentalSnapshot, PriceFactors, Report};
use super::fundamentals::{BALANCE, CASHFLOW, INCOME};
use super::market::calendar_days_for;
use super::prices;
use super::research::{self, RiskMetrics};
use super::securities;
use super::universe::Universe;
use crate::instrument::AssetType;

/// 因子的非价格输入中只写 `raw.record` 的数据集：三张季度财报（[`load_fundamentals`]）
pub const INPUT_DATASETS: [&str; 3] = [INCOME, BALANCE, CASHFLOW];
/// 因子的非价格输入所在的类型化表（`datasets.yaml` 的 sink）：行业成分（[`load_industries`]），
/// 证券信息、名称和 ST（`securities::load`）。这些数据或 [`INPUT_DATASETS`] 有变化时，
/// 应用层更新 `ops.derived_version` 的 `factor_input` 行，账户的因子排名据此失效；价格的变化由 `adj_factor` 表示
pub const INPUT_TABLES: [&str; 4] = [
    "ref.index_member",
    "ref.index_info",
    "ref.security",
    "ref.etf_security",
];

/// 组合回测的基准：全市场等权（含分红）
pub const EQUAL_WEIGHT_NAME: &str = "全市场等权（含分红）";
/// 因子检验的区间最长几年：全市场日线压缩后约每年 15 MB
pub const MAX_TEST_YEARS: i64 = 8;
/// 组合回测的区间最长几年
pub const MAX_PORTFOLIO_YEARS: i64 = 5;
/// 一个交易日至少有多少只股票有效才参与统计
const MIN_CROSS_SECTION: usize = 50;
/// 衰减分析看的持有期（交易日）
pub const DECAY_HORIZONS: [usize; 4] = [1, 5, 10, 20];

const IN_UNIVERSE: u8 = 1;
const BLOCK_BUY: u8 = 2;
const BLOCK_SELL: u8 = 4;

/// 压缩后的一根日线：日历下标、前复权开盘价和收盘价、股票池和一字板标记
#[derive(Clone, Copy, Debug)]
pub struct CBar {
    pub day: u16,
    pub flags: u8,
    pub q_open: f32,
    pub q_close: f32,
}

#[derive(Clone, Debug)]
pub struct Series {
    pub code: String,
    pub industry: Option<u16>,
    pub bars: Vec<CBar>,
}

impl Series {
    fn at(&self, day: usize) -> Option<usize> {
        self.bars
            .binary_search_by_key(&day, |b| b.day as usize)
            .ok()
    }

    /// 第一根日期不早于 day 的日线
    fn from(&self, day: usize) -> usize {
        self.bars.partition_point(|b| (b.day as usize) < day)
    }
}

/// 一个取样日的横截面：每个因子一列
#[derive(Clone, Debug, Default)]
pub struct EvalDay {
    pub stocks: Vec<u32>,
    pub size: Vec<f64>,
    /// values[k][j]：第 k 个因子在第 j 只股票上的原始值
    pub values: Vec<Vec<f64>>,
}

/// 加载好的全市场面板
pub struct Panel {
    pub calendar: Vec<NaiveDate>,
    pub start_idx: usize,
    pub factors: Vec<FactorId>,
    pub stocks: Vec<Series>,
    /// 取样日（日历下标）和对应的横截面
    pub eval_days: Vec<usize>,
    pub evals: Vec<EvalDay>,
    /// 当天有日线的股票太少（同步缺口），不参与取样和成交
    pub thin: Vec<bool>,
    /// 全市场等权日收益（前一天在股票池里的股票）
    pub market: Vec<f64>,
    /// 区间内每个交易日应该有多少只股票有日线：取 `counts` 在取样区间内的最大值。
    /// 不能取中位数——回补没跑完时所有日子一起偏少，中位数同样偏少，反而看不出缺数据
    pub expected: u32,
}

pub struct Spec {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub factors: Vec<FactorId>,
    pub universe: Universe,
    /// 每隔几个交易日取一次横截面
    pub step: usize,
}

/// 预处理选项
#[derive(Clone, Copy, Debug)]
pub struct Prep {
    /// 对行业和规模（成交额）做中性化
    pub neutralize: bool,
    /// 每个取样日剔除规模（成交额）最小的百分之几，对应 CH-3 模型剔除壳价值股的做法
    pub exclude_small_pct: f64,
}

// @project-doc docs/domains/factor_research.md#data
/// 读一次全市场日线，压缩保存，并在取样日计算全部因子的原始值
pub async fn load(pool: &PgPool, spec: &Spec) -> anyhow::Result<Option<Panel>> {
    let lo =
        spec.start - chrono::Duration::days(calendar_days_for(factors::WARMUP_BARS as u32 + 10));
    let calendar: Vec<NaiveDate> = sqlx::query_scalar(
        "SELECT cal_date FROM ref.trading_day WHERE cal_date > $1 AND cal_date <= $2 ORDER BY cal_date",
    )
    .bind(lo)
    .bind(spec.end)
    .fetch_all(pool)
    .await?;
    let start_idx = calendar.partition_point(|d| *d < spec.start);
    if start_idx >= calendar.len() || calendar.len() > u16::MAX as usize {
        return Ok(None);
    }
    let mut eval_days: Vec<usize> = (start_idx..calendar.len())
        .step_by(spec.step.max(1))
        .collect();
    if eval_days.last() != Some(&(calendar.len() - 1)) {
        eval_days.push(calendar.len() - 1);
    }
    let sec = securities::load(pool).await?;
    let industries = load_industries(pool).await?;
    let fundamentals = if spec
        .factors
        .iter()
        .any(|f| factors::info(*f).category == FactorCategory::Fundamental)
    {
        load_fundamentals(pool, spec.start.year() - 3).await?
    } else {
        HashMap::new()
    };
    let nf = spec.factors.len();
    let mut evals = vec![
        EvalDay {
            values: vec![vec![]; nf],
            ..EvalDay::default()
        };
        eval_days.len()
    ];
    let mut stocks: Vec<Series> = vec![];
    let mut counts = vec![0u32; calendar.len()];
    prices::for_each_stock(pool, lo, spec.end, |code, bars| {
        let Some(meta) = sec.meta.get(code) else {
            return;
        };
        if meta.asset_type != AssetType::Stock {
            return;
        }
        let history = sec.st_history(code);
        let pf = PriceFactors::new(bars);
        let snaps = fundamentals.get(code);
        let mut compact = Vec::with_capacity(bars.len());
        for (i, b) in bars.iter().enumerate() {
            let Ok(day) = calendar.binary_search(&b.date) else {
                continue;
            };
            counts[day] += 1;
            let prev = if i > 0 { bars[i - 1].q_close } else { b.q_open };
            let one_price = b.high == b.low;
            let mut flags = 0;
            if spec.universe.contains(bars, i, meta, history) {
                flags |= IN_UNIVERSE;
            }
            if order_blocked(true, one_price, b.q_open, prev) {
                flags |= BLOCK_BUY;
            }
            if order_blocked(false, one_price, b.q_open, prev) {
                flags |= BLOCK_SELL;
            }
            compact.push(CBar {
                day: day as u16,
                flags,
                q_open: b.q_open as f32,
                q_close: b.q_close as f32,
            });
        }
        if compact.iter().all(|b| (b.day as usize) < start_idx) {
            return;
        }
        let idx = stocks.len() as u32;
        for (k, &day) in eval_days.iter().enumerate() {
            let Ok(i) = bars.binary_search_by_key(&calendar[day], |b| b.date) else {
                continue;
            };
            if !spec.universe.contains(bars, i, meta, history) {
                continue;
            }
            let e = &mut evals[k];
            e.stocks.push(idx);
            e.size.push(pf.size(i));
            let fund = snaps.and_then(|s| factors::fundamental_at(s, bars[i].date));
            for (fi, id) in spec.factors.iter().enumerate() {
                let v = match factors::info(*id).category {
                    FactorCategory::Fundamental => {
                        fund.map_or(f64::NAN, |s| factors::fundamental_value(s, *id))
                    }
                    FactorCategory::PriceVolume => pf.value(*id, i),
                };
                e.values[fi].push(v);
            }
        }
        stocks.push(Series {
            code: code.to_string(),
            industry: industries.get(code).copied(),
            bars: compact,
        });
    })
    .await?;
    if stocks.is_empty() {
        return Ok(None);
    }
    Ok(Some(finish(
        calendar,
        start_idx,
        spec.factors.clone(),
        stocks,
        eval_days,
        evals,
        &counts,
    )))
}

/// 扫描之后的公共步骤：标记同步缺口日、算市场收益和特质波动率。测试用它从合成数据构造面板
pub fn finish(
    calendar: Vec<NaiveDate>,
    start_idx: usize,
    factor_ids: Vec<FactorId>,
    stocks: Vec<Series>,
    eval_days: Vec<usize>,
    evals: Vec<EvalDay>,
    counts: &[u32],
) -> Panel {
    // 有日线的股票数不到区间内中位数一半的交易日视为同步缺口
    let mut sorted: Vec<u32> = counts[start_idx..].to_vec();
    sorted.sort_unstable();
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
    let thin: Vec<bool> = counts.iter().map(|c| *c * 2 < median).collect();
    let expected = counts[start_idx..].iter().copied().max().unwrap_or(0);
    let mut sum = vec![0.0f64; calendar.len()];
    let mut n = vec![0u32; calendar.len()];
    for s in &stocks {
        for w in s.bars.windows(2) {
            if w[0].flags & IN_UNIVERSE == 0 || w[0].q_close <= 0.0 {
                continue;
            }
            let r = f64::from(w[1].q_close) / f64::from(w[0].q_close) - 1.0;
            if r.is_finite() {
                sum[w[1].day as usize] += r;
                n[w[1].day as usize] += 1;
            }
        }
    }
    let market: Vec<f64> = sum
        .iter()
        .zip(&n)
        .map(|(s, c)| if *c > 0 { s / f64::from(*c) } else { f64::NAN })
        .collect();
    let mut panel = Panel {
        calendar,
        start_idx,
        factors: factor_ids,
        stocks,
        eval_days,
        evals,
        thin,
        market,
        expected,
    };
    if let Some(fi) = panel.factors.iter().position(|f| *f == FactorId::IdioVol) {
        for k in 0..panel.evals.len() {
            let day = panel.eval_days[k];
            for j in 0..panel.evals[k].stocks.len() {
                let s = &panel.stocks[panel.evals[k].stocks[j] as usize];
                let v = s.at(day).map_or(f64::NAN, |i| idio_at(s, i, &panel.market));
                panel.evals[k].values[fi][j] = v;
            }
        }
    }
    panel
}

fn idio_at(s: &Series, i: usize, market: &[f64]) -> f64 {
    const N: usize = 20;
    if i < N {
        return f64::NAN;
    }
    let (mut stock, mut mkt) = (Vec::with_capacity(N), Vec::with_capacity(N));
    for k in i + 1 - N..=i {
        let (a, b) = (s.bars[k - 1], s.bars[k]);
        stock.push(if a.q_close > 0.0 {
            f64::from(b.q_close) / f64::from(a.q_close) - 1.0
        } else {
            f64::NAN
        });
        mkt.push(market[b.day as usize]);
    }
    factors::idio_vol(&stock, &mkt)
}

/// 每只股票当前所属的同花顺行业（二级）。行业成分没有历史，用当前归属近似；行业很少变化
async fn load_industries(pool: &PgPool) -> sqlx::Result<HashMap<String, u16>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "WITH cur AS (
           SELECT m.thscode, m.index_code FROM ref.index_member m
           JOIN ref.index_info i ON i.thscode = m.index_code
           WHERE i.category = 'industry' AND m.valid_to IS NULL
         ), n AS (SELECT index_code, count(*) AS c FROM cur GROUP BY index_code)
         SELECT DISTINCT ON (cur.thscode) cur.thscode, cur.index_code
         FROM cur JOIN n USING (index_code)
         ORDER BY cur.thscode, n.c DESC, cur.index_code",
    )
    .fetch_all(pool)
    .await?;
    let mut ids: HashMap<String, u16> = HashMap::new();
    let mut out = HashMap::with_capacity(rows.len());
    for (code, industry) in rows {
        let next = ids.len() as u16;
        out.insert(code, *ids.entry(industry).or_insert(next));
    }
    Ok(out)
}

type IncomeRow = (String, i32, String, Option<i64>, Option<f64>);
type BalanceRow = (String, i32, String, Option<f64>, Option<f64>);
type CashRow = (String, i32, String, Option<f64>);

/// 三张财报按（证券、年度、季度）合并成快照
async fn load_fundamentals(
    pool: &PgPool,
    from_year: i32,
) -> sqlx::Result<HashMap<String, Vec<FundamentalSnapshot>>> {
    let income: Vec<IncomeRow> = sqlx::query_as(
        "SELECT thscode, (payload->>'fiscal_year')::int, payload->>'fiscal_period',
                (payload->>'report_date_ms')::bigint, (payload->>'parent_holder_net_profit')::float8
         FROM raw.record WHERE dataset_id = $2 AND thscode IS NOT NULL
           AND (payload->>'fiscal_year')::int >= $1",
    )
    .bind(from_year)
    .bind(INCOME)
    .fetch_all(pool)
    .await?;
    let balance: Vec<BalanceRow> = sqlx::query_as(
        "SELECT thscode, (payload->>'fiscal_year')::int, payload->>'fiscal_period',
                (payload->>'holder_equity_total')::float8, (payload->>'assets_total')::float8
         FROM raw.record WHERE dataset_id = $2 AND thscode IS NOT NULL
           AND (payload->>'fiscal_year')::int >= $1",
    )
    .bind(from_year)
    .bind(BALANCE)
    .fetch_all(pool)
    .await?;
    let cash: Vec<CashRow> = sqlx::query_as(
        "SELECT thscode, (payload->>'fiscal_year')::int, payload->>'fiscal_period',
                (payload->>'act_cash_flow_net')::float8
         FROM raw.record WHERE dataset_id = $2 AND thscode IS NOT NULL
           AND (payload->>'fiscal_year')::int >= $1",
    )
    .bind(from_year)
    .bind(CASHFLOW)
    .fetch_all(pool)
    .await?;
    let quarter = |p: &str| match p {
        "Q1" => Some(1u8),
        "Q2" => Some(2),
        "Q3" => Some(3),
        "Q4" => Some(4),
        _ => None,
    };
    let mut reports: HashMap<(String, i32, u8), Report> = HashMap::new();
    for (code, fy, fp, ms, np) in income {
        let Some(q) = quarter(&fp) else { continue };
        let report_date = ms.and_then(crate::time::ms_to_cn_date);
        reports.insert(
            (code, fy, q),
            Report {
                fiscal_year: fy,
                quarter: q,
                report_date,
                parent_net_profit: np,
                equity: None,
                assets: None,
                operating_cash_flow: None,
            },
        );
    }
    for (code, fy, fp, equity, assets) in balance {
        if let Some(r) = quarter(&fp).and_then(|q| reports.get_mut(&(code, fy, q))) {
            r.equity = equity;
            r.assets = assets;
        }
    }
    for (code, fy, fp, cfo) in cash {
        if let Some(r) = quarter(&fp).and_then(|q| reports.get_mut(&(code, fy, q))) {
            r.operating_cash_flow = cfo;
        }
    }
    let mut by_code: HashMap<String, Vec<Report>> = HashMap::new();
    for ((code, _, _), r) in reports {
        by_code.entry(code).or_default().push(r);
    }
    Ok(by_code
        .into_iter()
        .map(|(code, list)| (code, factors::fundamental_snapshots(list)))
        .collect())
}

// ───────────────────────── 横截面预处理 ─────────────────────────

/// 去极值、标准化，可选行业和规模中性化。NaN 保持 NaN。`size` 为空时只做行业中性化
/// （规模因子本身不能对自己中性化）；中性化后几乎没有剩余变化时，这一期视为没有信息，全部为 NaN，
/// 否则标准化会把浮点误差放大成虚假的排序
pub fn standardize(
    values: &[f64],
    size: Option<&[f64]>,
    industry: &[Option<u16>],
    neutralize: bool,
) -> Vec<f64> {
    let finite: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.len() < 3 {
        return vec![f64::NAN; values.len()];
    }
    // 中位数 ± 3 倍稳健标准差（1.4826 × MAD）截尾
    let med = median(&finite);
    let mad = median(&finite.iter().map(|v| (v - med).abs()).collect::<Vec<_>>()) * 1.4826;
    let (lo, hi) = if mad > 0.0 {
        (med - 3.0 * mad, med + 3.0 * mad)
    } else {
        (f64::NEG_INFINITY, f64::INFINITY)
    };
    let mut x: Vec<f64> = values.iter().map(|v| v.clamp(lo, hi)).collect();
    zscore(&mut x);
    if neutralize {
        x = neutralize_industry_size(&x, size, industry);
        if mean_std(&x).is_none_or(|(_, sd)| sd < 1e-6) {
            return vec![f64::NAN; values.len()];
        }
        zscore(&mut x);
    }
    x
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

fn zscore(x: &mut [f64]) {
    let finite: Vec<f64> = x.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.len() < 2 {
        return;
    }
    let mean = finite.iter().sum::<f64>() / finite.len() as f64;
    let sd =
        (finite.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (finite.len() - 1) as f64).sqrt();
    for v in x.iter_mut() {
        *v = if sd > 1e-12 {
            (*v - mean) / sd
        } else {
            0.0 * *v
        };
    }
}

/// 对行业哑变量和规模做回归取残差。行业是哑变量，按 Frisch–Waugh 定理等价于：
/// 因子和规模都先减去行业均值，再用规模做一元回归。规模缺失的股票只做行业去均值
fn neutralize_industry_size(x: &[f64], size: Option<&[f64]>, industry: &[Option<u16>]) -> Vec<f64> {
    let nan = vec![f64::NAN; x.len()];
    let size = size.unwrap_or(&nan);
    let group = |i: usize| industry[i].map_or(u32::MAX, u32::from);
    let mut mean_x: HashMap<u32, (f64, f64)> = HashMap::new();
    let mut mean_s: HashMap<u32, (f64, f64)> = HashMap::new();
    for i in 0..x.len() {
        if x[i].is_finite() {
            let e = mean_x.entry(group(i)).or_default();
            e.0 += x[i];
            e.1 += 1.0;
            if size[i].is_finite() {
                let e = mean_s.entry(group(i)).or_default();
                e.0 += size[i];
                e.1 += 1.0;
            }
        }
    }
    let dx: Vec<f64> = (0..x.len())
        .map(|i| {
            let (s, n) = mean_x.get(&group(i)).copied().unwrap_or((0.0, 1.0));
            x[i] - s / n
        })
        .collect();
    let ds: Vec<f64> = (0..x.len())
        .map(|i| match mean_s.get(&group(i)) {
            Some((s, n)) if size[i].is_finite() && x[i].is_finite() => size[i] - s / n,
            _ => f64::NAN,
        })
        .collect();
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for i in 0..x.len() {
        if dx[i].is_finite() && ds[i].is_finite() {
            sxy += dx[i] * ds[i];
            sxx += ds[i] * ds[i];
        }
    }
    let beta = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    (0..x.len())
        .map(|i| {
            if ds[i].is_finite() {
                dx[i] - beta * ds[i]
            } else {
                dx[i]
            }
        })
        .collect()
}

/// 秩（并列取平均秩），从 0 开始
pub fn ranks(v: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|a, b| v[*a].total_cmp(&v[*b]));
    let mut out = vec![0.0; v.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && v[idx[j + 1]] == v[idx[i]] {
            j += 1;
        }
        let r = (i + j) as f64 / 2.0;
        for k in &idx[i..=j] {
            out[*k] = r;
        }
        i = j + 1;
    }
    out
}

pub fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    if a.len() < 3 {
        return f64::NAN;
    }
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        sab += (x - ma) * (y - mb);
        saa += (x - ma).powi(2);
        sbb += (y - mb).powi(2);
    }
    if saa > 0.0 && sbb > 0.0 {
        sab / (saa * sbb).sqrt()
    } else {
        f64::NAN
    }
}

pub fn spearman(a: &[f64], b: &[f64]) -> f64 {
    pearson(&ranks(a), &ranks(b))
}

/// 一个取样日、处理后的全部因子列（含合成列）。列顺序与 `columns` 一致；方向已统一成「越大越好」
struct Processed {
    stocks: Vec<u32>,
    /// columns[k][j]
    columns: Vec<Vec<f64>>,
}

/// 预处理一个取样日：剔除小规模股票、逐列标准化并统一方向，再按权重合成（权重为空时不合成）
fn process(panel: &Panel, k: usize, prep: Prep, weights: &[f64]) -> Processed {
    let e = &panel.evals[k];
    let mut keep: Vec<usize> = (0..e.stocks.len()).collect();
    if prep.exclude_small_pct > 0.0 {
        let mut sizes: Vec<f64> = e.size.iter().copied().filter(|v| v.is_finite()).collect();
        sizes.sort_by(f64::total_cmp);
        if !sizes.is_empty() {
            let cut_at = ((sizes.len() as f64 * prep.exclude_small_pct / 100.0) as usize)
                .min(sizes.len() - 1);
            let cut = sizes[cut_at];
            keep.retain(|&j| e.size[j].is_finite() && e.size[j] >= cut);
        }
    }
    let stocks: Vec<u32> = keep.iter().map(|&j| e.stocks[j]).collect();
    let size: Vec<f64> = keep.iter().map(|&j| e.size[j]).collect();
    let industry: Vec<Option<u16>> = stocks
        .iter()
        .map(|s| panel.stocks[*s as usize].industry)
        .collect();
    let mut columns: Vec<Vec<f64>> = panel
        .factors
        .iter()
        .enumerate()
        .map(|(fi, id)| {
            let raw: Vec<f64> = keep.iter().map(|&j| e.values[fi][j]).collect();
            let sign = if factors::info(*id).higher_is_better {
                1.0
            } else {
                -1.0
            };
            let size = (*id != FactorId::Liquidity).then_some(size.as_slice());
            standardize(&raw, size, &industry, prep.neutralize)
                .into_iter()
                .map(|v| v * sign)
                .collect()
        })
        .collect();
    if !weights.is_empty() {
        let total: f64 = weights.iter().map(|w| w.abs()).sum();
        let combined: Vec<f64> = (0..stocks.len())
            .map(|j| {
                let (mut acc, mut used) = (0.0, 0.0);
                for (fi, w) in weights.iter().enumerate() {
                    let v = columns[fi][j];
                    if v.is_finite() {
                        acc += w * v;
                        used += w.abs();
                    }
                }
                // 至少一半权重的因子有值才合成，避免只靠一个因子排出前几名
                if total > 0.0 && used * 2.0 >= total {
                    acc / used
                } else {
                    f64::NAN
                }
            })
            .collect();
        let mut combined = combined;
        zscore(&mut combined);
        columns.push(combined);
    }
    Processed { stocks, columns }
}

/// 最后一个取样日按合成得分从高到低排出的股票（代码、得分）；当天数据太少时为空。账户的多因子调仓用它
pub fn ranking(panel: &Panel, prep: Prep, weights: &[f64]) -> Vec<(String, f64)> {
    let Some(k) = panel.evals.len().checked_sub(1) else {
        return vec![];
    };
    if panel.thin[panel.eval_days[k]] {
        return vec![];
    }
    let pr = process(panel, k, prep, weights);
    let Some(score) = pr.columns.last() else {
        return vec![];
    };
    let mut ranked: Vec<(String, f64)> = pr
        .stocks
        .iter()
        .zip(score)
        .filter(|(_, v)| v.is_finite())
        .map(|(s, v)| (panel.stocks[*s as usize].code.clone(), *v))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

// ───────────────────────── 因子检验 ─────────────────────────

/// 前向收益：取样日 day 收盘后决定，下一个交易日开盘买入，horizon 个交易日后的开盘卖出（前复权价）。
/// 买入日停牌或一字涨停买不进时为 NaN；卖出日停牌或一字跌停时顺延到下一根能卖的日线；
/// 之后再没有日线（退市或停牌到期末）时按最后收盘价；持有期超出数据范围时为 NaN
fn forward_return(panel: &Panel, s: &Series, day: usize, horizon: usize) -> f64 {
    let (entry_day, exit_day) = (day + 1, day + 1 + horizon);
    if exit_day >= panel.calendar.len() {
        return f64::NAN;
    }
    let Some(i) = s.at(entry_day) else {
        return f64::NAN;
    };
    if s.bars[i].flags & BLOCK_BUY != 0 {
        return f64::NAN;
    }
    let entry = f64::from(s.bars[i].q_open);
    let mut k = s.from(exit_day);
    while k < s.bars.len() && s.bars[k].flags & BLOCK_SELL != 0 {
        k += 1;
    }
    let exit = match s.bars.get(k) {
        Some(b) => f64::from(b.q_open),
        None => f64::from(s.bars.last().map_or(f32::NAN, |b| b.q_close)),
    };
    if entry > 0.0 {
        exit / entry - 1.0
    } else {
        f64::NAN
    }
}

#[derive(Clone)]
pub struct TestParams {
    pub horizon: usize,
    pub quantiles: usize,
    pub prep: Prep,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct FactorDecayPoint {
    /// 持有期（交易日）
    pub horizon: usize,
    pub rank_ic: Option<f64>,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct FactorTestPoint {
    pub date: NaiveDate,
    pub ic: Option<f64>,
    pub rank_ic: Option<f64>,
    /// 多空组合（最好一组减最差一组）的累计净值，第一期前为 1
    pub long_short_nav: f64,
    /// 各组的累计净值，第一组是预期最差的一组
    pub quantile_navs: Vec<f64>,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct FactorTestStats {
    /// 合成因子为空
    pub id: Option<FactorId>,
    pub name: String,
    pub higher_is_better: bool,
    /// 有效的取样期数
    pub periods: usize,
    pub avg_stocks: f64,
    pub ic_mean: Option<f64>,
    pub rank_ic_mean: Option<f64>,
    pub rank_ic_std: Option<f64>,
    /// RankIC 均值 ÷ 标准差
    pub rank_icir: Option<f64>,
    /// RankIC 均值的 t 值；Harvey、Liu、Zhu 建议新因子 |t| ≥ 3 才算显著
    pub rank_ic_t: Option<f64>,
    /// RankIC 与预期方向一致的期数占比
    pub hit_rate: Option<f64>,
    /// 各组每期平均收益，第一组是预期最差的一组
    pub quantile_returns: Vec<f64>,
    /// 各组年化收益
    pub quantile_annual: Vec<Option<f64>>,
    pub long_short_annual: Option<f64>,
    pub long_short_t: Option<f64>,
    pub long_short_sharpe: Option<f64>,
    pub long_short_max_drawdown: f64,
    /// 最好一组相对股票池平均的年化超额
    pub top_excess_annual: Option<f64>,
    /// 最好一组每期换掉的比例
    pub top_turnover: Option<f64>,
    /// 组序号与组收益的秩相关：1 表示从最差组到最好组单调上升
    pub monotonicity: Option<f64>,
    pub decay: Vec<FactorDecayPoint>,
    /// 方向与预期一致（RankIC 均值为正）
    pub consistent: bool,
    /// |RankIC t| ≥ 3 且方向一致
    pub significant: bool,
    pub verdict: String,
    pub points: Vec<FactorTestPoint>,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct FactorOverfit {
    /// 参与比较的候选（各因子的多空组合）
    pub candidates: usize,
    /// 回测过拟合概率（CSCV）：在这些候选里按样本内 Sharpe 挑最好的，样本外排到后一半的概率
    pub pbo: Option<f64>,
    pub combinations: usize,
    /// 多空 Sharpe 最高的候选
    pub best: Option<String>,
    /// 最好候选的 Deflated Sharpe：考虑挑选次数之后真实 Sharpe 大于 0 的概率
    pub deflated_sharpe: Option<f64>,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct FactorTestOutcome {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub horizon: usize,
    pub quantiles: usize,
    pub factors: Vec<FactorTestStats>,
    /// 因子之间的平均横截面秩相关，行列顺序同 `factors`
    pub correlation: Vec<Vec<Option<f64>>>,
    pub overfit: FactorOverfit,
    /// 区间内因同步缺口跳过的交易日
    pub thin_days: Vec<NaiveDate>,
    /// 数据覆盖情况：区间内每个取样日应有多少只、实际有多少只
    pub coverage: Coverage,
}

/// 数据覆盖情况。
///
/// `thin` 是逐日判定（当天有日线的股票数不到区间中位数一半就跳过），它假设「中位数」本身就代表
/// 全市场。首次回补还没跑完时这个假设不成立：中位数也只有一半，于是每个交易日都「看起来正常」，
/// 横截面却在用残缺样本做统计。所以这里单独算一个整体比值，让调用方能把「数据不全」和
/// 「区间内真的没股票」区分开。
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Coverage {
    /// 区间内全市场应有的股票数：同期（±30 个交易日）出现过的证券数取最大值
    pub expected: u32,
    /// 各取样日实际参与横截面的股票数的中位数
    pub observed: u32,
    /// observed / expected，0 到 1
    pub ratio: f64,
    /// 取样日总数和因缺口被跳过的天数
    pub eval_days: usize,
    pub skipped_days: usize,
}

/// 覆盖度低于这个比例就认为回补还没跑完
pub const COVERAGE_WARN_RATIO: f64 = 0.9;

impl Coverage {
    /// 覆盖不足时给用户看的一句话
    pub fn warning(&self) -> Option<String> {
        if self.ratio >= COVERAGE_WARN_RATIO {
            return None;
        }
        Some(format!(
            "数据还没补全：区间内全市场应有约 {} 只，实际每个取样日中位数 {} 只（{:.0}%）。\
             历史回补还没跑完时，横截面用的是残缺样本，IC 和分组收益会失真，先看结论要谨慎。",
            self.expected,
            self.observed,
            self.ratio * 100.0,
        ))
    }
}

fn mean_std(v: &[f64]) -> Option<(f64, f64)> {
    let v: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if v.len() < 2 {
        return None;
    }
    let m = v.iter().sum::<f64>() / v.len() as f64;
    let sd = (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt();
    Some((m, sd))
}

fn annualize(per_period: f64, periods_per_year: f64) -> Option<f64> {
    let v = (1.0 + per_period).powf(periods_per_year) - 1.0;
    v.is_finite().then_some(v)
}

fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

/// 因子检验：每个取样日做一次横截面，统计 IC、分组收益、多空、换手和衰减，最后估计挑选偏差
pub fn evaluate(panel: &Panel, p: &TestParams) -> Option<FactorTestOutcome> {
    let nf = panel.factors.len();
    let with_composite = nf >= 2;
    let weights = if with_composite {
        vec![1.0; nf]
    } else {
        vec![]
    };
    let ncol = nf + usize::from(with_composite);
    let q = p.quantiles.clamp(2, 10);
    let periods_per_year = 252.0 / p.horizon as f64;
    // 取样日：区间内、不是缺口日、次日也不是缺口日、持有期在数据范围内
    let days: Vec<usize> = (0..panel.eval_days.len())
        .filter(|&k| {
            let d = panel.eval_days[k];
            d >= panel.start_idx
                && d + 1 + p.horizon < panel.calendar.len()
                && !panel.thin[d]
                && !panel.thin[d + 1]
        })
        .collect();
    if days.is_empty() {
        return None;
    }
    struct Acc {
        ic: Vec<f64>,
        rank_ic: Vec<f64>,
        quantile: Vec<Vec<f64>>,
        long_short: Vec<f64>,
        excess: Vec<f64>,
        turnover: Vec<f64>,
        stocks: Vec<f64>,
        decay: Vec<Vec<f64>>,
        points: Vec<FactorTestPoint>,
        prev_top: Option<HashSet<u32>>,
        nav: f64,
        qnav: Vec<f64>,
    }
    let mut acc: Vec<Acc> = (0..ncol)
        .map(|_| Acc {
            ic: vec![],
            rank_ic: vec![],
            quantile: vec![vec![]; q],
            long_short: vec![],
            excess: vec![],
            turnover: vec![],
            stocks: vec![],
            decay: vec![vec![]; DECAY_HORIZONS.len()],
            points: vec![],
            prev_top: None,
            nav: 1.0,
            qnav: vec![1.0; q],
        })
        .collect();
    let mut corr_sum = vec![vec![0.0; ncol]; ncol];
    let mut corr_n = vec![vec![0usize; ncol]; ncol];
    for &k in &days {
        let day = panel.eval_days[k];
        let pr = process(panel, k, p.prep, &weights);
        let fwd: Vec<f64> = pr
            .stocks
            .iter()
            .map(|s| forward_return(panel, &panel.stocks[*s as usize], day, p.horizon))
            .collect();
        let decay_fwd: Vec<Vec<f64>> = DECAY_HORIZONS
            .iter()
            .map(|h| {
                pr.stocks
                    .iter()
                    .map(|s| forward_return(panel, &panel.stocks[*s as usize], day, *h))
                    .collect()
            })
            .collect();
        for (col, a) in pr.columns.iter().zip(acc.iter_mut()) {
            let valid: Vec<usize> = (0..col.len())
                .filter(|&j| col[j].is_finite() && fwd[j].is_finite())
                .collect();
            if valid.len() < MIN_CROSS_SECTION {
                continue;
            }
            let x: Vec<f64> = valid.iter().map(|&j| col[j]).collect();
            let y: Vec<f64> = valid.iter().map(|&j| fwd[j]).collect();
            let ic = pearson(&x, &y);
            let ric = spearman(&x, &y);
            a.ic.push(ic);
            a.rank_ic.push(ric);
            a.stocks.push(valid.len() as f64);
            for (h, f) in decay_fwd.iter().enumerate() {
                let pairs: Vec<(f64, f64)> = (0..col.len())
                    .filter(|&j| col[j].is_finite() && f[j].is_finite())
                    .map(|j| (col[j], f[j]))
                    .collect();
                if pairs.len() >= MIN_CROSS_SECTION {
                    let (xs, ys): (Vec<f64>, Vec<f64>) = pairs.into_iter().unzip();
                    a.decay[h].push(spearman(&xs, &ys));
                }
            }
            let mut order: Vec<usize> = (0..valid.len()).collect();
            order.sort_by(|i, j| x[*i].total_cmp(&x[*j]));
            let slices: Vec<&[usize]> = (0..q)
                .map(|g| &order[g * order.len() / q..(g + 1) * order.len() / q])
                .collect();
            let groups: Vec<f64> = slices
                .iter()
                .map(|sl| sl.iter().map(|i| y[*i]).sum::<f64>() / sl.len().max(1) as f64)
                .collect();
            let top: HashSet<u32> = slices[q - 1].iter().map(|i| pr.stocks[valid[*i]]).collect();
            let universe = y.iter().sum::<f64>() / y.len() as f64;
            let ls = groups[q - 1] - groups[0];
            for ((series, nav), r) in a.quantile.iter_mut().zip(a.qnav.iter_mut()).zip(&groups) {
                series.push(*r);
                *nav *= 1.0 + r;
            }
            a.long_short.push(ls);
            a.excess.push(groups[q - 1] - universe);
            if let Some(prev) = &a.prev_top
                && !top.is_empty()
            {
                a.turnover
                    .push(1.0 - prev.intersection(&top).count() as f64 / top.len() as f64);
            }
            a.prev_top = Some(top);
            a.nav *= 1.0 + ls;
            a.points.push(FactorTestPoint {
                date: panel.calendar[day],
                ic: finite(ic),
                rank_ic: finite(ric),
                long_short_nav: a.nav,
                quantile_navs: a.qnav.clone(),
            });
        }
        for c1 in 0..ncol {
            for c2 in c1 + 1..ncol {
                let pairs: Vec<(f64, f64)> = (0..pr.stocks.len())
                    .filter(|&j| pr.columns[c1][j].is_finite() && pr.columns[c2][j].is_finite())
                    .map(|j| (pr.columns[c1][j], pr.columns[c2][j]))
                    .collect();
                if pairs.len() >= MIN_CROSS_SECTION {
                    let (xs, ys): (Vec<f64>, Vec<f64>) = pairs.into_iter().unzip();
                    let r = spearman(&xs, &ys);
                    if r.is_finite() {
                        corr_sum[c1][c2] += r;
                        corr_n[c1][c2] += 1;
                    }
                }
            }
        }
    }
    let stats: Vec<FactorTestStats> = acc
        .into_iter()
        .enumerate()
        .map(|(c, a)| {
            let (id, name, higher) = if c < nf {
                let info = factors::info(panel.factors[c]);
                (Some(info.id), info.name.to_string(), info.higher_is_better)
            } else {
                (None, "等权合成".to_string(), true)
            };
            let n = a.rank_ic.len();
            let ric = mean_std(&a.rank_ic);
            let rank_ic_t = ric.and_then(|(m, sd)| finite(m / sd * (n as f64).sqrt()));
            let ls = mean_std(&a.long_short);
            let quantile_returns: Vec<f64> = a
                .quantile
                .iter()
                .map(|g| g.iter().sum::<f64>() / g.len().max(1) as f64)
                .collect();
            let monotonicity = (n > 0).then(|| {
                spearman(
                    &(0..q).map(|g| g as f64).collect::<Vec<_>>(),
                    &quantile_returns,
                )
            });
            let mut peak = 1.0f64;
            let mut mdd = 0.0f64;
            for pt in &a.points {
                peak = peak.max(pt.long_short_nav);
                mdd = mdd.max(1.0 - pt.long_short_nav / peak);
            }
            let consistent = ric.is_some_and(|(m, _)| m > 0.0);
            let significant = consistent && rank_ic_t.is_some_and(|t| t >= 3.0);
            let verdict = match (ric, rank_ic_t) {
                (None, _) | (_, None) => "有效期数太少，无法判断".to_string(),
                (Some(_), Some(t)) if t >= 3.0 => "方向与预期一致且统计显著（t ≥ 3）".to_string(),
                (Some(_), Some(t)) if t >= 2.0 => {
                    "方向与预期一致，但未达到多重检验后的显著门槛（t < 3）".to_string()
                }
                (Some(_), Some(t)) if t <= -3.0 => {
                    "与预期方向相反且显著：在这段区间里反着用才有效".to_string()
                }
                _ => "预测力不显著，接近随机".to_string(),
            };
            FactorTestStats {
                id,
                name,
                higher_is_better: higher,
                periods: n,
                avg_stocks: a.stocks.iter().sum::<f64>() / a.stocks.len().max(1) as f64,
                ic_mean: mean_std(&a.ic).map(|x| x.0),
                rank_ic_mean: ric.map(|x| x.0),
                rank_ic_std: ric.map(|x| x.1),
                rank_icir: ric.and_then(|(m, sd)| finite(m / sd)),
                rank_ic_t,
                hit_rate: (n > 0)
                    .then(|| a.rank_ic.iter().filter(|v| **v > 0.0).count() as f64 / n as f64),
                quantile_annual: quantile_returns
                    .iter()
                    .map(|r| annualize(*r, periods_per_year))
                    .collect(),
                quantile_returns,
                long_short_annual: ls.and_then(|(m, _)| annualize(m, periods_per_year)),
                long_short_t: ls
                    .and_then(|(m, sd)| finite(m / sd * (a.long_short.len() as f64).sqrt())),
                long_short_sharpe: ls.and_then(|(m, sd)| finite(m / sd * periods_per_year.sqrt())),
                long_short_max_drawdown: mdd,
                top_excess_annual: mean_std(&a.excess)
                    .and_then(|(m, _)| annualize(m, periods_per_year)),
                top_turnover: mean_std(&a.turnover).map(|x| x.0),
                monotonicity: monotonicity.and_then(finite),
                decay: DECAY_HORIZONS
                    .iter()
                    .zip(&a.decay)
                    .map(|(h, v)| FactorDecayPoint {
                        horizon: *h,
                        rank_ic: mean_std(v).map(|x| x.0),
                    })
                    .collect(),
                consistent,
                significant,
                verdict,
                points: a.points,
            }
        })
        .collect();
    let correlation: Vec<Vec<Option<f64>>> = (0..ncol)
        .map(|i| {
            (0..ncol)
                .map(|j| {
                    let (a, b) = (i.min(j), i.max(j));
                    if i == j {
                        Some(1.0)
                    } else if corr_n[a][b] > 0 {
                        Some(corr_sum[a][b] / corr_n[a][b] as f64)
                    } else {
                        None
                    }
                })
                .collect()
        })
        .collect();
    let overfit = overfit(&stats, &days, panel);
    let skipped_days = (panel.start_idx..panel.calendar.len())
        .filter(|d| panel.thin[*d])
        .count();
    let thin_days = (panel.start_idx..panel.calendar.len())
        .filter(|d| panel.thin[*d])
        .map(|d| panel.calendar[d])
        .collect();
    let coverage = coverage(panel, skipped_days);
    Some(FactorTestOutcome {
        start: panel.calendar[panel.start_idx],
        end: *panel.calendar.last().expect("日历非空"),
        horizon: p.horizon,
        quantiles: q,
        factors: stats,
        correlation,
        overfit,
        thin_days,
        coverage,
    })
}

/// 统计区间内实际参与横截面的股票数：取各取样日股票数的中位数，与全市场应有数相比
fn coverage(panel: &Panel, skipped_days: usize) -> Coverage {
    let mut per_day: Vec<u32> = (panel.start_idx..panel.calendar.len())
        .filter(|d| !panel.thin[*d])
        .map(|d| {
            panel
                .stocks
                .iter()
                .filter(|s| {
                    s.bars
                        .binary_search_by_key(&(d as u16), |b| b.day)
                        .is_ok_and(|i| s.bars[i].flags & IN_UNIVERSE != 0)
                })
                .count() as u32
        })
        .collect();
    per_day.sort_unstable();
    let observed = per_day.get(per_day.len() / 2).copied().unwrap_or(0);
    let expected = panel.expected;
    let ratio = if expected == 0 {
        1.0
    } else {
        f64::from(observed) / f64::from(expected)
    };
    Coverage {
        expected,
        observed,
        ratio: ratio.clamp(0.0, 1.0),
        eval_days: per_day.len(),
        skipped_days,
    }
}

/// 用各候选的多空每期收益估计 PBO 和最好候选的 Deflated Sharpe。期数对齐到所有候选都有值的取样日
fn overfit(stats: &[FactorTestStats], days: &[usize], panel: &Panel) -> FactorOverfit {
    let dates: Vec<NaiveDate> = days
        .iter()
        .map(|k| panel.calendar[panel.eval_days[*k]])
        .collect();
    let series: Vec<HashMap<NaiveDate, f64>> = stats
        .iter()
        .map(|s| {
            let mut prev = 1.0;
            s.points
                .iter()
                .map(|p| {
                    let r = p.long_short_nav / prev - 1.0;
                    prev = p.long_short_nav;
                    (p.date, r)
                })
                .collect()
        })
        .collect();
    let common: Vec<NaiveDate> = dates
        .into_iter()
        .filter(|d| series.iter().all(|s| s.contains_key(d)))
        .collect();
    let aligned: Vec<Vec<f64>> = series
        .iter()
        .map(|s| common.iter().map(|d| s[d]).collect())
        .collect();
    let sharpes: Vec<f64> = aligned
        .iter()
        .map(|r| research::moments(r).map_or(f64::NAN, |(m, sd, _, _)| m / sd))
        .collect();
    let best = (0..aligned.len())
        .filter(|i| sharpes[*i].is_finite())
        .max_by(|a, b| sharpes[*a].total_cmp(&sharpes[*b]));
    let finite_sr: Vec<f64> = sharpes.iter().copied().filter(|v| v.is_finite()).collect();
    let var = mean_std(&finite_sr).map(|(_, sd)| sd * sd);
    let blocks = if common.len() >= 32 { 8 } else { 4 };
    let pbo = research::pbo_cscv(&aligned, blocks);
    FactorOverfit {
        candidates: aligned.len(),
        pbo: pbo.map(|x| x.0),
        combinations: pbo.map_or(0, |x| x.1),
        best: best.map(|i| stats[i].name.clone()),
        deflated_sharpe: best
            .and_then(|i| research::deflated_sharpe(&aligned[i], aligned.len() as u64, var)),
    }
}

// ───────────────────────── 多因子组合回测 ─────────────────────────

#[derive(Clone)]
pub struct PortfolioParams {
    pub weights: Vec<f64>,
    pub prep: Prep,
    pub top_n: usize,
    /// 已持有的股票排名在 top_n × (1 + buffer) 以内时继续持有，减少换手
    pub buffer: f64,
    pub capital: f64,
    pub costs: Costs,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct FactorPortfolioPoint {
    pub date: NaiveDate,
    pub equity: f64,
    pub benchmark: Option<f64>,
    pub positions: usize,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct FactorRebalance {
    pub date: NaiveDate,
    pub buys: usize,
    pub sells: usize,
    /// 本次计划换掉的持仓占目标只数的比例
    pub turnover: f64,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct FactorPick {
    pub thscode: String,
    pub name: String,
    /// 合成得分（标准化后）
    pub score: f64,
    pub rank: usize,
    pub held: bool,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct FactorPortfolioSummary {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub days: usize,
    pub capital: f64,
    pub final_equity: f64,
    pub total_return: f64,
    pub annual_return: Option<f64>,
    pub max_drawdown: f64,
    pub risk: RiskMetrics,
    pub benchmark_name: String,
    pub benchmark_return: Option<f64>,
    pub benchmark_annual: Option<f64>,
    /// 年化收益减基准年化收益
    pub excess_annual: Option<f64>,
    /// 跟踪误差：每日超额收益的年化标准差
    pub tracking_error: Option<f64>,
    /// 信息比率：年化超额 ÷ 跟踪误差
    pub information_ratio: Option<f64>,
    pub fees: f64,
    /// 年化单边换手：每年买入金额 ÷ 平均账户总值
    pub turnover_annual: f64,
    pub rebalances: usize,
    pub avg_positions: f64,
    /// 买不进（停牌、一字涨停）后被下一次调仓取消的买单
    pub skipped_buys: usize,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct FactorPortfolioOutcome {
    pub summary: FactorPortfolioSummary,
    pub equity: Vec<FactorPortfolioPoint>,
    pub rebalances: Vec<FactorRebalance>,
    /// 最近一个取样日按合成得分排出的前 top_n 只
    pub picks: Vec<FactorPick>,
    pub picks_date: Option<NaiveDate>,
    pub thin_days: Vec<NaiveDate>,
}

struct Holding {
    units: f64,
    last: f64,
}

/// 多因子组合：每个取样日收盘按合成得分排名，持有前 top_n 只（带缓冲区），下一个交易日开盘调仓。
/// 卖出遇到停牌、一字跌停顺延；买入遇到停牌、一字涨停顺延，到下一次调仓时取消。前复权价成交，允许零股
pub fn simulate(
    panel: &Panel,
    p: &PortfolioParams,
    names: &HashMap<String, String>,
) -> Option<FactorPortfolioOutcome> {
    let first = panel.start_idx;
    let last = panel.calendar.len() - 1;
    if first > last {
        return None;
    }
    let slip = p.costs.slippage_bps / 10_000.0;
    let stamp = p.costs.stamp_tax_rate;
    let mut cash = p.capital;
    let mut held: HashMap<u32, Holding> = HashMap::new();
    let mut sells: HashSet<u32> = HashSet::new();
    let mut buys: Vec<u32> = vec![];
    let mut fees = 0.0;
    let mut bought = 0.0;
    let mut skipped = 0usize;
    let mut equity: Vec<FactorPortfolioPoint> = vec![];
    let mut rebalances: Vec<FactorRebalance> = vec![];
    let mut picks: Vec<FactorPick> = vec![];
    let mut picks_date = None;
    let mut bench_level = 1.0;
    let mut pos_days = 0usize;
    let eval_at: HashMap<usize, usize> = panel
        .eval_days
        .iter()
        .enumerate()
        .map(|(k, d)| (*d, k))
        .collect();
    for day in first..=last {
        // 1. 开盘：执行上一次调仓留下的订单。先卖后买
        if day > first && !panel.thin[day] {
            let mut done = vec![];
            for &s in &sells {
                let series = &panel.stocks[s as usize];
                let h = &held[&s];
                match series.at(day) {
                    Some(i) if series.bars[i].flags & BLOCK_SELL == 0 => {
                        let amount = h.units * f64::from(series.bars[i].q_open) * (1.0 - slip);
                        let fee = p.costs.commission(amount)
                            + amount * (p.costs.transfer_fee_rate + stamp);
                        cash += amount - fee;
                        fees += fee;
                        done.push(s);
                    }
                    Some(_) => {}
                    // 之后再没有日线：退市或停牌到期末，按最后收盘价了结，不计费用
                    None if series.from(day) >= series.bars.len() => {
                        cash += h.units * h.last;
                        done.push(s);
                    }
                    None => {}
                }
            }
            for s in done {
                sells.remove(&s);
                held.remove(&s);
            }
            let open_value: f64 = cash + held.values().map(|h| h.units * h.last).sum::<f64>();
            let target = open_value / p.top_n as f64;
            let mut waiting = vec![];
            let pending: Vec<u32> = std::mem::take(&mut buys);
            let mut remaining = pending.len();
            for s in pending {
                let series = &panel.stocks[s as usize];
                let slots = p
                    .top_n
                    .saturating_sub(held.len() - sells.len().min(held.len()));
                if slots == 0 {
                    waiting.push(s);
                    remaining -= 1;
                    continue;
                }
                match series.at(day) {
                    Some(i) if series.bars[i].flags & BLOCK_BUY == 0 => {
                        let budget = (cash / remaining as f64).min(target);
                        let amount = p.costs.affordable_amount(budget);
                        let price = f64::from(series.bars[i].q_open) * (1.0 + slip);
                        if amount > 0.0 && price > 0.0 {
                            let fee =
                                p.costs.commission(amount) + amount * p.costs.transfer_fee_rate;
                            cash -= amount + fee;
                            fees += fee;
                            bought += amount;
                            held.insert(
                                s,
                                Holding {
                                    units: amount / price,
                                    last: f64::from(series.bars[i].q_open),
                                },
                            );
                        }
                    }
                    _ => waiting.push(s),
                }
                remaining -= 1;
            }
            buys = waiting;
        }
        // 2. 收盘：按前复权收盘价计算市值；停牌用最近的收盘价
        for (s, h) in held.iter_mut() {
            let series = &panel.stocks[*s as usize];
            if let Some(i) = series.at(day) {
                h.last = f64::from(series.bars[i].q_close);
            }
        }
        let value = cash + held.values().map(|h| h.units * h.last).sum::<f64>();
        if day > first && panel.market[day].is_finite() {
            bench_level *= 1.0 + panel.market[day];
        }
        pos_days += held.len();
        equity.push(FactorPortfolioPoint {
            date: panel.calendar[day],
            equity: value,
            benchmark: Some(bench_level * p.capital),
            positions: held.len(),
        });
        // 3. 取样日收盘：排名、决定下一个交易日的订单
        let Some(&k) = eval_at.get(&day) else {
            continue;
        };
        if panel.thin[day] {
            continue;
        }
        let pr = process(panel, k, p.prep, &p.weights);
        let score = pr.columns.last().expect("合成列");
        let mut ranked: Vec<(u32, f64)> = pr
            .stocks
            .iter()
            .zip(score)
            .filter(|(_, v)| v.is_finite())
            .map(|(s, v)| (*s, *v))
            .collect();
        if ranked.len() < p.top_n {
            continue;
        }
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let rank: HashMap<u32, usize> = ranked
            .iter()
            .enumerate()
            .map(|(r, (s, _))| (*s, r))
            .collect();
        let keep_limit = ((p.top_n as f64) * (1.0 + p.buffer)).ceil() as usize;
        let mut target: Vec<u32> = held
            .keys()
            .copied()
            .filter(|s| rank.get(s).is_some_and(|r| *r < keep_limit))
            .collect();
        target.sort_by_key(|s| rank[s]);
        target.truncate(p.top_n);
        let kept: HashSet<u32> = target.iter().copied().collect();
        for (s, _) in &ranked {
            if target.len() >= p.top_n {
                break;
            }
            if !kept.contains(s) {
                target.push(*s);
            }
        }
        let target_set: HashSet<u32> = target.iter().copied().collect();
        skipped += buys.len();
        sells = held
            .keys()
            .copied()
            .filter(|s| !target_set.contains(s))
            .collect();
        buys = target
            .iter()
            .copied()
            .filter(|s| !held.contains_key(s))
            .collect();
        if day < last {
            rebalances.push(FactorRebalance {
                date: panel.calendar[day],
                buys: buys.len(),
                sells: sells.len(),
                turnover: buys.len() as f64 / p.top_n as f64,
            });
        }
        picks_date = Some(panel.calendar[day]);
        picks = ranked
            .iter()
            .take(p.top_n)
            .enumerate()
            .map(|(r, (s, v))| {
                let code = &panel.stocks[*s as usize].code;
                FactorPick {
                    thscode: code.clone(),
                    name: names.get(code).cloned().unwrap_or_default(),
                    score: *v,
                    rank: r + 1,
                    held: held.contains_key(s),
                }
            })
            .collect();
    }
    let start = panel.calendar[first];
    let end = panel.calendar[last];
    let final_equity = equity.last().map_or(p.capital, |e| e.equity);
    let total_return = final_equity / p.capital - 1.0;
    let cal_days = (end - start).num_days();
    let ann =
        |total: f64| (cal_days >= 90).then(|| (1.0 + total).powf(365.25 / cal_days as f64) - 1.0);
    let annual_return = ann(total_return);
    let mut peak = f64::MIN;
    let mut max_dd = 0.0f64;
    for e in &equity {
        peak = peak.max(e.equity);
        max_dd = max_dd.max(1.0 - e.equity / peak);
    }
    let values: Vec<f64> = equity.iter().map(|e| e.equity).collect();
    let risk = research::risk_metrics(&values, annual_return, max_dd);
    let benchmark_return = Some(bench_level - 1.0);
    let benchmark_annual = ann(bench_level - 1.0);
    let active: Vec<f64> = equity
        .windows(2)
        .filter_map(|w| {
            let (a, b) = (w[0].benchmark?, w[1].benchmark?);
            Some((w[1].equity / w[0].equity - 1.0) - (b / a - 1.0))
        })
        .filter(|v| v.is_finite())
        .collect();
    let tracking_error = mean_std(&active).map(|(_, sd)| sd * 252.0_f64.sqrt());
    let excess_annual = annual_return.zip(benchmark_annual).map(|(a, b)| a - b);
    let information_ratio =
        mean_std(&active).and_then(|(m, sd)| (sd > 0.0).then(|| m / sd * 252.0_f64.sqrt()));
    let avg_equity = values.iter().sum::<f64>() / values.len().max(1) as f64;
    let years = equity.len() as f64 / 252.0;
    Some(FactorPortfolioOutcome {
        summary: FactorPortfolioSummary {
            start,
            end,
            days: equity.len(),
            capital: p.capital,
            final_equity,
            total_return,
            annual_return,
            max_drawdown: max_dd,
            risk,
            benchmark_name: EQUAL_WEIGHT_NAME.to_string(),
            benchmark_return,
            benchmark_annual,
            excess_annual,
            tracking_error,
            information_ratio,
            fees,
            turnover_annual: if avg_equity > 0.0 && years > 0.0 {
                bought / avg_equity / years
            } else {
                0.0
            },
            rebalances: rebalances.len(),
            avg_positions: pos_days as f64 / equity.len().max(1) as f64,
            skipped_buys: skipped,
        },
        equity,
        rebalances,
        picks,
        picks_date,
        thin_days: (first..=last)
            .filter(|d| panel.thin[*d])
            .map(|d| panel.calendar[d])
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(i: usize) -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 1, 1).unwrap() + chrono::Duration::days(i as i64)
    }

    /// 合成面板：n 只股票、t 个交易日。第 s 只股票每天涨 drift(s)，因子值等于 drift(s) 加噪声，
    /// 所以「越大越好」的因子应当有很高的 RankIC
    fn synthetic(n: usize, t: usize, step: usize, factor: FactorId, sign: f64) -> Panel {
        let calendar: Vec<NaiveDate> = (0..t).map(day).collect();
        let drift = |s: usize| (s as f64 - n as f64 / 2.0) * 0.0002;
        let stocks: Vec<Series> = (0..n)
            .map(|s| {
                let mut price = 10.0f64;
                Series {
                    code: format!("{s:06}.SZ"),
                    industry: Some((s % 5) as u16),
                    bars: (0..t)
                        .map(|d| {
                            let open = price;
                            price *= 1.0 + drift(s) + 0.01 * ((s * 7 + d * 3) as f64).sin();
                            CBar {
                                day: d as u16,
                                flags: IN_UNIVERSE,
                                q_open: open as f32,
                                q_close: price as f32,
                            }
                        })
                        .collect(),
                }
            })
            .collect();
        let eval_days: Vec<usize> = (0..t).step_by(step).collect();
        let evals: Vec<EvalDay> = eval_days
            .iter()
            .map(|d| EvalDay {
                stocks: (0..n as u32).collect(),
                size: (0..n).map(|s| 10.0 + (s % 3) as f64).collect(),
                values: vec![
                    (0..n)
                        .map(|s| sign * (drift(s) + 0.0001 * ((s + d) as f64).cos()))
                        .collect(),
                ],
            })
            .collect();
        finish(
            calendar,
            0,
            vec![factor],
            stocks,
            eval_days,
            evals,
            &vec![n as u32; t],
        )
    }

    fn prep() -> Prep {
        Prep {
            neutralize: false,
            exclude_small_pct: 0.0,
        }
    }

    #[test]
    fn coverage_counts_missing_days_as_incomplete() {
        // 合成 4 个抽样日、每天 10 只股票：应该是满覆盖
        let panel = synthetic(10, 6, 1, FactorId::Reversal, 1.0);
        let mut per_day: Vec<u32> = (panel.start_idx..panel.calendar.len())
            .filter(|d| !panel.thin[*d])
            .map(|d| {
                panel
                    .stocks
                    .iter()
                    .filter(|s| s.bars.binary_search_by_key(&(d as u16), |b| b.day).is_ok())
                    .count() as u32
            })
            .collect();
        per_day.sort_unstable();
        let observed = per_day.get(per_day.len() / 2).copied().unwrap_or(0);
        assert_eq!(panel.expected, observed, "满覆盖时期望值和实测值应该一致");
    }

    #[test]
    fn coverage_ratio_flags_partial_backfill() {
        let c = Coverage {
            expected: 5000,
            observed: 2500,
            ratio: 0.5,
            eval_days: 100,
            skipped_days: 0,
        };
        let w = c.warning().expect("一半的数据应该报警");
        assert!(w.contains("50%"), "提示里要写出实际比例：{w}");

        let ok = Coverage {
            expected: 5000,
            observed: 4800,
            ratio: 0.96,
            eval_days: 100,
            skipped_days: 0,
        };
        assert!(ok.warning().is_none(), "覆盖 96% 不该报警");
    }

    #[test]
    fn ranks_handle_ties() {
        assert_eq!(ranks(&[3.0, 1.0, 3.0, 2.0]), vec![2.5, 0.0, 2.5, 1.0]);
        assert!((spearman(&[1.0, 2.0, 3.0], &[10.0, 20.0, 30.0]) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn standardize_clips_outliers_and_neutralizes() {
        let mut v: Vec<f64> = (0..100).map(|i| i as f64).collect();
        v[0] = -1e6;
        let z = standardize(&v, None, &vec![None; 100], false);
        // 不截尾时 z 约为 -10；截尾到中位数减 3 倍稳健标准差后约为 -3.6
        assert!(z[0] > -5.0, "极值应被截尾：{}", z[0]);
        // 因子完全由行业决定时，行业中性化之后没有剩余信息
        let ind: Vec<Option<u16>> = (0..100).map(|i| Some((i % 4) as u16)).collect();
        let by_industry: Vec<f64> = (0..100).map(|i| (i % 4) as f64).collect();
        let size: Vec<f64> = (0..100).map(|i| (i * 7 % 11) as f64).collect();
        let n = standardize(&by_industry, Some(&size), &ind, true);
        assert!(n.iter().all(|v| v.abs() < 1e-9 || v.is_nan()), "{n:?}");
        // 因子就是规模变量本身：对规模中性化后没有信息，不能把浮点误差放大成排序；只做行业中性化时保留
        let self_neutral = standardize(&size, Some(&size), &ind, true);
        assert!(self_neutral.iter().all(|v| v.is_nan()), "{self_neutral:?}");
        let industry_only = standardize(&size, None, &ind, true);
        assert!(industry_only.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn evaluate_finds_planted_signal_in_expected_direction() {
        // 反转因子「越大越好」：值与漂移同向时 RankIC 应显著为正
        let panel = synthetic(120, 200, 5, FactorId::Reversal, 1.0);
        let out = evaluate(
            &panel,
            &TestParams {
                horizon: 5,
                quantiles: 5,
                prep: prep(),
            },
        )
        .unwrap();
        let s = &out.factors[0];
        assert!(s.rank_ic_mean.unwrap() > 0.2, "{:?}", s.rank_ic_mean);
        assert!(s.significant);
        assert!(s.long_short_annual.unwrap() > 0.0);
        assert!(s.monotonicity.unwrap() > 0.9);
        // 动量因子「越小越好」：同样的值会被反向，RankIC 为负
        let panel = synthetic(120, 200, 5, FactorId::Momentum, 1.0);
        let out = evaluate(
            &panel,
            &TestParams {
                horizon: 5,
                quantiles: 5,
                prep: prep(),
            },
        )
        .unwrap();
        assert!(out.factors[0].rank_ic_mean.unwrap() < -0.2);
        assert!(!out.factors[0].consistent);
    }

    #[test]
    fn forward_return_skips_blocked_entry_and_defers_blocked_exit() {
        let mut panel = synthetic(60, 40, 5, FactorId::Reversal, 1.0);
        panel.stocks[0].bars[1].flags |= BLOCK_BUY;
        assert!(forward_return(&panel, &panel.stocks[0], 0, 5).is_nan());
        panel.stocks[1].bars[6].flags |= BLOCK_SELL;
        let s = &panel.stocks[1];
        let expected = f64::from(s.bars[7].q_open) / f64::from(s.bars[1].q_open) - 1.0;
        assert!((forward_return(&panel, s, 0, 5) - expected).abs() < 1e-12);
        // 持有期超出数据范围
        assert!(forward_return(&panel, s, 36, 5).is_nan());
    }

    #[test]
    fn simulate_holds_top_names_and_beats_benchmark_on_planted_signal() {
        let panel = synthetic(100, 120, 10, FactorId::Reversal, 1.0);
        let p = PortfolioParams {
            weights: vec![1.0],
            prep: prep(),
            top_n: 10,
            buffer: 0.5,
            capital: 1_000_000.0,
            costs: Costs::default(),
        };
        let out = simulate(&panel, &p, &HashMap::new()).unwrap();
        let s = &out.summary;
        assert!(s.total_return > s.benchmark_return.unwrap(), "{s:?}");
        assert!(s.avg_positions > 8.0);
        assert_eq!(out.picks.len(), 10);
        // 排名第一的是漂移最大的股票
        assert_eq!(out.picks[0].thscode, "000099.SZ");
        // 信号稳定时缓冲区让后续调仓几乎不换股
        assert!(
            out.rebalances.iter().skip(1).all(|r| r.turnover <= 0.2),
            "{:?}",
            out.rebalances
        );
        assert!(s.fees > 0.0);
    }

    #[test]
    fn simulate_defers_blocked_orders_and_liquidates_ended_series() {
        let mut panel = synthetic(60, 60, 10, FactorId::Reversal, 1.0);
        // 第一名在第 1 天一字涨停，买单顺延到第 2 天
        let top = 59usize;
        panel.stocks[top].bars[1].flags |= BLOCK_BUY;
        // 另一只在第 25 天之后没有日线：持有时按最后收盘价了结
        panel.stocks[58].bars.truncate(25);
        let p = PortfolioParams {
            weights: vec![1.0],
            prep: prep(),
            top_n: 5,
            buffer: 0.0,
            capital: 100_000.0,
            costs: Costs::default(),
        };
        let out = simulate(&panel, &p, &HashMap::new()).unwrap();
        assert_eq!(out.equity[1].positions, 4);
        assert_eq!(out.equity[2].positions, 5);
        assert!(
            out.equity
                .iter()
                .all(|e| e.equity.is_finite() && e.equity > 0.0)
        );
    }
}
