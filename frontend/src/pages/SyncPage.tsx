import { useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { useMemo, useState } from "react";
import { Link, useNavigate } from "react-router";
import type {
  DatasetStatus,
  Gap,
  RunSummary,
  ScheduleTimes,
  SyncHealth,
  SyncPlan,
  SyncSchedule,
} from "../api/generated/model";
import {
  getGetScheduleQueryKey,
  getListRunsQueryKey,
  useCreateRun,
  useGetSchedule,
  useListDatasets,
  useListRuns,
  useSyncHealth,
  useSyncRepair,
  useUpdateSchedule,
} from "../api/generated/sync/sync";
import { ChainProgress } from "../components/ChainProgress";
import { DataCheck } from "../components/DataCheck";
import {
  Badge,
  buttonClass,
  Card,
  Empty,
  errorText,
  Field,
  inputClass,
  PageHeader,
  ProgressBar,
  QueryView,
  Segmented,
  Spinner,
  Table,
} from "../components/ui";
import { fmtAgo, fmtDateTime, fmtDuration, fmtInt, fmtSlot } from "../lib/format";
import { PLANS, RUN_STATUS, runSource, WEEKDAYS } from "../lib/labels";
import { groupChains } from "../lib/runs";

// @project-doc docs/architecture/frontend.md#pages
/** 数据同步：状态、同步计划（什么时候自动同步什么）、数据核检、同步记录和手动同步，然后按分类看股票与 ETF 等数据 */
export function SyncPage() {
  const health = useSyncHealth({ query: { refetchInterval: 15_000, staleTime: 0 } });
  const datasets = useListDatasets({ query: { refetchInterval: 30_000, staleTime: 0 } });
  const runs = useListRuns({ limit: 60 }, { query: { refetchInterval: 10_000, staleTime: 0 } });
  const schedule = useGetSchedule({ query: { refetchInterval: 30_000, staleTime: 0 } });
  const [manual, setManual] = useState(false);
  const titles = useMemo(() => new Map((datasets.data ?? []).map((d) => [d.dataset_id, d.title])), [datasets.data]);

  return (
    <>
      <PageHeader
        title="数据同步"
        desc="股票和 ETF 的行情都按下面的同步计划自动同步，时间都是上海时间。每天同步后自动核对最近 20 个交易日的数据是否齐全，能补的自动补。"
        extra={
          <div className="flex flex-wrap items-center gap-2">
            <SyncNow
              label="立即同步全部"
              datasets={[]}
              hint="把所有已开启的数据都增量同步一次：从上次同步到的位置往后取；已经在队列里的不会重复"
            />
            <button type="button" className={buttonClass("ghost")} onClick={() => setManual((v) => !v)}>
              {manual ? "收起" : "手动同步…"}
            </button>
            <Link to="/indices" className="text-sm text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-100">
              指数与板块 →
            </Link>
          </div>
        }
      />
      {manual && datasets.data && <ManualSync datasets={datasets.data} onDone={() => setManual(false)} />}
      {health.isPending ? <Spinner /> : health.data && <Status h={health.data} />}
      {datasets.data && <AssetSyncSummary rows={datasets.data} />}
      <QueryView query={schedule}>{(s) => <Plans s={s} titles={titles} />}</QueryView>
      <DataCheck />
      <Card title="同步记录" extra={<span className="text-xs text-zinc-400">每 10 秒刷新，点一行看详情</span>}>
        <QueryView query={runs}>{(rows) => <History rows={rows} />}</QueryView>
      </Card>
      {health.data && (health.data.gaps.length > 0 || health.data.failures.length > 0) && <Problems h={health.data} />}
      <Card
        title="数据"
        extra={<span className="text-xs text-zinc-400">按分类，每类数据是什么、属于哪个同步计划、现在怎么样</span>}
      >
        <QueryView query={datasets}>{(rows) => <Datasets rows={rows} />}</QueryView>
      </Card>
    </>
  );
}

/** 增量同步一批数据集，规划好后跳到这次同步的详情。datasets 为空时同步全部已开启的数据集 */
function SyncNow({
  label,
  datasets,
  hint,
  ghost,
  disabled,
}: {
  label: string;
  datasets: string[];
  hint: string;
  ghost?: boolean;
  disabled?: boolean;
}) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const create = useCreateRun({
    mutation: {
      onSuccess: (r) => {
        qc.invalidateQueries({ queryKey: getListRunsQueryKey() });
        navigate(`/sync/runs/${r.run_id}`);
      },
    },
  });
  return (
    <>
      <button
        type="button"
        className={buttonClass(ghost ? "ghost" : undefined)}
        disabled={disabled || create.isPending}
        title={hint}
        onClick={() => create.mutate({ data: { kind: "incremental", datasets, priorities: [], thscodes: [] } })}
      >
        {create.isPending ? "规划中…" : label}
      </button>
      {!!create.error && <span className="text-sm text-red-600">{errorText(create.error)}</span>}
    </>
  );
}

