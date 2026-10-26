//! 数据集注册表（config/datasets.yaml）的类型化表示。
//!
//! YAML 先解析为 `serde_json::Value`，再逐项转换为强类型配置。转换过程中做校验：
//! 未知的分区类型、分页方式、调度类型都会在启动时报错，不会等到执行时才发现。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use chrono::{Duration, Months, NaiveDate};
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub struct Registry {
    pub datasets: Vec<DatasetSpec>,
    pub max_attempts: i16,
    pub backoff_s: Vec<u64>,
}

impl Registry {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("读取 {}", path.display()))?;
        Self::parse(&text)
    }

    // @project-doc docs/interfaces/dataset_registry.md#fields
    pub fn parse(yaml: &str) -> anyhow::Result<Self> {
        let root: Value = serde_saphyr::from_str(yaml).context("datasets.yaml 不是合法的 YAML")?;
        let defaults = root
            .get("defaults")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let retry = defaults.get("retry").cloned().unwrap_or(Value::Null);
        let max_attempts = retry
            .get("max_attempts")
            .and_then(Value::as_i64)
            .unwrap_or(4) as i16;
        let backoff_s = retry
            .get("backoff_s")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_else(|| vec![2, 8, 30]);
        let items = root
            .get("datasets")
            .and_then(Value::as_array)
            .context("datasets.yaml 缺少 datasets 列表")?;
        let mut datasets = Vec::with_capacity(items.len());
        for item in items {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("<缺少 id>")
                .to_string();
            datasets.push(
                DatasetSpec::from_value(item, &defaults)
                    .with_context(|| format!("数据集 {id} 配置有误"))?,
            );
        }
        let mut seen = std::collections::HashSet::new();
        for d in &datasets {
            if !seen.insert(d.id.as_str()) {
                bail!("数据集 ID 重复：{}", d.id);
            }
        }
        for d in &datasets {
            if let Some(r) = &d.recover_with
                && !seen.contains(r.as_str())
            {
                bail!("数据集 {} 的 recover_with 指向不存在的数据集：{r}", d.id);
            }
            if let Partition::FromDataset { dataset, .. } = &d.partition
                && !seen.contains(dataset.as_str())
            {
                bail!(
                    "数据集 {} 的 from_dataset 指向不存在的数据集：{dataset}",
                    d.id
                );
            }
            if d.sink.iter().any(|s| !KNOWN_SINKS.contains(&s.as_str())) {
                let bad = d
                    .sink
                    .iter()
                    .find(|s| !KNOWN_SINKS.contains(&s.as_str()))
                    .unwrap();
                bail!("数据集 {} 的 sink 不受支持：{bad}", d.id);
            }
            if matches!(&d.records, Records::Paths(paths) if paths.is_empty()) {
                bail!("数据集 {} 的 records 不能为空", d.id);
            }
        }
        for d in &datasets {
            let mut path = std::collections::HashSet::new();
            validate_dependency_cycle(d, &datasets, &mut path)?;
        }
        Ok(Registry {
            datasets,
            max_attempts,
            backoff_s,
        })
    }

    pub fn get(&self, id: &str) -> Option<&DatasetSpec> {
        self.datasets.iter().find(|d| d.id == id)
    }

    /// 写入任一指定类型化表的数据集。数据集在流程里扮演什么角色由它的 sink 决定，代码里不写死数据集 ID
    pub fn writing_to(&self, tables: &[&str]) -> Vec<&DatasetSpec> {
        self.datasets
            .iter()
            .filter(|d| d.sink.iter().any(|s| tables.contains(&s.as_str())))
            .collect()
    }

    /// 按证券和时间窗口取历史日线的数据集，以及它写入的日线表：数据核检逐只核对它们，补偿时按股票补
    pub fn bar_histories(&self) -> Vec<(&DatasetSpec, &'static str)> {
        self.datasets
            .iter()
            .filter(|d| matches!(d.partition, Partition::SecurityWindow { .. }))
            .filter_map(|d| {
                BAR_TABLES
                    .iter()
                    .find(|t| d.sink.iter().any(|s| s == *t))
                    .map(|t| (d, *t))
            })
            .collect()
    }

    /// 个股历史日线的数据集（写 `mkt.stock_bar_1d`、按证券和时间窗口取）
    pub fn stock_bar_history(&self) -> Option<&DatasetSpec> {
        self.bar_histories()
            .into_iter()
            .find(|(_, t)| *t == STOCK_BARS)
            .map(|(d, _)| d)
    }
}

