/** 因子研究：先检验因子有没有预测力（RankIC、分组收益、挑选偏差），再用多个因子合成打分做组合回测。
 * 规则见 docs/domains/factor_research.md */
import { keepPreviousData, useQueryClient } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { Link } from "react-router";
import type {
  FactorInfo,
  FactorPortfolioParams,
  FactorPortfolioResult,
  FactorTestParams,
  FactorTestResult,
  FactorTestStats,
  RunManifest,
} from "../api/generated/model";
import {
  getFactorPortfolioQueryKey,
  getFactorTestQueryKey,
  useFactorCatalog,
  useFactorPortfolio,
  useFactorTest,
} from "../api/generated/research/research";
import { EquityChart } from "../components/EquityChart";
import { type Line, MultiLineChart, quantileColors } from "../components/FactorCharts";
import { TaskRunning, useElapsedSeconds } from "../components/longTask";
import { CostFields, costParams, LogToggle, NumberField, RulesNote } from "../components/researchFields";
import {
  Badge,
  buttonClass,
  Card,
  Empty,
  ErrorState,
  HelpTip,
  inputClass,
  PageHeader,
  QueryView,
  Segmented,
  Spinner,
  Stat,
  Table,
} from "../components/ui";
import { useSearchState } from "../hooks/useSearchState";
import {
  DEFAULT_FACTORS,
  type FactorWeight,
  fmtNum,
  fmtProb,
  formatFactors,
  parseFactors,
  toggleFactor,
  verdictTone,
} from "../lib/factors";
import { fmtBig, fmtInt, fmtPercent, rangeStart, upDownClass } from "../lib/format";

const TABS = [
  { value: "test", label: "因子检验", hint: "单个因子有没有预测力：RankIC、分组收益、挑选偏差" },
  { value: "portfolio", label: "多因子组合", hint: "多个因子合成打分，定期持有得分最高的一批股票" },
] as const;

const TEST_RANGES = [
  { value: "3", label: "3 年" },
  { value: "5", label: "5 年" },
  { value: "8", label: "8 年" },
] as const;
const PORTFOLIO_RANGES = [
  { value: "1", label: "1 年" },
  { value: "3", label: "3 年" },
  { value: "5", label: "5 年" },
] as const;
const HORIZONS = [
  { value: "5", label: "5 日", hint: "每周取样一次，看之后 5 个交易日的收益" },
  { value: "10", label: "10 日" },
  { value: "20", label: "20 日", hint: "每月取样一次，看之后 20 个交易日的收益" },
] as const;
const REBALANCES = [
  { value: "5", label: "每周" },
  { value: "10", label: "两周" },
  { value: "20", label: "每月" },
  { value: "60", label: "每季" },
] as const;
const SMALL = [
  { value: "0", label: "不剔除" },
  { value: "30", label: "剔除最小 30%", hint: "按成交额剔除最小的 30%：小盘股里有一部分是壳价值，不是因子带来的收益" },
] as const;
const ON_OFF = [
  { value: "1", label: "行业和规模中性" },
  { value: "", label: "不中性化", hint: "因子里会混入行业和大小盘的影响" },
] as const;

export function ResearchPage() {
  const [s, set] = useSearchState({ tab: "test", f: DEFAULT_FACTORS });
  const picks = parseFactors(s.f);
  return (
    <>
      <PageHeader
        title="因子研究"
        desc="均线金叉这类单只股票的买卖信号，在 A 股扣掉成本后很难赚钱。专业做法是每天给全市场股票按某个特征（因子）打分排名，先检验排名靠前的股票之后是否真的涨得更多，再把几个有效的因子合起来，定期持有得分最高的一批。"
      />
      <Card>
        <div className="flex flex-col gap-3">
          <Segmented value={s.tab as "test" | "portfolio"} options={TABS} onChange={(tab) => set({ tab })} />
          <FactorPicker
            picks={picks}
            onChange={(next) => set({ f: formatFactors(next) })}
            weights={s.tab === "portfolio"}
          />
        </div>
      </Card>
      {picks.length === 0 ? (
        <Card>
          <Empty>至少选一个因子</Empty>
        </Card>
      ) : s.tab === "portfolio" ? (
        <PortfolioPanel picks={picks} />
      ) : (
        <TestPanel picks={picks} />
      )}
    </>
  );
}