const PLAN_HINTS: Record<string, string> = {
  daily: "收盘后当天就有的数据：股票和 ETF 日 K 线快照、指数行情、估值、异动原因、公司行动等",
  weekly:
    "变化不频繁的基础数据：交易日历、股票和 ETF 代码表、ETF 历史日线、指数成分、财务报表，以及按股票重新核对一遍日 K 线",
  monthly: "更新很慢的数据，跟每月第一次每周同步一起",
  report_season: "财报密集的月份每天取，其余月份每周取",
};

/** 同步计划：每个计划什么时候同步、包括什么、下次和上次 */
function Plans({ s, titles }: { s: SyncSchedule; titles: Map<string, string> }) {
  const [editing, setEditing] = useState(false);
  const auto = s.plans.filter((p) => p.key !== "manual");
  const manual = s.plans.find((p) => p.key === "manual");
  return (
    <Card
      title="同步计划"
      extra={
        <button type="button" className={buttonClass("ghost")} onClick={() => setEditing((v) => !v)}>
          {editing ? "收起" : "修改时间"}
        </button>
      }
    >
      <div className="flex flex-col gap-3">
        {editing && <EditTimes times={s.times} onDone={() => setEditing(false)} />}
        <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
          {auto.map((p) => (
            <PlanTile key={p.key} p={p} titles={titles} />
          ))}
        </div>
        {manual && manual.datasets.length > 0 && (
          <p className="text-xs text-zinc-500">
            不自动同步：{manual.datasets.map((id) => titles.get(id) ?? id).join("、")}，需要时用「手动同步」。
          </p>
        )}
        <p className="text-xs text-zinc-400">
          股票和 ETF 行情默认开启；其他基金、期货、期权等数据可在「手动同步」里单独同步。
        </p>
      </div>
    </Card>
  );
}

function PlanTile({ p, titles }: { p: SyncPlan; titles: Map<string, string> }) {
  const [open, setOpen] = useState(false);
  const st = p.last_status ? (RUN_STATUS[p.last_status] ?? { label: p.last_status, tone: "muted" as const }) : null;
  const names = p.datasets.map((id) => titles.get(id) ?? id);
  return (
    <section className="flex flex-col gap-2 rounded-lg border border-zinc-200 p-3 text-sm dark:border-zinc-800">
      <div>
        <h3 className="font-medium">{p.name}</h3>
        <p className="text-accent">{p.rule}</p>
        <p className="mt-1 text-xs text-zinc-500">{PLAN_HINTS[p.key]}</p>
      </div>
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
        <dt className="text-zinc-400">下次</dt>
        <dd className="num">{p.datasets.length > 0 ? fmtSlot(p.next_at) : "—"}</dd>
        <dt className="text-zinc-400">上次</dt>
        <dd className="flex flex-wrap items-center gap-1">
          {p.last_at && p.datasets.length > 0 ? (
            <>
              <span className="num">{fmtSlot(p.last_at)}</span>
              {st && p.last_run_id && (
                <Link to={`/sync/runs/${p.last_run_id}`}>
                  <Badge tone={st.tone}>{st.label}</Badge>
                </Link>
              )}
            </>
          ) : (
            <span className="text-zinc-400">{p.key === "report_season" ? "随每日、每周同步" : "还没有"}</span>
          )}
        </dd>
      </dl>
      <div className="mt-auto flex flex-wrap items-center justify-between gap-2">
        <button
          type="button"
          className="text-xs text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-100"
          onClick={() => setOpen((v) => !v)}
          disabled={names.length === 0}
        >
          {names.length === 0 ? "没有开启的数据" : `${open ? "▾" : "▸"} ${names.length} 类数据`}
        </button>
        {names.length > 0 && (
          <SyncNow
            label="现在同步一次"
            datasets={p.datasets}
            ghost
            hint={`不等到点，现在就把「${p.name}」的数据增量同步一次`}
          />
        )}
      </div>
      {open && <p className="text-xs leading-relaxed text-zinc-600 dark:text-zinc-400">{names.join("、")}</p>}
    </section>
  );
}