const KNOWN_SINKS: &[&str] = &[
    "ref.security",
    "ref.etf_security",
    "ref.index_info",
    "ref.trading_day",
    "mkt.stock_bar_1d",
    "mkt.etf_bar_1d",
    "mkt.index_bar_1d",
    "mkt.corp_action",
    "ref.index_member",
    "fin.valuation_1d",
];

fn validate_dependency_cycle(
    spec: &DatasetSpec,
    all: &[DatasetSpec],
    path: &mut std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    if !path.insert(spec.id.clone()) {
        bail!(
            "数据集依赖存在循环：{}",
            path.iter().cloned().collect::<Vec<_>>().join(" -> ")
        );
    }
    if let Partition::FromDataset { dataset, .. } = &spec.partition {
        let next = all
            .iter()
            .find(|d| d.id == *dataset)
            .expect("from_dataset 已在上层校验");
        validate_dependency_cycle(next, all, path)?;
    }
    path.remove(&spec.id);
    Ok(())
}

/// 日线表
pub const STOCK_BARS: &str = "mkt.stock_bar_1d";
pub const ETF_BARS: &str = "mkt.etf_bar_1d";
pub const BAR_TABLES: [&str; 3] = [STOCK_BARS, ETF_BARS, "mkt.index_bar_1d"];
/// 影响复权价的表：个股日线和公司行动。写入它们的数据集有变化时，运行结束后重算复权；
/// 它们的首次回补没完成时不生成建议（`readiness::CORE_TABLES` 是同一组表）
pub const PRICE_TABLES: [&str; 2] = crate::readiness::CORE_TABLES;

#[derive(Debug, Clone)]
pub struct DatasetSpec {
    pub id: String,
    pub domain: String,
    /// 中文名、分类、一句话说明：数据同步页展示用
    pub title: String,
    pub category: String,
    pub summary: String,
    /// 这个数据集取不到时，由哪个数据集补回同样的数据
    pub recover_with: Option<String>,
    pub endpoint: String,
    pub priority: String,
    pub enabled: bool,
    pub partition: Partition,
    pub params: BTreeMap<String, String>,
    pub paging: Paging,
    pub records: Records,
    pub key: String,
    pub thscode: Option<String>,
    pub obs_date: Option<String>,
    pub incremental: Incremental,
    pub schedule: Schedule,
    pub store_record: bool,
    /// 只采集运行交易日收盘后的数据（快照类数据集），见 calendar::TradeDateGuard
    pub trade_date_guard: bool,
    /// 这些供应商错误码按「没有数据」处理：不重试，任务记为 empty
    pub empty_codes: Vec<i64>,
    pub sink: Vec<String>,
    /// 同一类型化表的投影变体，例如 history / snapshot / valuation / float_cap。
    /// 新增数据集复用已有 sink 时只需声明变体，不需要修改同步代码。
    pub sink_mode: Option<String>,
    /// 完整的原始配置，写入 ops.dataset.spec
    pub raw: Value,
}