/** 按类别列出因子，点一下选中或取消；组合模式下每个选中的因子可以填权重 */
function FactorPicker({
  picks,
  onChange,
  weights,
}: {
  picks: FactorWeight[];
  onChange: (p: FactorWeight[]) => void;
  weights: boolean;
}) {
  const catalog = useFactorCatalog({ query: { staleTime: Number.POSITIVE_INFINITY } });
  return (
    <QueryView query={catalog}>
      {(items) => (
        <div className="flex flex-col gap-3">
          {(
            [
              ["price_volume", "价量因子（只用日线）"],
              ["fundamental", "财务因子（按法定披露截止日才使用，避免偷看未来）"],
            ] as const
          ).map(([cat, title]) => (
            <div key={cat} className="flex flex-col gap-1.5">
              <span className="text-xs text-zinc-500">{title}</span>
              <div className="flex flex-wrap gap-2">
                {items
                  .filter((f) => f.category === cat)
                  .map((f) => (
                    <FactorChip
                      key={f.id}
                      f={f}
                      pick={picks.find((p) => p.id === f.id)}
                      weights={weights}
                      onToggle={() => onChange(toggleFactor(picks, f.id))}
                      onWeight={(w) => onChange(picks.map((p) => (p.id === f.id ? { ...p, weight: w } : p)))}
                    />
                  ))}
              </div>
            </div>
          ))}
        </div>
      )}
    </QueryView>
  );
}

function FactorChip({
  f,
  pick,
  weights,
  onToggle,
  onWeight,
}: {
  f: FactorInfo;
  pick?: FactorWeight;
  weights: boolean;
  onToggle: () => void;
  onWeight: (w: number) => void;
}) {
  const on = pick !== undefined;
  return (
    <span
      className={`inline-flex items-center gap-1 rounded-lg border px-2 py-1 text-sm ${
        on ? "border-accent bg-accent/5" : "border-zinc-200 dark:border-zinc-700"
      }`}
    >
      <button type="button" onClick={onToggle} className="inline-flex items-center gap-1" title={f.description}>
        <span className={on ? "text-accent" : "text-zinc-400"}>{on ? "✓" : "+"}</span>
        {f.name}
        <span className="text-xs text-zinc-400">{f.higher_is_better ? "越大越好" : "越小越好"}</span>
      </button>
      <HelpTip label={`${f.name}的说明`}>
        <b>{f.description}。</b>
        <br />
        {f.rationale}
      </HelpTip>
      {on && weights && (
        <input
          aria-label={`${f.name}的权重`}
          className={`${inputClass} num ml-1 w-14 px-1.5 py-0.5 text-xs`}
          defaultValue={pick.weight}
          title="权重：作用在按预期方向统一后的得分上，负数表示反过来用"
          onBlur={(e) => {
            const w = Number(e.target.value);
            if (Number.isFinite(w) && w !== pick.weight) onWeight(w);
          }}
        />
      )}
    </span>
  );
}

/** 计算的通用状态：只有点按钮时才把参数固定下来，算的时候可以取消 */
function useManualRun<P>(form: P, queryKey: (p: P) => readonly unknown[]) {
  const qc = useQueryClient();
  const [run, setRun] = useState<P | null>(null);
  const changed = run !== null && JSON.stringify(run) !== JSON.stringify(form);
  return {
    run,
    changed,
    start: (refetch: () => unknown) => (run && !changed ? refetch() : setRun(form)),
    cancel: () => {
      if (run) void qc.cancelQueries({ queryKey: queryKey(run) });
      setRun(null);
    },
  };
}

const QUERY_OPTIONS = {
  staleTime: 10 * 60_000,
  placeholderData: keepPreviousData,
  retry: false,
  refetchOnWindowFocus: false,
} as const;

function RunBar({
  running,
  seconds,
  changed,
  hasRun,
  onStart,
  onCancel,
  estimate,
}: {
  running: boolean;
  seconds: number;
  changed: boolean;
  hasRun: boolean;
  onStart: () => void;
  onCancel: () => void;
  estimate: string;
}) {
  return (
    <>
      <div className="mt-4 flex flex-wrap items-center gap-3">
        {running ? (
          <TaskRunning text="正在逐只计算全市场几年的日线和因子" seconds={seconds} onCancel={onCancel} />
        ) : (
          <>
            <button type="button" className={buttonClass()} onClick={onStart}>
              {hasRun && changed ? "按新参数重新计算" : hasRun ? "重新计算" : "开始计算"}
            </button>
            <span className="text-xs text-zinc-500">{estimate}</span>
          </>
        )}
      </div>
      {changed && !running && (
        <p className="mt-2 text-xs text-amber-600">参数已改，下面还是上一次的结果；点上面的按钮按新参数重新计算。</p>
      )}
    </>
  );
}