/** 修改每日和每周的时间（上海时间）；每月、财报季跟着它们走 */
function EditTimes({ times, onDone }: { times: ScheduleTimes; onDone: () => void }) {
  const [t, setT] = useState(times);
  const qc = useQueryClient();
  const save = useUpdateSchedule({
    mutation: {
      onSuccess: (r) => {
        qc.setQueryData(getGetScheduleQueryKey(), r);
        onDone();
      },
    },
  });
  return (
    <form
      className="flex flex-col gap-3 rounded-lg bg-zinc-50 p-3 dark:bg-zinc-800/50"
      onSubmit={(e) => {
        e.preventDefault();
        save.mutate({ data: t });
      }}
    >
      <div className="flex flex-wrap items-end gap-6">
        <Field
          label="每日收盘后：每个交易日的"
          hint="A 股 15:00 收盘，供应商的当天数据一般在收盘后一两个小时内出齐；到点后还没出的会自动等"
        >
          <input
            type="time"
            className={`${inputClass} w-32`}
            value={t.daily_time}
            onChange={(e) => setT({ ...t, daily_time: e.target.value })}
            required
          />
        </Field>
        <Field label="每周：每周的哪天、几点" hint="建议放在周末，不和每日同步抢请求额度">
          <div className="flex gap-2">
            <select
              className={inputClass}
              value={t.weekly_weekday}
              onChange={(e) => setT({ ...t, weekly_weekday: Number(e.target.value) })}
            >
              {WEEKDAYS.map((w, i) => (
                <option key={w} value={i + 1}>
                  {w}
                </option>
              ))}
            </select>
            <input
              type="time"
              className={`${inputClass} w-32`}
              value={t.weekly_time}
              onChange={(e) => setT({ ...t, weekly_time: e.target.value })}
              required
            />
          </div>
        </Field>
      </div>
      <p className="text-xs text-zinc-500">
        时间是上海时间，与服务器在哪里无关。保存后 30
        秒内生效；每月的数据跟每月第一次每周同步一起，财报季的数据跟每日或每周同步一起。
      </p>
      {!!save.error && <p className="text-sm text-red-600">{errorText(save.error)}</p>}
      <div className="flex gap-2">
        <button type="submit" className={buttonClass()} disabled={save.isPending}>
          {save.isPending ? "保存中…" : "保存"}
        </button>
        <button type="button" className={buttonClass("ghost")} onClick={onDone}>
          取消
        </button>
      </div>
    </form>
  );
}

/** 顶部一句话：最近一个交易日的数据是否就绪，以及历史回补的进度 */
function Status({ h }: { h: SyncHealth }) {
  const tone = {
    ready: "bg-emerald-500",
    running: "bg-accent animate-pulse",
    incomplete: "bg-amber-500",
    not_started: "bg-zinc-400",
  }[h.daily_state];
  const b = h.backfill;
  return (
    <Card>
      <div className="flex flex-col gap-2 text-sm">
        <p className="flex items-center gap-2 text-base font-medium">
          <span className={clsx("size-2.5 rounded-full", tone)} />
          {h.message}
        </p>
        {b && b.total > 0 && (
          <div className="flex flex-col gap-1">
            <p className="text-zinc-600 dark:text-zinc-400">
              正在补历史数据：{fmtInt(b.done)} / {fmtInt(b.total)}（{((b.done / b.total) * 100).toFixed(0)}%）
              {b.eta_hours !== null &&
                b.eta_hours !== undefined &&
                `，按现在的速度还要约 ${Math.ceil(b.eta_hours)} 小时`}
              。补历史排在每日数据后面，不影响每天的同步。
            </p>
            <div className="max-w-md">
              <ProgressBar done={b.done} failed={0} total={b.total} />
            </div>
          </div>
        )}
        {/* 限速会因为遇到 429 自动降低、连续成功后再回升；两者不等时说明正在被供应商限流 */}
        <p className="text-xs text-zinc-400">
          接口限速：每秒 {h.rate.current} 次
          {h.rate.current < h.rate.ceiling && (
            <span className="text-amber-600 dark:text-amber-500">
              （上限 {h.rate.ceiling} 次，遇到限流已自动降低，恢复后会自己升回去）
            </span>
          )}
        </p>
        {h.last_repair_at && (
          <p className="text-xs text-zinc-400">上次自动核对和补齐：{fmtDateTime(h.last_repair_at)}</p>
        )}
      </div>
    </Card>
  );
}