impl DatasetSpec {
    fn from_value(v: &Value, defaults: &Value) -> anyhow::Result<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let id = s("id").context("缺少 id")?;
        let domain = id.split('.').next().unwrap_or_default().to_string();
        let priority = s("priority").context("缺少 priority")?;
        if !["P0", "P1", "P2", "P3"].contains(&priority.as_str()) {
            bail!("priority 只能是 P0–P3：{priority}");
        }
        let pick = |k: &str| {
            v.get(k)
                .or_else(|| defaults.get(k))
                .cloned()
                .unwrap_or(Value::Null)
        };
        let bool_of = |k: &str, dflt: bool| pick(k).as_bool().unwrap_or(dflt);
        let params = match v.get("params") {
            Some(Value::Object(m)) => m
                .iter()
                .map(|(k, v)| (k.clone(), scalar_to_string(v)))
                .collect(),
            Some(Value::Null) | None => BTreeMap::new(),
            Some(_) => bail!("params 必须是映射"),
        };
        let sink = match v.get("sink") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => vec![],
        };
        let sink_mode = s("sink_mode");
        let text = |k: &str| {
            s(k).map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .with_context(|| format!("缺少 {k}"))
        };
        Ok(DatasetSpec {
            title: text("title")?,
            category: text("category")?,
            summary: text("summary")?,
            recover_with: s("recover_with"),
            endpoint: s("endpoint").context("缺少 endpoint")?,
            // 未显式配置时，P0/P1 默认开启，P2/P3 默认关闭
            enabled: v
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(priority == "P0" || priority == "P1"),
            partition: Partition::from_value(v.get("partition").context("缺少 partition")?)?,
            params,
            paging: Paging::from_value(&pick("paging"))?,
            records: Records::from_value(v.get("records").context("缺少 records")?)?,
            key: s("key").context("缺少 key")?,
            thscode: s("thscode"),
            obs_date: s("obs_date"),
            incremental: Incremental::from_value(&pick("incremental"))?,
            schedule: Schedule::from_value(&pick("schedule"))?,
            store_record: bool_of("store_record", true),
            trade_date_guard: bool_of("trade_date_guard", false),
            empty_codes: v
                .get("empty_codes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default(),
            sink,
            sink_mode,
            raw: v.clone(),
            domain,
            priority,
            id,
        })
    }

    pub fn priority_rank(&self) -> i16 {
        match self.priority.as_str() {
            "P0" => 10,
            "P1" => 20,
            "P2" => 30,
            _ => 40,
        }
    }

    /// 全市场导出（先取预签名链接，再下载 Parquet）
    pub fn is_dump(&self) -> bool {
        matches!(self.records, Records::Parquet)
    }
}

pub fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ───────────────────────── 日期规则 ─────────────────────────

/// 绝对日期（2005-01-01）或相对今天的偏移（-1y、-5y、-30d）
#[derive(Debug, Clone, PartialEq)]
pub enum DateSpec {
    Abs(NaiveDate),
    YearsAgo(u32),
    DaysAgo(i64),
}

impl DateSpec {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let s = s.trim();
        if let Some(n) = s.strip_prefix('-').and_then(|r| r.strip_suffix('y')) {
            return Ok(DateSpec::YearsAgo(n.parse()?));
        }
        if let Some(n) = s.strip_prefix('-').and_then(|r| r.strip_suffix('d')) {
            return Ok(DateSpec::DaysAgo(n.parse()?));
        }
        Ok(DateSpec::Abs(
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .with_context(|| format!("无法解析日期：{s}"))?,
        ))
    }

    pub fn resolve(&self, today: NaiveDate) -> NaiveDate {
        match self {
            DateSpec::Abs(d) => *d,
            DateSpec::YearsAgo(n) => today
                .checked_sub_months(Months::new(n * 12))
                .unwrap_or(today),
            DateSpec::DaysAgo(n) => today - Duration::days(*n),
        }
    }
}

// ───────────────────────── 分区 ─────────────────────────