function TestPanel({ picks }: { picks: FactorWeight[] }) {
  const [s, set] = useSearchState({ range: "5", h: "5", q: "5", n: "1", small: "0" });
  const form: FactorTestParams = {
    factors: formatFactors(picks, false),
    start: rangeStart(s.range),
    horizon: Number(s.h),
    quantiles: Number(s.q),
    neutralize: s.n === "1",
    exclude_small_pct: Number(s.small),
  };
  const m = useManualRun(form, getFactorTestQueryKey);
  const query = useFactorTest(m.run ?? form, { query: { ...QUERY_OPTIONS, enabled: m.run !== null } });
  const running = m.run !== null && query.isFetching;
  const seconds = useElapsedSeconds(running);
  return (
    <>
      <Card>
        <div className="flex flex-wrap items-end gap-4">
          <Labeled label="看过去多久">
            <Segmented value={s.range} options={TEST_RANGES} onChange={(range) => set({ range })} />
          </Labeled>
          <Labeled label="持有期">
            <Segmented value={s.h} options={HORIZONS} onChange={(h) => set({ h })} />
          </Labeled>
          <Labeled label="分几组">
            <Segmented
              value={s.q}
              options={[
                { value: "5", label: "5 组" },
                { value: "10", label: "10 组" },
              ]}
              onChange={(q) => set({ q })}
            />
          </Labeled>
          <Labeled label="中性化">
            <Segmented value={s.n} options={ON_OFF} onChange={(n) => set({ n })} />
          </Labeled>
          <Labeled label="小盘股">
            <Segmented value={s.small} options={SMALL} onChange={(small) => set({ small })} />
          </Labeled>
        </div>
        <RunBar
          running={running}
          seconds={seconds}
          changed={m.changed}
          hasRun={m.run !== null}
          onStart={() => m.start(query.refetch)}
          onCancel={m.cancel}
          estimate="全市场 5 年通常 20 到 40 秒；含财务因子时更久"
        />
      </Card>
      {m.run !== null &&
        (query.error && !running ? (
          <Card>
            <ErrorState error={query.error} onRetry={() => query.refetch()} />
          </Card>
        ) : query.data !== undefined ? (
          <TestResult data={query.data} />
        ) : running ? null : (
          <Card>
            <Spinner />
          </Card>
        ))}
      <RulesNote
        items={[
          "每隔「持有期」个交易日取样一次：当天收盘时给股票池里的每只股票算因子值，下一个交易日开盘买入、持有期结束后的开盘卖出，用前复权价算收益。买入日停牌或一字涨停的股票不计入，卖出日一字跌停顺延。",
          "因子值先去极值（中位数 ± 3 倍稳健标准差），再标准化；选择中性化时对同花顺行业和规模（20 日平均成交额的对数）回归取残差。规模因子本身只做行业中性化。",
          "RankIC 是因子排名和之后收益排名的相关系数，A 股里单个因子稳定在 0.03 以上就算有用。t 值按 Harvey、Liu、Zhu 的建议要达到 3 才算显著：因子被试得越多，偶然显著的就越多。",
          "过拟合概率（PBO）用组合对称交叉验证估计：把时间切成 8 段，枚举一半做样本内、一半做样本外，样本内最好的候选在样本外排到后一半的比例。Deflated Sharpe 是考虑挑选次数之后真实 Sharpe 大于 0 的概率。",
          "财务因子按法定披露截止日（一季报 4 月底、半年报 8 月底、三季报 10 月底、年报次年 4 月底）之后才使用；供应商的报告日期会被之后的报告覆盖，不能直接用。",
          "已知偏差：本地没有开始采集之前就退市的股票（幸存者偏差，结果偏乐观）；行业用当前归属；没有历史股本，用成交额代替市值。",
        ]}
      />
    </>
  );
}

function Labeled({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1 text-xs text-zinc-500">
      <span>{label}</span>
      {children}
    </div>
  );
}