/** 缺口和失败：能补的一键补，补不回来的如实说明 */
function Problems({ h }: { h: SyncHealth }) {
  const qc = useQueryClient();
  const repair = useSyncRepair({ mutation: { onSuccess: () => qc.invalidateQueries() } });
  const recoverable = h.gaps.filter((g) => g.recoverable);
  const lost = h.gaps.filter((g) => !g.recoverable);
  return (
    <Card
      title="需要注意"
      extra={
        <button
          type="button"
          className={buttonClass()}
          disabled={repair.isPending}
          title="能补的缺口安排补偿同步，供应商暂时不可用的失败再试一次；本来就没有数据的失败（节假日、超出一年、证券已退市）改为跳过"
          onClick={() => repair.mutate()}
        >
          {repair.isPending ? "处理中…" : "立即补齐"}
        </button>
      }
    >
      <div className="flex flex-col gap-4 text-sm">
        {repair.data && (
          <p className="text-emerald-700 dark:text-emerald-400">
            已安排 {repair.data.runs.length} 个补偿同步，重新排队 {repair.data.retried}{" "}
            个失败的任务，会排在每日数据之后执行
            {repair.data.cleared > 0 && `；${repair.data.cleared} 个本来就没有数据的失败任务已改为跳过`}。
          </p>
        )}
        {!!repair.error && <p className="text-red-600">{errorText(repair.error)}</p>}
        {recoverable.length > 0 && (
          <GapList title="缺少的数据（可以补，每天同步后会自动补）" gaps={recoverable} tone="warn" />
        )}
        {lost.length > 0 && <GapList title="补不回来的数据（只有当天的值，错过了就没有了）" gaps={lost} tone="bad" />}
        {h.failures.length > 0 && (
          <div>
            <h3 className="mb-1 text-xs font-medium text-zinc-500">失败的任务</h3>
            <ul className="flex flex-col gap-1">
              {h.failures.map((f) => (
                <li key={`${f.dataset_id}-${f.sample_error}`} className="flex flex-wrap items-baseline gap-x-2">
                  <span className="font-medium">{f.title}</span>
                  <span className="text-zinc-500">
                    {fmtInt(f.tasks)} 个：{f.reason}
                  </span>
                  <span className="truncate text-xs text-zinc-400" title={f.sample_error}>
                    {f.sample_error}
                  </span>
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
    </Card>
  );
}

function GapList({ title, gaps, tone }: { title: string; gaps: Gap[]; tone: "warn" | "bad" }) {
  return (
    <div>
      <h3
        className={clsx(
          "mb-1 text-xs font-medium",
          tone === "bad" ? "text-red-600 dark:text-red-400" : "text-amber-700 dark:text-amber-400",
        )}
      >
        {title}
      </h3>
      <ul className="flex flex-col gap-1">
        {gaps.slice(0, 20).map((g) => (
          <li key={`${g.trade_date}-${g.dataset_id}-${g.kind}`} className="flex flex-wrap items-baseline gap-x-2">
            <span className="num text-zinc-500">{g.trade_date}</span>
            <span className="font-medium">{g.title}</span>
            <span className="text-xs text-zinc-500">{g.message}</span>
          </li>
        ))}
        {gaps.length > 20 && <li className="text-xs text-zinc-400">还有 {gaps.length - 20} 项</li>}
      </ul>
    </div>
  );
}

/** 一个数据集的状态，用一个词说清 */
function datasetState(d: DatasetStatus): {
  label: string;
  tone: "ok" | "warn" | "bad" | "info" | "muted";
  hint?: string;
} {
  if (!d.enabled) return { label: "未开启", tone: "muted" };
  if (d.dead_tasks > 0) return { label: `${fmtInt(d.dead_tasks)} 个失败`, tone: "bad", hint: "见上方「需要注意」" };
  if (d.backfill_total > 0 && d.backfill_done < d.backfill_total)
    return {
      label: `补历史 ${Math.floor((d.backfill_done / d.backfill_total) * 100)}%`,
      tone: "info",
      hint: `${fmtInt(d.backfill_done)} / ${fmtInt(d.backfill_total)}`,
    };
  if (d.open_tasks > 0) return { label: "同步中", tone: "info" };
  if (!d.last_done_at) return { label: "还没同步过", tone: "warn" };
  return { label: "正常", tone: "ok" };
}

/** 按分类分组，分类的先后由后端按 datasets.yaml 给出（category_order） */
function groupByCategory(list: DatasetStatus[]): [string, DatasetStatus[]][] {
  const m = new Map<string, DatasetStatus[]>();
  for (const d of [...list].sort((a, b) => a.category_order - b.category_order)) {
    m.set(d.category, [...(m.get(d.category) ?? []), d]);
  }
  return [...m];
}

/** ETF 的行情和资料（后端标记 etf_market 的数据集）：页面、因子和建议都用到，应保持开启 */
function AssetSyncSummary({ rows }: { rows: DatasetStatus[] }) {
  const etf = rows.filter((d) => d.etf_market);
  if (etf.length === 0) return null;
  return (
    <Card title="股票与 ETF 行情同步" extra={<span className="text-xs text-zinc-400">两类行情都应保持开启</span>}>
      <div className="grid gap-3 sm:grid-cols-3">
        {etf.map((d) => {
          const st = datasetState(d);
          return (
            <div key={d.dataset_id} className="rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
              <div className="flex items-center justify-between gap-2">
                <span className="font-medium">{d.title}</span>
                <Badge tone={st.tone}>{st.label}</Badge>
              </div>
              <p className="mt-1 text-xs text-zinc-500">
                {d.schedule === "daily" ? "每日" : "每周"} · 最近完成{" "}
                {d.last_done_at ? fmtAgo(d.last_done_at) : "还没有"}
              </p>
              {d.open_tasks > 0 && <p className="mt-1 text-xs text-blue-600">进行中任务：{fmtInt(d.open_tasks)}</p>}
              {d.dead_tasks > 0 && <p className="mt-1 text-xs text-red-600">未解决失败：{fmtInt(d.dead_tasks)}</p>}
            </div>
          );
        })}
      </div>
    </Card>
  );
}

/** 按分类分组；没有开启的数据集收在最后 */
function Datasets({ rows }: { rows: DatasetStatus[] }) {
  const [showOff, setShowOff] = useState(false);
  const on = useMemo(() => groupByCategory(rows.filter((d) => d.enabled)), [rows]);
  const off = useMemo(() => groupByCategory(rows.filter((d) => !d.enabled)), [rows]);
  const offCount = rows.filter((d) => !d.enabled).length;
  return (
    <div className="flex flex-col gap-5">
      {on.map(([cat, list]) => (
        <Category key={cat} name={cat} rows={list} />
      ))}
      {offCount > 0 && (
        <div>
          <button
            type="button"
            className="text-sm text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-100"
            onClick={() => setShowOff((v) => !v)}
          >
            {showOff ? "▾" : "▸"} 没有开启的数据（{offCount} 个：其他基金、期货、期权等，开启方法见数据目录文档）
          </button>
          {showOff && (
            <div className="mt-3 flex flex-col gap-5 opacity-70">
              {off.map(([cat, list]) => (
                <Category key={cat} name={cat} rows={list} />
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function Category({ name, rows }: { name: string; rows: DatasetStatus[] }) {
  return (
    <section>
      <h3 className="mb-1 text-xs font-medium text-zinc-500">
        {name} <span className="text-zinc-400">{rows.length}</span>
      </h3>
      <ul className="divide-y divide-zinc-100 rounded-lg border border-zinc-100 dark:divide-zinc-800 dark:border-zinc-800">
        {rows.map((d) => {
          const st = datasetState(d);
          return (
            <li key={d.dataset_id} className="flex flex-col gap-1 px-3 py-2 sm:flex-row sm:items-center sm:gap-4">
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium" title={d.dataset_id}>
                    {d.title}
                  </span>
                  {d.only_current && (
                    <span
                      className="text-xs text-amber-700 dark:text-amber-400"
                      title="接口只返回当天的值，没有历史，错过当天就补不回来"
                    >
                      只有当天值
                    </span>
                  )}
                </div>
                <p className="text-xs text-zinc-500">{d.summary}</p>
              </div>
              <div className="flex shrink-0 items-center gap-3 text-xs text-zinc-500">
                <span className="w-24">{PLANS[d.schedule] ?? d.schedule}</span>
                <span className="w-20" title={fmtDateTime(d.last_done_at)}>
                  {d.last_done_at ? fmtAgo(d.last_done_at) : "—"}
                </span>
                <span className="w-24 text-right" title={st.hint}>
                  <Badge tone={st.tone}>{st.label}</Badge>
                </span>
              </div>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

/** 同步记录：一次同步可能分几个波次（有依赖的数据集要等前一批完成），这里合并成一行 */
function History({ rows }: { rows: RunSummary[] }) {
  const navigate = useNavigate();
  const chains = useMemo(() => groupChains(rows).slice(0, 15), [rows]);
  if (chains.length === 0) return <Empty>还没有同步过</Empty>;
  return (
    <Table>
      <thead>
        <tr>
          <th>来源</th>
          <th>交易日</th>
          <th>状态</th>
          <th className="w-56">进度</th>
          <th>开始</th>
          <th>耗时</th>
        </tr>
      </thead>
      <tbody>
        {chains.map((c) => {
          const st = RUN_STATUS[c.status] ?? { label: c.status, tone: "muted" as const };
          return (
            <tr
              key={c.root}
              className="cursor-pointer hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
              onClick={() => navigate(`/sync/runs/${c.root}`)}
            >
              <td>
                <Link to={`/sync/runs/${c.root}`} className="hover:underline">
                  {runSource(c.first.kind, c.first.params)}
                </Link>
                {c.waves > 1 && <span className="ml-1 text-xs text-zinc-400">{c.waves} 批</span>}
              </td>
              <td className="num">{c.first.trade_date ?? "—"}</td>
              <td>
                <Badge tone={st.tone}>{st.label}</Badge>
              </td>
              <td>
                <ChainProgress c={c} />
              </td>
              <td className="num text-xs text-zinc-500">{fmtDateTime(c.first.started_at)}</td>
              <td className="num text-xs text-zinc-500">
                {c.status === "running" ? "进行中" : fmtDuration(c.first.started_at, c.finishedAt)}
              </td>
            </tr>
          );
        })}
      </tbody>
    </Table>
  );
}

const MANUAL_GROUPS = ["daily", "weekly", "monthly", "report_season", "manual"];

/** 手动同步：按同步计划勾选（可以全选），每组能展开到单个数据集；没有开启的数据单独一组 */
function ManualSync({ datasets, onDone }: { datasets: DatasetStatus[]; onDone: () => void }) {
  const [kind, setKind] = useState<"incremental" | "backfill">("incremental");
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [open, setOpen] = useState<Set<string>>(new Set());
  const [codes, setCodes] = useState("");
  const navigate = useNavigate();
  const qc = useQueryClient();
  const create = useCreateRun({
    mutation: {
      onSuccess: (r) => {
        qc.invalidateQueries({ queryKey: getListRunsQueryKey() });
        onDone();
        navigate(`/sync/runs/${r.run_id}`);
      },
    },
  });
  const thscodes = codes
    .split(/[\s,，、]+/)
    .map((c) => c.trim().toUpperCase())
    .filter(Boolean);
  const groups = useMemo(() => {
    const on = datasets.filter((d) => d.enabled);
    const out: { key: string; name: string; rows: DatasetStatus[] }[] = MANUAL_GROUPS.map((k) => ({
      key: k,
      name: PLANS[k] ?? k,
      rows: on.filter((d) => d.schedule === k),
    })).filter((g) => g.rows.length > 0);
    const off = datasets.filter((d) => !d.enabled);
    if (off.length > 0) out.push({ key: "off", name: "没有开启的数据（其他基金、期货、期权等）", rows: off });
    return out;
  }, [datasets]);
  const enabledIds = datasets.filter((d) => d.enabled).map((d) => d.dataset_id);
  const allOn = enabledIds.length > 0 && enabledIds.every((id) => picked.has(id));
  const setMany = (ids: string[], on: boolean) => {
    const next = new Set(picked);
    for (const id of ids) on ? next.add(id) : next.delete(id);
    setPicked(next);
  };
  const toggleOpen = (k: string) => {
    const next = new Set(open);
    next.has(k) ? next.delete(k) : next.add(k);
    setOpen(next);
  };

  return (
    <Card title="手动同步">
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          create.mutate({ data: { kind, datasets: [...picked], priorities: [], thscodes } });
        }}
      >
        <Field label="怎么同步">
          <div className="self-start">
            <Segmented
              value={kind}
              onChange={setKind}
              options={[
                { value: "incremental", label: "增量", hint: "从上次同步到的位置往后取，平时用这个" },
                { value: "backfill", label: "全部重取", hint: "从数据集配置的起点全部重取，任务量很大" },
              ]}
            />
          </div>
        </Field>
        <div className="flex flex-col gap-2">
          <div className="flex flex-wrap items-center gap-3 text-sm">
            <label className="flex items-center gap-1.5 font-medium">
              <input type="checkbox" checked={allOn} onChange={() => setMany(enabledIds, !allOn)} />
              全选已开启的数据（{enabledIds.length} 类）
            </label>
            {picked.size > 0 && (
              <button
                type="button"
                className="text-xs text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-100"
                onClick={() => setPicked(new Set())}
              >
                清空
              </button>
            )}
          </div>
          <div className="flex flex-col divide-y divide-zinc-100 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-700">
            {groups.map((g) => {
              const ids = g.rows.map((d) => d.dataset_id);
              const n = ids.filter((id) => picked.has(id)).length;
              return (
                <div key={g.key} className="px-3 py-2">
                  <div className="flex items-center gap-2 text-sm">
                    <input
                      type="checkbox"
                      aria-label={g.name}
                      checked={n === ids.length}
                      ref={(el) => {
                        if (el) el.indeterminate = n > 0 && n < ids.length;
                      }}
                      onChange={() => setMany(ids, n < ids.length)}
                    />
                    <button type="button" className="flex-1 text-left" onClick={() => toggleOpen(g.key)}>
                      {open.has(g.key) ? "▾" : "▸"} {g.name}
                      <span className="ml-1 text-xs text-zinc-400">
                        {n > 0 ? `已选 ${n} / ${ids.length}` : `${ids.length} 类`}
                      </span>
                    </button>
                  </div>
                  {open.has(g.key) && (
                    <div className="mt-2 grid gap-1 pl-6 sm:grid-cols-2 lg:grid-cols-3">
                      {g.rows.map((d) => (
                        <label key={d.dataset_id} className="flex items-center gap-1.5 text-xs" title={d.summary}>
                          <input
                            type="checkbox"
                            checked={picked.has(d.dataset_id)}
                            onChange={() => setMany([d.dataset_id], !picked.has(d.dataset_id))}
                          />
                          {d.title}
                          <span className="text-zinc-400">{d.category}</span>
                        </label>
                      ))}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        </div>
        <Field
          label="只同步这些证券（可选）"
          hint="作用于按证券展开的数据集，例如重新拉取个别股票或 ETF；多个代码用空格或逗号分隔"
        >
          <input
            className={`${inputClass} max-w-md`}
            placeholder="600519.SH 510300.SH"
            value={codes}
            onChange={(e) => setCodes(e.target.value)}
          />
        </Field>
        {kind === "backfill" && thscodes.length === 0 && picked.size > 0 && (
          <p className="rounded-lg bg-amber-50 p-3 text-sm text-amber-800 dark:bg-amber-500/10 dark:text-amber-300">
            全市场全部重取可能产生数万到数十万个任务，要执行一两天，排在每日数据后面。已在队列中的同一分区不会重复创建。
          </p>
        )}
        {!!create.error && <p className="text-sm text-red-600 dark:text-red-400">{errorText(create.error)}</p>}
        <div className="flex items-center gap-2">
          <button type="submit" className={buttonClass()} disabled={picked.size === 0 || create.isPending}>
            {create.isPending ? "规划中…" : `开始同步（${picked.size} 类）`}
          </button>
          <button type="button" className={buttonClass("ghost")} onClick={onDone}>
            取消
          </button>
        </div>
      </form>
    </Card>
  );
}
