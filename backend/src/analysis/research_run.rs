//! 一次因子研究运行：类型化的配置、执行、运行记录（`ops.backtest_run`）和试验次数。
//!
//! - **方法定义**是 [`FactorConfig`]：决定结果的全部参数（因子和权重、预处理、股票池、持有期或调仓间隔、
//!   分组数、持有只数、缓冲区、本金和成本），不含日期区间。配置哈希由它派生，同一个定义算一次试验，
//!   完整配置随运行记录保存，离开当次响应也能还原。
//! - **一次具体运行**是定义加上日期区间和当时的数据版本，写在运行清单 [`RunManifest`] 里。
//! - 从读入输入到算完、记下结果，整个用例占一个全市场计算的槽位（[`compute::run`]）。
//!
//! HTTP 层只负责把查询参数转换成 [`FactorConfig`]。规则见 docs/domains/factor_research.md。

use std::collections::HashMap;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;

use super::compute::{self, Budget};
use super::costs::Costs;
use super::factor_lab::{
    self, Coverage, FactorPortfolioOutcome, FactorTestOutcome, PortfolioParams, Prep, Spec,
    TestParams,
};
use super::factors::FactorId;
use super::research::{self, RunManifest};
use super::securities;
use super::universe::Universe;

/// 已知偏差：没有采集之前的退市股、行业用当前归属、财报按法定截止日可见
const BIAS: &str = "survivorship_biased:no_history_before_collection;current_industry;fundamentals_at_statutory_deadline";

/// 研究的种类，存在 `ops.backtest_run.mode`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    FactorTest,
    FactorPortfolio,
}

impl Mode {
    pub const fn key(self) -> &'static str {
        match self {
            Self::FactorTest => "factor_test",
            Self::FactorPortfolio => "factor_portfolio",
        }
    }

    const fn manifest_kind(self) -> &'static str {
        match self {
            Self::FactorTest => "cross_sectional_factor_test",
            Self::FactorPortfolio => "factor_portfolio",
        }
    }
}

/// 方法定义：决定结果的全部参数，不含日期区间。写入运行记录并计算哈希
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct FactorConfig {
    pub factors: Vec<FactorId>,
    /// 权重，按因子的预期方向合成；因子检验的合成全部为 1
    pub weights: Vec<f64>,
    pub neutralize: bool,
    pub exclude_small_pct: f64,
    pub universe: Universe,
    /// 股票池的白话描述
    pub universe_text: Vec<String>,
    /// 检验的持有期或组合的调仓间隔（交易日）
    pub horizon: usize,
    /// 因子检验分几组；组合回测为空
    pub quantiles: Option<usize>,
    /// 组合持有只数；因子检验为空
    pub top_n: Option<usize>,
    pub buffer: Option<f64>,
    /// 组合回测的本金（元）；因子检验为空
    pub capital: Option<f64>,
    /// 组合回测的交易成本；因子检验为空
    pub costs: Option<Costs>,
}

impl FactorConfig {
    pub fn prep(&self) -> Prep {
        Prep {
            neutralize: self.neutralize,
            exclude_small_pct: self.exclude_small_pct,
        }
    }

    /// 同一个方法定义的哈希相同，算一次试验
    pub fn definition_hash(&self) -> String {
        research::definition_hash(self)
    }
}

/// 运行没能给出结果的原因
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// 区间内没有足够的数据，参数本身没问题
    #[error("{0}")]
    NoData(&'static str),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for RunError {
    fn from(e: sqlx::Error) -> Self {
        RunError::Internal(e.into())
    }
}