function TestResult({ data }: { data: FactorTestResult }) {
  const [selected, setSelected] = useState(0);
  const f = data.factors[Math.min(selected, data.factors.length - 1)];
  const o = data.overfit;
  if (!f) return null;
  return (
    <>
      <Card
        title={
          <span className="text-xs font-normal text-zinc-500">
            {data.start} 至 {data.end} · 持有期 {data.horizon} 个交易日 · 分 {data.quantiles} 组 ·{" "}
            {data.config.neutralize ? "行业和规模中性" : "未中性化"}
            {data.thin_days.length > 0 && ` · 跳过同步缺口 ${data.thin_days.join("、")}`}
          </span>
        }
      >
        <Table>
          <thead>
            <tr>
              <th>因子</th>
              <th title="排名和之后收益排名的相关系数，按预期方向，正数表示符合预期">RankIC</th>
              <th title="RankIC 均值 ÷ 标准差：越大越稳定">ICIR</th>
              <th title="RankIC 均值的 t 值，3 以上才算显著">t 值</th>
              <th title="RankIC 为正的期数占比">胜率</th>
              <th title="最好一组减最差一组的年化收益，不含交易成本">多空年化</th>
              <th title="最好一组相对股票池平均的年化超额，不含交易成本">最好组超额</th>
              <th title="最好一组每期换掉的比例，越高交易成本越大">换手</th>
              <th>结论</th>
            </tr>
          </thead>
          <tbody>
            {data.factors.map((x, i) => (
              <tr
                key={x.name}
                onClick={() => setSelected(i)}
                className={`cursor-pointer ${i === selected ? "bg-accent/5" : "hover:bg-zinc-50 dark:hover:bg-zinc-800/50"}`}
              >
                <td className="font-medium">
                  {x.name}
                  {x.id === null || x.id === undefined ? (
                    <span className="ml-1 text-xs text-zinc-400">（以上因子等权）</span>
                  ) : null}
                </td>
                <td className={`num ${upDownClass(x.rank_ic_mean)}`}>{fmtNum(x.rank_ic_mean, 3)}</td>
                <td className="num">{fmtNum(x.rank_icir)}</td>
                <td className="num">{fmtNum(x.rank_ic_t)}</td>
                <td className="num">{fmtProb(x.hit_rate)}</td>
                <td className={`num ${upDownClass(x.long_short_annual)}`}>{fmtPercent(x.long_short_annual, 1)}</td>
                <td className={`num ${upDownClass(x.top_excess_annual)}`}>{fmtPercent(x.top_excess_annual, 1)}</td>
                <td className="num">{fmtProb(x.top_turnover)}</td>
                <td>
                  <Badge tone={verdictTone(x)}>{x.verdict}</Badge>
                </td>
              </tr>
            ))}
          </tbody>
        </Table>
        <p className="mt-3 text-xs text-zinc-500">
          点一行看它的分组净值、IC 衰减；收益都不含交易成本，换手越高实际越打折。
        </p>
        <RunNote manifest={data.manifest} />
      </Card>
      <div className="grid gap-4 lg:grid-cols-3">
        <Card title={`${f.name}：各组净值`} className="lg:col-span-2">
          <QuantileChart f={f} />
        </Card>
        <Card title="挑选偏差">
          <div className="grid grid-cols-2 gap-4">
            <Stat
              label="过拟合概率 PBO"
              value={fmtProb(o.pbo)}
              sub={o.pbo === null || o.pbo === undefined ? "至少两个候选才能估计" : "越低越好，高于 50% 说明在挑噪声"}
            />
            <Stat
              label="Deflated Sharpe"
              value={fmtProb(o.deflated_sharpe)}
              sub={o.best ? `最好的候选「${o.best}」扣除挑选次数后，真实有效的概率；95% 以上才可信` : undefined}
            />
          </div>
          <p className="mt-3 text-xs text-zinc-500">
            本次比较 {o.candidates} 个候选（各因子的多空组合）；这类检验已试过 {data.manifest.trial_count} 组不同配置。
          </p>
          <h3 className="mt-4 mb-1 text-xs font-medium text-zinc-500">IC 衰减（不同持有期的 RankIC）</h3>
          <div className="flex gap-3 text-sm">
            {f.decay.map((d) => (
              <span key={d.horizon} className="num">
                {d.horizon} 日 <b className={upDownClass(d.rank_ic)}>{fmtNum(d.rank_ic, 3)}</b>
              </span>
            ))}
          </div>
          <p className="mt-1 text-xs text-zinc-500">持有期越长 RankIC 仍高，说明因子衰减慢，可以少调仓、省成本。</p>
        </Card>
      </div>
      {data.factors.length > 1 && <Correlation data={data} />}
    </>
  );
}