#[derive(Debug, Clone)]
pub enum Partition {
    Once,
    Enum(BTreeMap<String, Vec<String>>),
    Security {
        universe: String,
    },
    SecurityBatch {
        universe: String,
        size: usize,
    },
    SecurityWindow {
        universe: String,
        window_days: i64,
        floor: DateSpec,
        start_from_list_date: bool,
    },
    TradingDay {
        from: DateSpec,
        natural_calendar: bool,
        dates_from: Option<String>,
    },
    ReportPeriod {
        universe: String,
        from_year: i32,
        from_quarter: u32,
        quarters: Vec<u32>,
    },
    /// filter：额外的 SQL 条件，作用于来源记录（别名 r，列 rkey / thscode / obs_date / payload）
    FromDataset {
        dataset: String,
        fields: BTreeMap<String, String>,
        filter: Option<String>,
    },
    /// 配置里出现、但规划器尚不支持的分区（例如 cross），该数据集无法规划
    Unsupported(String),
}

impl Partition {
    fn from_value(v: &Value) -> anyhow::Result<Self> {
        if v.as_str() == Some("once") {
            return Ok(Partition::Once);
        }
        let m = v.as_object().context("partition 必须是 once 或映射")?;
        if m.contains_key("cross") {
            return Ok(Partition::Unsupported("cross（多维组合分区）".into()));
        }
        let (kind, body) = m.iter().next().context("partition 为空")?;
        let universe = || -> anyhow::Result<String> {
            body.get("universe")
                .and_then(Value::as_str)
                .map(str::to_string)
                .context("缺少 universe")
        };
        Ok(match kind.as_str() {
            "enum" => {
                let mut out = BTreeMap::new();
                for (k, vals) in body.as_object().context("enum 必须是映射")? {
                    let vals = vals.as_array().context("enum 的取值必须是列表")?;
                    out.insert(k.clone(), vals.iter().map(scalar_to_string).collect());
                }
                Partition::Enum(out)
            }
            "security" => Partition::Security {
                universe: universe()?,
            },
            "security_batch" => Partition::SecurityBatch {
                universe: universe()?,
                size: positive_usize(body.get("size"), "security_batch.size")?,
            },
            "security_window" => Partition::SecurityWindow {
                universe: universe()?,
                window_days: positive_i64(body.get("window_days"), "security_window.window_days")?,
                floor: DateSpec::parse(&scalar_to_string(
                    body.get("floor").context("缺少 floor")?,
                ))?,
                start_from_list_date: body.get("start_from").and_then(Value::as_str)
                    == Some("list_date"),
            },
            "trading_day" => Partition::TradingDay {
                from: DateSpec::parse(&scalar_to_string(
                    body.get("from").context("trading_day 缺少 from")?,
                ))?,
                natural_calendar: body.get("calendar").and_then(Value::as_str) == Some("natural"),
                dates_from: body
                    .get("dates_from")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            "report_period" => {
                let from = scalar_to_string(body.get("from").context("report_period 缺少 from")?);
                let (y, q) = from
                    .split_once('-')
                    .context("report_period.from 格式应为 YYYY-Q")?;
                let quarters = body
                    .get("quarters")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_u64)
                            .map(|q| q as u32)
                            .collect()
                    })
                    .unwrap_or_else(|| vec![1, 2, 3, 4]);
                Partition::ReportPeriod {
                    universe: universe()?,
                    from_year: y.parse()?,
                    from_quarter: q.parse()?,
                    quarters,
                }
            }
            "from_dataset" => Partition::FromDataset {
                dataset: body
                    .get("dataset")
                    .and_then(Value::as_str)
                    .context("from_dataset 缺少 dataset")?
                    .to_string(),
                fields: body
                    .get("fields")
                    .and_then(Value::as_object)
                    .context("from_dataset 缺少 fields")?
                    .iter()
                    .map(|(k, v)| (k.clone(), scalar_to_string(v)))
                    .collect(),
                filter: body
                    .get("filter")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            other => bail!("未知的分区类型：{other}"),
        })
    }
}

// ───────────────────────── 分页、记录、增量、调度 ─────────────────────────

