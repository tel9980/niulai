//! 研究的统计工具（风险指标、Deflated Sharpe、PBO）和可复现性元数据。

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, Default)]
pub struct RiskMetrics {
    /// 年化波动率。
    pub annual_volatility: Option<f64>,
    /// 无风险利率为 0 的年化 Sharpe。
    pub sharpe: Option<f64>,
    /// 只使用负收益的年化 Sortino。
    pub sortino: Option<f64>,
    /// 年化收益 / 最大回撤。
    pub calmar: Option<f64>,
    /// 每日平均收益。
    pub average_daily_return: Option<f64>,
}

/// 一次研究运行的可复现信息，写入 `ops.backtest_run`
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct RunManifest {
    /// 配置的哈希：同一组配置算一次试验
    pub definition_hash: String,
    pub data_start: NaiveDate,
    pub data_end: NaiveDate,
    /// cross_sectional_factor_test / factor_portfolio
    pub kind: String,
    /// 这类运行此前试过多少组不同配置（含这一次），用于 Deflated Sharpe
    pub trial_count: u64,
    /// 派生数据（复权因子、涨跌停池）的更新时间
    pub derived_versions: std::collections::BTreeMap<String, String>,
    /// 区间内有日线的股票数和日线根数
    pub instruments: u64,
    pub observed_bars: u64,
    /// 已知偏差：幸存者偏差、当前行业、财报按法定截止日
    pub bias: String,
    /// 数据覆盖情况。只有因子检验能算出来，组合回测不填
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<crate::analysis::factor_lab::Coverage>,
}

pub fn definition_hash<T: Serialize>(value: &T) -> String {
    let json = serde_json::to_vec(value).unwrap_or_default();
    Sha256::digest(json)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn risk_metrics(values: &[f64], annual_return: Option<f64>, max_drawdown: f64) -> RiskMetrics {
    if values.len() < 2 {
        return RiskMetrics::default();
    }
    let returns: Vec<f64> = values
        .windows(2)
        .filter(|w| w[0] > 0.0)
        .map(|w| w[1] / w[0] - 1.0)
        .filter(|v| v.is_finite())
        .collect();
    if returns.len() < 2 {
        return RiskMetrics::default();
    }
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let variance =
        returns.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (returns.len() - 1) as f64;
    let vol = variance.sqrt();
    let downside = returns
        .iter()
        .filter(|v| **v < 0.0)
        .map(|v| v.powi(2))
        .sum::<f64>()
        / returns.len() as f64;
    let downside_dev = downside.sqrt();
    let annual_volatility = (vol.is_finite() && vol > 0.0).then_some(vol * 252.0_f64.sqrt());
    let sharpe = annual_volatility
        .map(|_| mean / vol * 252.0_f64.sqrt())
        .filter(|v| v.is_finite());
    let sortino = (downside_dev > 0.0)
        .then_some(mean / downside_dev * 252.0_f64.sqrt())
        .filter(|v| v.is_finite());
    let calmar = annual_return
        .filter(|_| max_drawdown > 0.0)
        .map(|v| v / max_drawdown)
        .filter(|v| v.is_finite());
    RiskMetrics {
        annual_volatility,
        sharpe,
        sortino,
        calmar,
        average_daily_return: Some(mean),
    }
}

/// 标准正态分布函数（Abramowitz–Stegun 7.1.26 近似 erf，误差约 1e-7）
pub fn norm_cdf(x: f64) -> f64 {
    let z = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * z);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - poly * (-z * z).exp();
    if x >= 0.0 {
        0.5 * (1.0 + erf)
    } else {
        0.5 * (1.0 - erf)
    }
}

/// 标准正态分布的分位数（Acklam 近似，相对误差约 1e-9）
pub fn norm_inv(p: f64) -> f64 {
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    let tail = |q: f64| {
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    };
    if p < 0.024_25 {
        tail((-2.0 * p.ln()).sqrt())
    } else if p > 1.0 - 0.024_25 {
        -tail((-2.0 * (1.0 - p).ln()).sqrt())
    } else {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    }
}

/// 一组收益的均值、样本标准差、偏度和峰度（非超额峰度，正态为 3）
pub fn moments(r: &[f64]) -> Option<(f64, f64, f64, f64)> {
    let r: Vec<f64> = r.iter().copied().filter(|v| v.is_finite()).collect();
    if r.len() < 3 {
        return None;
    }
    let n = r.len() as f64;
    let mean = r.iter().sum::<f64>() / n;
    let m2 = r.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    if m2 <= 0.0 {
        return None;
    }
    let m3 = r.iter().map(|v| (v - mean).powi(3)).sum::<f64>() / n;
    let m4 = r.iter().map(|v| (v - mean).powi(4)).sum::<f64>() / n;
    let sd = (m2 * n / (n - 1.0)).sqrt();
    Some((mean, sd, m3 / m2.powf(1.5), m4 / (m2 * m2)))
}