/** 运行清单：数据覆盖范围和已知偏差。结果必须在了解这些边界之后再读。 */
function RunNote({ manifest }: { manifest: RunManifest }) {
  // 回补还没跑完时横截面用的是残缺样本，IC 和分组收益会失真，这里必须显眼地提示
  const partial = manifest.coverage && manifest.coverage.ratio < COVERAGE_WARN_RATIO;
  return (
    <>
      {partial && manifest.coverage && (
        <p className="mt-3 rounded border border-amber-300 bg-amber-50 px-3 py-2 text-xs text-amber-800 dark:border-amber-700 dark:bg-amber-950 dark:text-amber-300">
          数据还没补全：区间内全市场应有约 {fmtInt(manifest.coverage.expected)} 只，实际每个取样日中位数{" "}
          {fmtInt(manifest.coverage.observed)} 只（
          {(manifest.coverage.ratio * 100).toFixed(0)}%）。历史回补没跑完时结论会失真，仅供参考。
        </p>
      )}
      <p className="mt-3 text-xs text-zinc-500">
        覆盖 {fmtInt(manifest.instruments)} 只股票、{fmtInt(manifest.observed_bars)} 根日线
        {manifest.derived_versions && Object.keys(manifest.derived_versions).length > 0
          ? ` · 派生数据：${Object.entries(manifest.derived_versions)
              .map(([k, v]) => `${k} ${v.slice(0, 10)}`)
              .join("，")}`
          : null}
        ；已知偏差：{BIAS_NOTES[manifest.bias] ?? manifest.bias}。
      </p>
    </>
  );
}

/** 与后端 factor_lab::COVERAGE_WARN_RATIO 保持一致 */
const COVERAGE_WARN_RATIO = 0.9;

const BIAS_NOTES: Record<string, string> = {
  "survivorship_biased:no_history_before_collection;current_industry;fundamentals_at_statutory_deadline":
    "不含开始采集以前退市的股票（幸存者偏差），行业按当前归属，财报可用日按法定披露截止日推断",
};

function QuantileChart({ f }: { f: FactorTestStats }) {
  const lines: Line[] = useMemo(() => {
    const n = f.quantile_returns.length;
    const colors = quantileColors(n);
    const out: Line[] = Array.from({ length: n }, (_, g) => ({
      label: g === 0 ? "第 1 组（预期最差）" : g === n - 1 ? `第 ${n} 组（预期最好）` : `第 ${g + 1} 组`,
      color: colors[g] ?? "#a1a1aa",
      width: g === 0 || g === n - 1 ? 2 : 1,
      points: f.points.map((p) => ({ date: p.date, value: p.quantile_navs[g] ?? Number.NaN })),
    }));
    out.push({
      label: "多空（最好减最差）",
      color: "#3b6ef5",
      width: 2,
      points: f.points.map((p) => ({ date: p.date, value: p.long_short_nav })),
    });
    return out;
  }, [f]);
  if (f.points.length === 0) return <Empty>有效期数太少</Empty>;
  return (
    <>
      <MultiLineChart lines={lines} />
      <p className="mt-2 text-xs text-zinc-500">
        好因子的各组从下到上依次排开，多空线稳定向上。各组年化（不含成本）：
        {f.quantile_annual.map((v, g) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: 组序号就是身份
          <span key={g} className={`num ml-2 ${upDownClass(v)}`}>
            {g + 1} 组 {fmtPercent(v, 1)}
          </span>
        ))}
      </p>
    </>
  );
}