#[derive(Debug, Clone)]
pub enum Paging {
    None,
    Offset {
        limit_param: String,
        offset_param: String,
        size: usize,
    },
    Page {
        page_param: String,
        size_param: Option<String>,
        size: usize,
    },
}

impl Paging {
    fn from_value(v: &Value) -> anyhow::Result<Self> {
        if v.is_null() || v.as_str() == Some("none") {
            return Ok(Paging::None);
        }
        let get = |b: &Value, k: &str| b.get(k).and_then(Value::as_str).map(str::to_string);
        if let Some(b) = v.get("offset") {
            return Ok(Paging::Offset {
                limit_param: get(b, "limit_param").unwrap_or_else(|| "limit".into()),
                offset_param: get(b, "offset_param").unwrap_or_else(|| "offset".into()),
                size: positive_usize(b.get("size"), "offset.size")?,
            });
        }
        if let Some(b) = v.get("page") {
            return Ok(Paging::Page {
                page_param: get(b, "page_param").unwrap_or_else(|| "page".into()),
                size_param: get(b, "size_param"),
                size: positive_usize(b.get("size"), "page.size")?,
            });
        }
        bail!("未知的分页方式：{v}")
    }
}

/// 记录在响应中的位置
#[derive(Debug, Clone)]
pub enum Records {
    /// 一个或多个路径（如 data.item）；多个路径时，每条记录的 `_array` 是来源路径的最后一段
    Paths(Vec<String>),
    /// 先取预签名链接，再下载 Parquet；每行一条记录
    Parquet,
}

impl Records {
    fn from_value(v: &Value) -> anyhow::Result<Self> {
        match v {
            Value::String(s) if s == "parquet" => Ok(Records::Parquet),
            Value::String(s) => Ok(Records::Paths(vec![s.clone()])),
            Value::Array(a) => Ok(Records::Paths(
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            )),
            _ => Err(anyhow!("records 必须是路径、路径列表或 parquet")),
        }
    }
}

fn positive_usize(v: Option<&Value>, name: &str) -> anyhow::Result<usize> {
    let n = v
        .and_then(Value::as_u64)
        .with_context(|| format!("{name} 必须是正整数"))?;
    usize::try_from(n)
        .ok()
        .filter(|n| *n > 0)
        .with_context(|| format!("{name} 必须是正整数"))
}

fn positive_i64(v: Option<&Value>, name: &str) -> anyhow::Result<i64> {
    let n = v
        .and_then(Value::as_i64)
        .with_context(|| format!("{name} 必须是正整数"))?;
    (n > 0)
        .then_some(n)
        .with_context(|| format!("{name} 必须是正整数"))
}

#[derive(Debug, Clone)]
pub enum Incremental {
    /// 每次整份重取（与回补的分区方式相同）
    Rerun,
    /// 从游标往后取，重叠 overlap_days 天
    SinceCursor { overlap_days: i64 },
    /// 只取最近 N 期：params 覆盖请求参数（去掉时间窗口参数），或 n 指定最近 N 个报告期
    LatestN {
        params: BTreeMap<String, String>,
        n: Option<usize>,
    },
}

impl Incremental {
    fn from_value(v: &Value) -> anyhow::Result<Self> {
        if v.is_null() || v.as_str() == Some("rerun") {
            return Ok(Incremental::Rerun);
        }
        if let Some(b) = v.get("since_cursor") {
            return Ok(Incremental::SinceCursor {
                overlap_days: b.get("overlap_days").and_then(Value::as_i64).unwrap_or(0),
            });
        }
        if let Some(b) = v.get("latest_n") {
            let params = b
                .get("params")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), scalar_to_string(v)))
                        .collect()
                })
                .unwrap_or_default();
            return Ok(Incremental::LatestN {
                params,
                n: b.get("n").and_then(Value::as_u64).map(|n| n as usize),
            });
        }
        bail!("未知的增量规则：{v}")
    }
}