// @project-doc docs/domains/factor_research.md#test
/// 因子检验：按持有期取样，统计 RankIC、分组收益、多空收益、换手、衰减和挑选偏差，并记下运行
pub async fn factor_test(
    pool: &PgPool,
    config: &FactorConfig,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<(RunManifest, FactorTestOutcome), RunError> {
    compute::run(Budget::Foreground, async {
        let panel = factor_lab::load(pool, &spec(config, start, end))
            .await?
            .ok_or(RunError::NoData("所选区间内没有日线"))?;
        let params = TestParams {
            horizon: config.horizon,
            quantiles: config.quantiles.unwrap_or(5),
            prep: config.prep(),
        };
        let outcome = compute::run_cpu(|| factor_lab::evaluate(&panel, &params))
            .ok_or(RunError::NoData("区间太短，没有完整的持有期"))?;
        drop(panel);
        let hash = config.definition_hash();
        let (trials, _) = trials(pool, Mode::FactorTest, &hash, None).await?;
        let manifest = manifest(
            pool,
            Mode::FactorTest,
            &hash,
            outcome.start,
            outcome.end,
            trials,
            Some(outcome.coverage.clone()),
        )
        .await?;
        #[derive(Serialize)]
        struct Brief<'a> {
            factors: Vec<(&'a str, Option<f64>, Option<f64>)>,
            pbo: Option<f64>,
        }
        let brief = Brief {
            factors: outcome
                .factors
                .iter()
                .map(|f| (f.name.as_str(), f.rank_ic_mean, f.rank_ic_t))
                .collect(),
            pbo: outcome.overfit.pbo,
        };
        record(pool, Mode::FactorTest, &manifest, config, &brief).await?;
        Ok((manifest, outcome))
    })
    .await
}

// @project-doc docs/domains/factor_research.md#portfolio
/// 多因子组合回测：每隔 horizon 个交易日按合成得分持有前 top_n 只，下一个交易日开盘调仓，并记下运行。
/// 返回运行清单、Deflated Sharpe（考虑这类回测此前试过的配置数之后，真实 Sharpe 大于 0 的概率）和结果
pub async fn factor_portfolio(
    pool: &PgPool,
    config: &FactorConfig,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<(RunManifest, Option<f64>, FactorPortfolioOutcome), RunError> {
    compute::run(Budget::Foreground, async {
        let names: HashMap<String, String> = securities::load(pool).await?.names;
        let panel = factor_lab::load(pool, &spec(config, start, end))
            .await?
            .ok_or(RunError::NoData("所选区间内没有日线"))?;
        let params = PortfolioParams {
            weights: config.weights.clone(),
            prep: config.prep(),
            top_n: config.top_n.unwrap_or(30),
            buffer: config.buffer.unwrap_or(0.5),
            capital: config.capital.unwrap_or(1_000_000.0),
            costs: config.costs.unwrap_or_default(),
        };
        let outcome = compute::run_cpu(|| factor_lab::simulate(&panel, &params, &names))
            .ok_or(RunError::NoData("所选区间内没有日线"))?;
        drop(panel);
        let hash = config.definition_hash();
        let (trials, others) =
            trials(pool, Mode::FactorPortfolio, &hash, Some("risk.sharpe")).await?;
        // 运行记录里是年化 Sharpe，换成每日 Sharpe 再算各组之间的方差
        let mut all: Vec<f64> = others.iter().map(|v| v / 252.0_f64.sqrt()).collect();
        if let Some(sr) = outcome.summary.risk.sharpe {
            all.push(sr / 252.0_f64.sqrt());
        }
        let var = (all.len() >= 2).then(|| {
            let m = all.iter().sum::<f64>() / all.len() as f64;
            all.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (all.len() - 1) as f64
        });
        let returns: Vec<f64> = outcome
            .equity
            .windows(2)
            .map(|w| w[1].equity / w[0].equity - 1.0)
            .collect();
        let dsr = research::deflated_sharpe(&returns, trials, var);
        let manifest = manifest(
            pool,
            Mode::FactorPortfolio,
            &hash,
            outcome.summary.start,
            outcome.summary.end,
            trials,
            // 组合回测的取样口径不同，暂不统计覆盖度
            None,
        )
        .await?;
        record(
            pool,
            Mode::FactorPortfolio,
            &manifest,
            config,
            &outcome.summary,
        )
        .await?;
        Ok((manifest, dsr, outcome))
    })
    .await
}

fn spec(config: &FactorConfig, start: NaiveDate, end: NaiveDate) -> Spec {
    Spec {
        start,
        end,
        factors: config.factors.clone(),
        universe: config.universe.clone(),
        step: config.horizon,
    }
}

/// 同一种研究此前试过多少组不同的方法定义（按配置哈希去重，含这一次），以及其他各组最近一次的结果指标
async fn trials(
    pool: &PgPool,
    mode: Mode,
    hash: &str,
    metric_path: Option<&str>,
) -> sqlx::Result<(u64, Vec<f64>)> {
    let rows: Vec<(String, Option<f64>)> = sqlx::query_as(
        "SELECT DISTINCT ON (definition_hash) definition_hash, (summary #>> $2::text[])::float8
         FROM ops.backtest_run WHERE mode = $1 ORDER BY definition_hash, created_at DESC",
    )
    .bind(mode.key())
    .bind(metric_path.map(|p| p.split('.').collect::<Vec<_>>()))
    .fetch_all(pool)
    .await?;
    let seen = rows.iter().any(|(h, _)| h == hash);
    let n = rows.len() as u64 + u64::from(!seen);
    Ok((
        n,
        rows.into_iter()
            .filter_map(|(h, v)| (h != hash).then_some(v).flatten())
            .collect(),
    ))
}

/// 运行清单：配置哈希、试验次数、派生数据版本、覆盖范围和已知偏差
async fn manifest(
    pool: &PgPool,
    mode: Mode,
    hash: &str,
    start: NaiveDate,
    end: NaiveDate,
    trials: u64,
    coverage: Option<Coverage>,
) -> sqlx::Result<RunManifest> {
    let versions: Vec<(String, String)> =
        sqlx::query_as("SELECT name, updated_at::text FROM ops.derived_version ORDER BY name")
            .fetch_all(pool)
            .await?;
    let (instruments, observed_bars): (i64, i64) = sqlx::query_as(
        "SELECT count(DISTINCT thscode), count(*) FROM mkt.stock_bar_1d WHERE trade_date BETWEEN $1 AND $2",
    )
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await?;
    Ok(RunManifest {
        definition_hash: hash.to_string(),
        data_start: start,
        data_end: end,
        kind: mode.manifest_kind().into(),
        trial_count: trials,
        derived_versions: versions.into_iter().collect(),
        instruments: instruments.max(0) as u64,
        observed_bars: observed_bars.max(0) as u64,
        bias: BIAS.into(),
        coverage,
    })
}

/// 写入一次运行：方法定义的完整配置、运行清单和结果摘要
async fn record(
    pool: &PgPool,
    mode: Mode,
    manifest: &RunManifest,
    config: &FactorConfig,
    summary: &impl Serialize,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO ops.backtest_run (mode, start_date, end_date, definition_hash, config, manifest, summary)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(mode.key())
    .bind(manifest.data_start)
    .bind(manifest.data_end)
    .bind(&manifest.definition_hash)
    .bind(serde_json::to_value(config)?)
    .bind(serde_json::to_value(manifest)?)
    .bind(serde_json::to_value(summary)?)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> FactorConfig {
        FactorConfig {
            factors: vec![FactorId::Reversal],
            weights: vec![1.0],
            neutralize: true,
            exclude_small_pct: 0.0,
            universe: Universe::default(),
            universe_text: Universe::default().describe(),
            horizon: 5,
            quantiles: Some(5),
            top_n: None,
            buffer: None,
            capital: None,
            costs: None,
        }
    }

    #[test]
    fn every_result_affecting_parameter_changes_the_hash() {
        let base = config();
        let mut quantiles = config();
        quantiles.quantiles = Some(10);
        let mut costs = config();
        costs.costs = Some(Costs::default());
        let mut capital = config();
        capital.capital = Some(1e6);
        for other in [quantiles, costs, capital] {
            assert_ne!(base.definition_hash(), other.definition_hash());
        }
        assert_eq!(base.definition_hash(), config().definition_hash());
        let back: FactorConfig =
            serde_json::from_value(serde_json::to_value(&base).unwrap()).unwrap();
        assert_eq!(
            back.definition_hash(),
            base.definition_hash(),
            "保存的配置能还原出同一个定义"
        );
    }
}