/// Deflated Sharpe Ratio（Bailey 与 López de Prado，2014）：在试过 `trials` 组参数、挑出表现最好的一组之后，
/// 这组的真实 Sharpe 大于 0 的概率。`returns` 是被选中那组的每期收益；`trial_sharpe_variance` 是各组每期 Sharpe 的方差，
/// 只有一组时用单个 Sharpe 估计误差的方差近似。返回 0 到 1，通常要求高于 0.95
pub fn deflated_sharpe(
    returns: &[f64],
    trials: u64,
    trial_sharpe_variance: Option<f64>,
) -> Option<f64> {
    let (mean, sd, skew, kurt) = moments(returns)?;
    let t = returns.iter().filter(|v| v.is_finite()).count() as f64;
    let sr = mean / sd;
    let n = trials.max(1) as f64;
    let var = trial_sharpe_variance
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or((1.0 + 0.5 * sr * sr) / t);
    // 试 n 次时最大 Sharpe 的期望值（欧拉常数 γ ≈ 0.5772）
    let sr0 = if n <= 1.0 {
        0.0
    } else {
        const GAMMA: f64 = std::f64::consts::EULER_GAMMA;
        var.sqrt()
            * ((1.0 - GAMMA) * norm_inv(1.0 - 1.0 / n)
                + GAMMA * norm_inv(1.0 - 1.0 / (n * std::f64::consts::E)))
    };
    let denom = 1.0 - skew * sr + (kurt - 1.0) / 4.0 * sr * sr;
    if denom <= 0.0 {
        return None;
    }
    let z = (sr - sr0) * (t - 1.0).sqrt() / denom.sqrt();
    Some(norm_cdf(z)).filter(|v| v.is_finite())
}

/// 回测过拟合概率 PBO（Bailey、Borwein、López de Prado、Zhu，2015）的 CSCV 估计。
/// `series` 是 N 组候选（参数或因子）同一组时间点上的每期收益。把时间切成 S 段，
/// 枚举一半做样本内、一半做样本外：样本内 Sharpe 最高的候选在样本外排到后一半的比例就是 PBO。
/// 候选少于 2 组或期数不足 2S 时返回空
pub fn pbo_cscv(series: &[Vec<f64>], blocks: usize) -> Option<(f64, usize)> {
    let n = series.len();
    let t = series.first()?.len();
    if n < 2
        || blocks < 2
        || !blocks.is_multiple_of(2)
        || t < blocks * 2
        || series.iter().any(|s| s.len() != t)
    {
        return None;
    }
    let size = t / blocks;
    let sharpe = |s: &[f64], picked: &[usize]| -> f64 {
        let v: Vec<f64> = picked
            .iter()
            .flat_map(|b| s[b * size..(b + 1) * size].iter().copied())
            .filter(|x| x.is_finite())
            .collect();
        match moments(&v) {
            Some((m, sd, _, _)) if sd > 0.0 => m / sd,
            _ => f64::NAN,
        }
    };
    let mut overfit = 0usize;
    let mut total = 0usize;
    // 枚举 blocks 选 blocks/2 的全部组合（blocks 最多 16，组合数 12870）
    for mask in 0u32..(1 << blocks) {
        if mask.count_ones() as usize != blocks / 2 {
            continue;
        }
        let is: Vec<usize> = (0..blocks).filter(|b| mask & (1 << b) != 0).collect();
        let oos: Vec<usize> = (0..blocks).filter(|b| mask & (1 << b) == 0).collect();
        let is_sr: Vec<f64> = series.iter().map(|s| sharpe(s, &is)).collect();
        let Some(best) = (0..n)
            .filter(|i| is_sr[*i].is_finite())
            .max_by(|a, b| is_sr[*a].total_cmp(&is_sr[*b]))
        else {
            continue;
        };
        let oos_sr: Vec<f64> = series.iter().map(|s| sharpe(s, &oos)).collect();
        if !oos_sr[best].is_finite() {
            continue;
        }
        // 样本外相对排名 ω ∈ (0, 1)；logit(ω) ≤ 0 即排在后一半
        let below = oos_sr
            .iter()
            .filter(|v| v.is_finite() && **v < oos_sr[best])
            .count();
        let omega = (below as f64 + 1.0) / (n as f64 + 1.0);
        total += 1;
        if omega <= 0.5 {
            overfit += 1;
        }
    }
    (total > 0).then(|| (overfit as f64 / total as f64, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_distribution_helpers() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-7);
        assert!((norm_cdf(1.96) - 0.975).abs() < 1e-4);
        assert!((norm_inv(0.975) - 1.959_964).abs() < 1e-5);
        assert!((norm_cdf(norm_inv(0.1)) - 0.1).abs() < 1e-6);
    }

    #[test]
    fn deflated_sharpe_penalizes_many_trials() {
        let r: Vec<f64> = (0..500)
            .map(|i| 0.001 + 0.01 * ((i as f64) * 1.7).sin())
            .collect();
        let one = deflated_sharpe(&r, 1, None).unwrap();
        let many = deflated_sharpe(&r, 200, Some(0.01)).unwrap();
        assert!(one > 0.9, "{one}");
        assert!(many < one);
    }

    #[test]
    fn pbo_detects_noise_and_skill() {
        // 一组有稳定正收益，其余是噪声：被选中的总是它，样本外也最好，PBO 接近 0
        let skill: Vec<f64> = (0..160)
            .map(|i| 0.01 + 0.002 * ((i as f64) * 2.3).sin())
            .collect();
        let noise =
            |k: f64| -> Vec<f64> { (0..160).map(|i| 0.01 * ((i as f64) * k).sin()).collect() };
        let (p, combos) = pbo_cscv(&[skill, noise(1.1), noise(2.9), noise(0.7)], 8).unwrap();
        assert_eq!(combos, 70);
        assert!(p < 0.2, "{p}");
        assert!(pbo_cscv(&[noise(1.0)], 8).is_none());
    }

    #[test]
    fn risk_metrics_use_daily_returns() {
        let r = risk_metrics(&[100.0, 110.0, 99.0, 108.9], Some(0.1), 0.1);
        assert!(r.sharpe.is_some());
        assert!(r.sortino.is_some());
        assert_eq!(r.calmar, Some(1.0));
    }
}