function Correlation({ data }: { data: FactorTestResult }) {
  const cell = (v: number | null | undefined) => {
    if (v === null || v === undefined) return "";
    const a = Math.min(Math.abs(v), 1);
    return v >= 0 ? `rgba(59, 110, 245, ${a * 0.5})` : `rgba(220, 38, 38, ${a * 0.5})`;
  };
  return (
    <Card title="因子之间的相关性">
      <Table>
        <thead>
          <tr>
            <th />
            {data.factors.map((f) => (
              <th key={f.name}>{f.name}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {data.correlation.map((row, i) => (
            <tr key={data.factors[i]?.name ?? i}>
              <td className="text-xs text-zinc-500">{data.factors[i]?.name}</td>
              {row.map((v, j) => (
                <td key={data.factors[j]?.name ?? j} className="num text-center" style={{ background: cell(v) }}>
                  {fmtNum(v)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </Table>
      <p className="mt-3 text-xs text-zinc-500">
        平均横截面秩相关（方向已统一）。两个因子相关性高于 0.7 时信息大量重复，合成时只留一个或降低权重。
      </p>
    </Card>
  );
}

const PORTFOLIO_COSTS = { capital: "1000000", commission: "2.5", minFee: "5", stamp: "0.5", slippage: "10" };

function PortfolioPanel({ picks }: { picks: FactorWeight[] }) {
  const [s, set] = useSearchState({
    range: "3",
    reb: "20",
    top: "30",
    buffer: "0.5",
    n: "1",
    small: "0",
    log: "",
    ...PORTFOLIO_COSTS,
  });
  const form: FactorPortfolioParams = {
    factors: formatFactors(picks),
    start: rangeStart(s.range),
    rebalance: Number(s.reb),
    top_n: Number(s.top) || 30,
    buffer: Number(s.buffer),
    neutralize: s.n === "1",
    exclude_small_pct: Number(s.small),
    ...costParams(s),
  };
  const m = useManualRun(form, getFactorPortfolioQueryKey);
  const query = useFactorPortfolio(m.run ?? form, { query: { ...QUERY_OPTIONS, enabled: m.run !== null } });
  const running = m.run !== null && query.isFetching;
  const seconds = useElapsedSeconds(running);
  return (
    <>
      <Card>
        <div className="flex flex-wrap items-end gap-4">
          <Labeled label="看过去多久">
            <Segmented value={s.range} options={PORTFOLIO_RANGES} onChange={(range) => set({ range })} />
          </Labeled>
          <Labeled label="多久调仓一次">
            <Segmented value={s.reb} options={REBALANCES} onChange={(reb) => set({ reb })} />
          </Labeled>
          <NumberField
            label="持有几只"
            hint="5 到 100。只数越多越接近因子本身的表现，单只股票的偶然涨跌影响越小"
            value={s.top}
            onCommit={(top) => set({ top })}
            width="w-16"
          />
          <NumberField
            label="缓冲区"
            hint="已持有的股票排名还在 持有只数 ×（1 + 缓冲区）以内时不卖，减少来回换手。0 到 2"
            value={s.buffer}
            onCommit={(buffer) => set({ buffer })}
            width="w-16"
          />
          <Labeled label="中性化">
            <Segmented value={s.n} options={ON_OFF} onChange={(n) => set({ n })} />
          </Labeled>
          <Labeled label="小盘股">
            <Segmented value={s.small} options={SMALL} onChange={(small) => set({ small })} />
          </Labeled>
          <CostFields s={s} set={set} />
        </div>
        <RunBar
          running={running}
          seconds={seconds}
          changed={m.changed}
          hasRun={m.run !== null}
          onStart={() => m.start(query.refetch)}
          onCancel={m.cancel}
          estimate="全市场 3 年通常 20 到 40 秒"
        />
      </Card>
      {m.run !== null &&
        (query.error && !running ? (
          <Card>
            <ErrorState error={query.error} onRetry={() => query.refetch()} />
          </Card>
        ) : query.data !== undefined ? (
          <PortfolioResultView data={query.data} log={s.log === "1"} onLog={(v) => set({ log: v ? "1" : "" })} />
        ) : running ? null : (
          <Card>
            <Spinner />
          </Card>
        ))}
      <RulesNote
        items={[
          "每到调仓日收盘，给股票池里的股票按选中因子的合成得分排名：各因子先标准化并统一成「越大越好」，再按权重加权平均（至少一半权重的因子有值才参与）。",
          "持有得分最高的若干只；已持有的股票排名还在缓冲区以内时继续持有。下一个交易日开盘先卖后买，新买的每只投入「账户总值 ÷ 持有只数」和「现金 ÷ 待买只数」中较小的一个。",
          "卖出遇到停牌、一字跌停时顺延；买入遇到停牌、一字涨停时顺延，到下一次调仓时取消。之后再没有日线的股票按最后收盘价了结。收益按前复权价计算（相当于分红再投资），允许零股。",
          "佣金（含最低佣金）、印花税、滑点照常扣除。资金少、持有只数多时，最低佣金会明显拖累收益，所以这里默认用 100 万。",
          "基准是全市场等权（含分红），和组合回测相同。Deflated Sharpe 按这类回测已经试过的配置数打折：试得越多，偶然好看的结果越多。",
          "已知偏差：没有开始采集之前就退市的股票（幸存者偏差，结果偏乐观）；行业用当前归属；没有历史股本，用成交额代替市值。",
        ]}
      />
    </>
  );
}

function PortfolioResultView({
  data,
  log,
  onLog,
}: {
  data: FactorPortfolioResult;
  log: boolean;
  onLog: (v: boolean) => void;
}) {
  const m = data.summary;
  const excess = m.excess_annual ?? null;
  return (
    <>
      <Card
        title={
          <span className="text-xs font-normal text-zinc-500">
            {m.start} 至 {m.end}（{fmtInt(m.days)} 个交易日）· 持有 {data.config.top_n} 只 · 每 {data.config.horizon}{" "}
            个交易日调仓 · 调仓 {m.rebalances} 次
          </span>
        }
      >
        <p className="mb-4 text-base">
          {fmtBig(m.capital)} 元按这组因子持有，变成{" "}
          <b className={upDownClass(m.total_return)}>{fmtBig(m.final_equity)} 元</b>（{fmtPercent(m.total_return)}）；
          {m.benchmark_name}是 <b className={upDownClass(m.benchmark_return)}>{fmtPercent(m.benchmark_return)}</b>。
          {excess !== null &&
            (excess >= 0 ? (
              <span className="text-up">每年多赚 {fmtPercent(excess)}。</span>
            ) : (
              <span className="text-down">每年少赚 {fmtPercent(-excess)}。</span>
            ))}
        </p>
        <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
          <Stat
            label="年化收益"
            value={<span className={upDownClass(m.annual_return)}>{fmtPercent(m.annual_return)}</span>}
            sub={`基准年化 ${fmtPercent(m.benchmark_annual)}`}
          />
          <Stat
            label="最多亏过"
            value={<span className="text-down">{fmtPercent(-m.max_drawdown)}</span>}
            sub={`Sharpe ${fmtNum(m.risk.sharpe)}`}
          />
          <Stat
            label="信息比率"
            value={fmtNum(m.information_ratio)}
            sub={`跟踪误差 ${fmtPercent(m.tracking_error, 1).replace("+", "")}；0.5 以上算不错`}
          />
          <Stat
            label="真实有效的概率"
            value={fmtProb(data.deflated_sharpe)}
            sub={`Deflated Sharpe，已按试过的 ${data.manifest.trial_count} 组配置打折；95% 以上才可信`}
          />
        </div>
        <p className="mt-3 text-xs text-zinc-500">
          交易成本 {fmtBig(m.fees)} 元；年换手 {fmtNum(m.turnover_annual, 1)} 倍（每年买入金额 ÷ 平均账户总值）；
          平均持有 {fmtNum(m.avg_positions, 1)} 只；买不进被取消 {m.skipped_buys} 次。
        </p>
        <div className="mt-5">
          <EquityChart points={data.equity} capital={m.capital} log={log} benchmarkLabel={m.benchmark_name} />
          <LogToggle log={log} onLog={onLog} note={`蓝线是因子组合，灰线是${m.benchmark_name}。`} />
        </div>
      </Card>
      <Card title={data.picks_date ? `${data.picks_date} 收盘后得分最高的 ${data.picks.length} 只` : "最新得分"}>
        {data.picks.length === 0 ? (
          <Empty>最近一个交易日没有足够的股票参与排名</Empty>
        ) : (
          <Table>
            <thead>
              <tr>
                <th>排名</th>
                <th>股票</th>
                <th title="合成得分，标准化后的值，越大越好">得分</th>
                <th>组合里已持有</th>
              </tr>
            </thead>
            <tbody>
              {data.picks.map((p) => (
                <tr key={p.thscode}>
                  <td className="num">{p.rank}</td>
                  <td>
                    <Link to={`/securities/${p.thscode}`} className="hover:text-accent">
                      {p.name || p.thscode} <span className="text-xs text-zinc-400">{p.thscode}</span>
                    </Link>
                  </td>
                  <td className="num">{fmtNum(p.score)}</td>
                  <td>{p.held ? <Badge tone="info">已持有</Badge> : null}</td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}
        <p className="mt-3 text-xs text-zinc-500">只是按历史规律排出的名单，不是买卖建议；历史表现不代表未来。</p>
      </Card>
      <RunNote manifest={data.manifest} />
    </>
  );
}