/// 同步计划：数据集多久同步一次，见 docs/architecture/sync_pipeline.md#scheduling
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    /// 每个交易日收盘后
    Daily,
    Weekly,
    /// 每月第一次每周同步时
    Monthly,
    /// 财报季（1、4、7、8、10 月）随每日同步，其余月份随每周同步
    ReportSeason,
    /// 不自动同步，只手动发起
    Manual,
}

impl Schedule {
    pub const ALL: [Schedule; 5] = [
        Schedule::Daily,
        Schedule::Weekly,
        Schedule::Monthly,
        Schedule::ReportSeason,
        Schedule::Manual,
    ];

    /// 配置和接口中的写法
    pub fn key(self) -> &'static str {
        match self {
            Schedule::Daily => "daily",
            Schedule::Weekly => "weekly",
            Schedule::Monthly => "monthly",
            Schedule::ReportSeason => "report_season",
            Schedule::Manual => "manual",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Schedule::Daily => "每日收盘后",
            Schedule::Weekly => "每周",
            Schedule::Monthly => "每月",
            Schedule::ReportSeason => "财报季",
            Schedule::Manual => "不自动同步",
        }
    }

    fn from_value(v: &Value) -> anyhow::Result<Self> {
        let key = v.as_str().unwrap_or("daily");
        Schedule::ALL
            .into_iter()
            .find(|s| s.key() == key)
            .ok_or_else(|| anyhow::anyhow!("未知的同步计划：{key}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_registry_parses() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/datasets.yaml");
        let reg = Registry::load(&path).expect("config/datasets.yaml 应能解析");
        assert!(reg.datasets.len() >= 80);
        // 数据集的角色由 sink 和分区决定
        let price: Vec<&str> = reg
            .writing_to(&PRICE_TABLES)
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(
            price,
            [
                // 全市场导出和逐只历史都写 mkt.stock_bar_1d：前者覆盖最近约 10 年，后者补齐更早的年份
                "a_share.bar_dump",
                "a_share.bar_1d",
                "a_share.snapshot",
                "a_share.corp_action_dump"
            ]
        );
        let bars: Vec<(&str, &str)> = reg
            .bar_histories()
            .iter()
            .map(|(d, t)| (d.id.as_str(), *t))
            .collect();
        assert_eq!(
            bars,
            [
                ("a_share.bar_1d", "mkt.stock_bar_1d"),
                ("index.bar_1d", "mkt.index_bar_1d"),
                ("fund.etf_bar_1d", "mkt.etf_bar_1d")
            ]
        );
        assert_eq!(
            reg.stock_bar_history().map(|d| d.id.as_str()),
            Some("a_share.bar_1d")
        );
        let bar = reg.get("a_share.bar_1d").unwrap();
        assert!(matches!(
            bar.partition,
            Partition::SecurityWindow {
                window_days: 3650,
                start_from_list_date: true,
                ..
            }
        ));
        assert!(!bar.store_record);
        assert!(matches!(
            bar.incremental,
            Incremental::SinceCursor { overlap_days: 10 }
        ));
        assert!(reg.get("a_share.corp_action_dump").unwrap().is_dump());
        // ETF 资料和行情默认开启，其他 P2/P3 数据仍默认关闭
        assert!(reg.get("fund.profile").unwrap().enabled);
        assert!(reg.get("fund.etf_bar_1d").unwrap().enabled);
        assert!(reg.get("fund.market_snapshot").unwrap().enabled);
        assert_eq!(reg.get("fund.market_snapshot").unwrap().empty_codes, [3002]);
        assert!(reg.get("special.limit_up_pool").unwrap().enabled);
    }

    #[test]
    fn relative_dates() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        assert_eq!(
            DateSpec::parse("-1y").unwrap().resolve(today),
            NaiveDate::from_ymd_opt(2025, 10, 15).unwrap()
        );
        assert_eq!(
            DateSpec::parse("2005-01-01").unwrap().resolve(today),
            NaiveDate::from_ymd_opt(2005, 1, 1).unwrap()
        );
    }
}
