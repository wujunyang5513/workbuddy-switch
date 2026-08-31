import { useCallback, useEffect, useMemo, useState } from "react";
import { toast } from "sonner";
import {
  CircleAlert,
  Clock3,
  History,
  ListChecks,
  Loader2,
  MapPin,
  PackageCheck,
  PackageOpen,
  PlaneTakeoff,
  RefreshCw,
  Save,
} from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Separator } from "@/components/ui/separator";
import { Switch } from "@/components/ui/switch";
import { Label } from "@/components/ui/label";
import * as api from "@/lib/api";
import type {
  TravelAutoConfig,
  TravelBatchResult,
  TravelLog,
  TravelState,
  TravelStatus,
} from "@/lib/types";
import { useAccountsStore } from "@/stores/accounts";

function formatDateTime(ts: number | null | undefined): string {
  if (ts === null || ts === undefined || !Number.isFinite(ts)) return "—";
  return new Date(ts).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function stateLabel(state: string | undefined): string {
  switch (state) {
    case "idle":
      return "空闲";
    case "traveling":
      return "旅行中";
    case "arrived":
      return "已到达";
    case "finished":
    case "done":
      return "已完成";
    default:
      return state || "未知";
  }
}

const STATE_COLORS: Record<string, string> = {
  idle: "bg-emerald-500/15 text-emerald-600 border-emerald-500/30",
  traveling: "bg-sky-500/15 text-sky-600 border-sky-500/30",
  arrived: "bg-amber-500/15 text-amber-600 border-amber-500/30",
  finished: "bg-violet-500/15 text-violet-600 border-violet-500/30",
  done: "bg-violet-500/15 text-violet-600 border-violet-500/30",
};

const KIND_LABEL: Record<string, string> = {
  depart: "派遣全部",
  claim: "领取全部",
};

const TRIGGER_LABEL: Record<string, string> = {
  manual: "手动",
  auto: "自动",
};

export default function TravelPage() {
  const accounts = useAccountsStore((s) => s.accounts);
  const [accountId, setAccountId] = useState<string>("");
  const [status, setStatus] = useState<TravelStatus | null>(null);
  const [loading, setLoading] = useState(false);
  const [actionLoading, setActionLoading] = useState<"depart" | "claim" | null>(null);
  const [batchLoading, setBatchLoading] = useState<"depart" | "claim" | null>(null);
  const [batchResult, setBatchResult] = useState<TravelBatchResult | null>(null);
  const [autoConfig, setAutoConfig] = useState<TravelAutoConfig | null>(null);
  const [configLoading, setConfigLoading] = useState(false);
  const [logs, setLogs] = useState<TravelLog[]>([]);
  const [logsLoading, setLogsLoading] = useState(false);

  const load = useCallback(
    async (id: string, silent = false) => {
      if (!id) return;
      if (!silent) setLoading(true);
      try {
        const res = await api.getTravelStatus(id);
        setStatus(res);
      } catch (e) {
        if (!silent) toast.error(`查询旅行状态失败：${api.asError(e)}`);
        setStatus(null);
      } finally {
        if (!silent) setLoading(false);
      }
    },
    [],
  );

  const loadAutoConfig = useCallback(async () => {
    try {
      const cfg = await api.getTravelAutoConfig();
      setAutoConfig(cfg);
    } catch (e) {
      toast.error(`读取自动执行配置失败：${api.asError(e)}`);
    }
  }, []);

  const loadLogs = useCallback(async () => {
    setLogsLoading(true);
    try {
      const res = await api.getTravelLogs();
      setLogs(res.logs ?? []);
    } catch (e) {
      toast.error(`读取旅行日志失败：${api.asError(e)}`);
    } finally {
      setLogsLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadAutoConfig();
    void loadLogs();
  }, [loadAutoConfig, loadLogs]);

  // 账号列表变化时，默认选中第一个并查询。
  useEffect(() => {
    if (accounts.length === 0) return;
    if (!accountId || !accounts.some((a) => a.id === accountId)) {
      const first = accounts[0].id;
      setAccountId(first);
      void load(first);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [accounts, accountId]);

  const selectAccount = (id: string) => {
    setAccountId(id);
    void load(id);
  };

  const doDepart = async () => {
    if (!accountId) return;
    setActionLoading("depart");
    try {
      const res = await api.departTravel(accountId, 0);
      if (res.ok) {
        toast.success(`已派遣旅行${res.locationName ? `到「${res.locationName}」` : ""}`);
      } else if (res.skipped) {
        toast.info(res.reason || "当前无需派遣");
      } else {
        toast.error(res.error || "派遣失败");
      }
      await load(accountId, true);
    } catch (e) {
      toast.error(`派遣失败：${api.asError(e)}`);
    } finally {
      setActionLoading(null);
    }
  };

  const doClaim = async () => {
    if (!accountId) return;
    setActionLoading("claim");
    try {
      const res = await api.claimTravel(accountId);
      if (res.ok) {
        const credit = res.rewardCredit ?? 0;
        toast.success(`已领取旅行奖励${credit > 0 ? `，获得 ${credit} 积分` : ""}`);
      } else if (res.skipped) {
        toast.info(res.reason || "当前无可领取的旅行奖励");
      } else {
        toast.error(res.error || "领取失败");
      }
      await load(accountId, true);
    } catch (e) {
      toast.error(`领取失败：${api.asError(e)}`);
    } finally {
      setActionLoading(null);
    }
  };

  const doDepartAll = async () => {
    setBatchLoading("depart");
    setBatchResult(null);
    try {
      const res = await api.departAllTravels(0);
      setBatchResult(res);
      if (res.ok > 0) toast.success(`一键派遣完成：成功 ${res.ok}，跳过 ${res.skipped}，失败 ${res.failed}`);
      else if (res.skipped > 0) toast.info(`无可派遣账号（跳过 ${res.skipped}）`);
      else toast.error(`一键派遣失败（失败 ${res.failed}）`);
      if (accountId) await load(accountId, true);
    } catch (e) {
      toast.error(`一键派遣失败：${api.asError(e)}`);
    } finally {
      setBatchLoading(null);
      void loadLogs();
    }
  };

  const doClaimAll = async () => {
    setBatchLoading("claim");
    setBatchResult(null);
    try {
      const res = await api.claimAllTravels();
      setBatchResult(res);
      if (res.ok > 0) toast.success(`一键领取完成：成功 ${res.ok}，跳过 ${res.skipped}，失败 ${res.failed}`);
      else if (res.skipped > 0) toast.info(`无可领取奖励（跳过 ${res.skipped}）`);
      else toast.error(`一键领取失败（失败 ${res.failed}）`);
      if (accountId) await load(accountId, true);
    } catch (e) {
      toast.error(`一键领取失败：${api.asError(e)}`);
    } finally {
      setBatchLoading(null);
      void loadLogs();
    }
  };

  const saveAutoConfig = async () => {
    if (!autoConfig) return;
    setConfigLoading(true);
    try {
      const saved = await api.saveTravelAutoConfig(autoConfig);
      setAutoConfig(saved);
      toast.success("已保存自动执行配置");
    } catch (e) {
      toast.error(`保存自动执行配置失败：${api.asError(e)}`);
    } finally {
      setConfigLoading(false);
    }
  };

  const updateConfig = (patch: Partial<TravelAutoConfig>) => {
    setAutoConfig((prev) => (prev ? { ...prev, ...patch } : prev));
  };

  const t: TravelState | null = status?.travel ?? null;
  const reversedLogs = useMemo(() => [...logs].reverse(), [logs]);

  return (
    <div className="mx-auto flex max-w-3xl flex-col gap-6 p-6">
      <header className="flex flex-col gap-1">
        <h1 className="text-xl font-semibold tracking-[-0.01em]">猫猫旅行</h1>
        <p className="text-sm text-muted-foreground">
          派遣 Buddy 出门旅行，到达后领取旅行奖励积分；支持一键批量操作与每日自动执行。
        </p>
      </header>

      {/* 一键批量操作 */}
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2 text-base">
            <ListChecks className="size-4 text-muted-foreground" />
            一键批量操作
          </CardTitle>
          <CardDescription>
            对所有账号批量执行：一键派遣全部可派遣账号、一键领取全部可领取奖励
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-4">
          <div className="flex flex-wrap items-center gap-3">
            <Button
              onClick={doDepartAll}
              disabled={batchLoading !== null || accounts.length === 0}
            >
              {batchLoading === "depart" ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <PlaneTakeoff className="size-4" />
              )}
              {batchLoading === "depart" ? "派遣全部中…" : "一键派遣全部"}
            </Button>
            <Button
              variant="secondary"
              onClick={doClaimAll}
              disabled={batchLoading !== null || accounts.length === 0}
            >
              {batchLoading === "claim" ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <PackageCheck className="size-4" />
              )}
              {batchLoading === "claim" ? "领取全部中…" : "一键领取全部"}
            </Button>
            {accounts.length === 0 && (
              <span className="text-xs text-muted-foreground">暂无账号，先到「账号管理」添加</span>
            )}
          </div>

          {batchResult && (
            <div className="flex flex-col gap-3 rounded-lg border p-4">
              <div className="flex flex-wrap items-center gap-2 text-sm">
                <Badge variant="outline" className="px-2 py-0 text-xs">
                  {KIND_LABEL[batchResult.kind] ?? batchResult.kind}
                </Badge>
                <span className="font-medium">
                  成功 <span className="text-emerald-600">{batchResult.ok}</span>
                </span>
                <span>
                  跳过 <span className="text-amber-600">{batchResult.skipped}</span>
                </span>
                <span>
                  失败 <span className="text-destructive">{batchResult.failed}</span>
                </span>
                <span className="text-xs text-muted-foreground">共 {batchResult.total} 个账号</span>
              </div>
              {batchResult.accounts.length > 0 && (
                <div className="flex max-h-48 flex-col gap-1 overflow-y-auto rounded-md bg-muted/40 p-2 text-xs">
                  {batchResult.accounts.map((r, idx) => (
                    <div key={idx} className="flex items-start gap-2">
                      <span
                        className={`shrink-0 font-medium ${
                          r.ok
                            ? "text-emerald-600"
                            : r.skipped
                              ? "text-muted-foreground"
                              : "text-destructive"
                        }`}
                      >
                        {r.ok ? "✓" : r.skipped ? "–" : "✗"}
                      </span>
                      <span className="shrink-0 font-medium">{r.email || "未知账号"}</span>
                      <span className="text-muted-foreground">
                        {r.ok
                          ? r.locationName
                            ? `已派遣 → ${r.locationName}`
                            : r.rewardCredit != null
                              ? `已领取 ${r.rewardCredit} 积分`
                              : "成功"
                          : r.reason || r.error || "失败"}
                      </span>
                    </div>
                  ))}
                </div>
              )}
            </div>
          )}
        </CardContent>
      </Card>

      {/* 自动执行 */}
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2 text-base">
            <Clock3 className="size-4 text-muted-foreground" />
            每日自动执行
          </CardTitle>
          <CardDescription>
            到点自动运行「一键派遣全部 / 一键领取全部」，执行结果写入日志
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-4">
          {!autoConfig ? (
            <span className="text-sm text-muted-foreground">加载配置中…</span>
          ) : (
            <>
              <div className="flex items-center justify-between gap-4 rounded-lg border p-4">
                <div>
                  <p className="text-sm font-medium">启用自动执行</p>
                  <p className="text-xs text-muted-foreground">
                    关闭后仅保留手动按钮，每日自动执行暂停
                  </p>
                </div>
                <Switch
                  checked={autoConfig.enabled}
                  onCheckedChange={(v) => updateConfig({ enabled: v })}
                  aria-label="启用自动执行"
                />
              </div>

              <div className="grid grid-cols-2 gap-4">
                <div className="flex flex-col gap-1.5">
                  <Label htmlFor="depart-time">每日派遣时间</Label>
                  <Input
                    id="depart-time"
                    type="time"
                    value={autoConfig.depart_time}
                    onChange={(e) => updateConfig({ depart_time: e.target.value })}
                  />
                  <p className="text-xs text-muted-foreground">每天到点自动派出所有可派遣账号</p>
                </div>
                <div className="flex flex-col gap-1.5">
                  <Label htmlFor="claim-time">每日领取时间</Label>
                  <Input
                    id="claim-time"
                    type="time"
                    value={autoConfig.claim_time}
                    onChange={(e) => updateConfig({ claim_time: e.target.value })}
                  />
                  <p className="text-xs text-muted-foreground">每天到点自动领取所有已到达奖励</p>
                </div>
              </div>

              <div className="flex items-center justify-end gap-2">
                <span className="text-xs text-muted-foreground">
                  默认 08:00 派遣 / 20:00 领取
                </span>
                <Button size="sm" onClick={saveAutoConfig} disabled={configLoading}>
                  {configLoading ? (
                    <Loader2 className="size-4 animate-spin" />
                  ) : (
                    <Save className="size-4" />
                  )}
                  保存
                </Button>
              </div>
            </>
          )}
        </CardContent>
      </Card>

      {/* 执行日志 */}
      <Card>
        <CardHeader className="pb-3">
          <div className="flex items-center justify-between">
            <CardTitle className="flex items-center gap-2 text-base">
              <History className="size-4 text-muted-foreground" />
              执行日志
            </CardTitle>
            <Button
              variant="outline"
              size="sm"
              onClick={loadLogs}
              disabled={logsLoading}
              aria-label="刷新日志"
            >
              <RefreshCw className={`size-4 ${logsLoading ? "animate-spin" : ""}`} />
              刷新
            </Button>
          </div>
          <CardDescription>手动与自动执行的批量操作记录（最近 200 条）</CardDescription>
        </CardHeader>
        <CardContent>
          {reversedLogs.length === 0 ? (
            <p className="text-sm text-muted-foreground">暂无执行记录。</p>
          ) : (
            <div className="flex max-h-64 flex-col gap-2 overflow-y-auto pr-1">
              {reversedLogs.slice(0, 50).map((log, idx) => (
                <div key={idx} className="flex flex-col gap-1 rounded-lg border p-3 text-xs">
                  <div className="flex flex-wrap items-center gap-2">
                    <Badge variant="outline" className="px-2 py-0">
                      {KIND_LABEL[log.kind] ?? log.kind}
                    </Badge>
                    <Badge
                      variant="outline"
                      className={`px-2 py-0 ${
                        log.trigger === "auto"
                          ? "bg-sky-500/15 text-sky-600 border-sky-500/30"
                          : "bg-muted text-muted-foreground"
                      }`}
                    >
                      {TRIGGER_LABEL[log.trigger] ?? log.trigger}
                    </Badge>
                    <span className="text-muted-foreground">{formatDateTime(log.ts)}</span>
                    <span className="ml-auto font-medium">
                      成功 <span className="text-emerald-600">{log.summary?.ok ?? 0}</span> · 跳过{" "}
                      <span className="text-amber-600">{log.summary?.skipped ?? 0}</span> · 失败{" "}
                      <span className="text-destructive">{log.summary?.failed ?? 0}</span>
                    </span>
                  </div>
                  {log.summary?.accounts && log.summary.accounts.length > 0 && (
                    <p className="text-muted-foreground">
                      {log.summary.accounts
                        .slice(0, 8)
                        .map((a) => `${a.email || "?"}${a.ok ? "✓" : a.skipped ? "–" : "✗"}`)
                        .join("，")}
                      {log.summary.accounts.length > 8 ? ` 等 ${log.summary.accounts.length} 个账号` : ""}
                    </p>
                  )}
                </div>
              ))}
            </div>
          )}
        </CardContent>
      </Card>

      <Separator />

      {/* 单账号操作（保留原有） */}
      <header className="flex flex-col gap-1">
        <h2 className="text-base font-semibold">单账号操作</h2>
        <p className="text-sm text-muted-foreground">选择某个账号单独派遣 / 领取</p>
      </header>

      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">选择账号</CardTitle>
          <CardDescription>选择要操作的 WorkBuddy 账号</CardDescription>
        </CardHeader>
        <CardContent className="flex items-center gap-3">
          <Select value={accountId || undefined} onValueChange={selectAccount}>
            <SelectTrigger className="w-full max-w-[320px]">
              <SelectValue placeholder="选择账号" />
            </SelectTrigger>
            <SelectContent>
              {accounts.length === 0 ? (
                <SelectItem value="__none__" disabled>
                  暂无账号，请先到「账号管理」添加
                </SelectItem>
              ) : (
                accounts.map((account) => (
                  <SelectItem key={account.id} value={account.id}>
                    {account.nickname || account.email || account.id}
                  </SelectItem>
                ))
              )}
            </SelectContent>
          </Select>
          <Button
            variant="outline"
            size="sm"
            onClick={() => load(accountId)}
            disabled={!accountId || loading}
            aria-label="刷新旅行状态"
          >
            <RefreshCw className={`size-4 ${loading ? "animate-spin" : ""}`} />
            刷新
          </Button>
        </CardContent>
      </Card>

      {!accountId ? (
        <Alert>
          <CircleAlert className="size-4" />
          <AlertTitle>暂无账号</AlertTitle>
          <AlertDescription>
            请先在「账号管理」添加并登录 WorkBuddy 账号，再进行猫猫旅行操作。
          </AlertDescription>
        </Alert>
      ) : !t ? (
        <Alert>
          <CircleAlert className="size-4" />
          <AlertTitle>未查询到旅行状态</AlertTitle>
          <AlertDescription>
            {loading ? "正在查询…" : "点击「刷新」重新查询当前账号的猫猫旅行状态。"}
          </AlertDescription>
        </Alert>
      ) : (
        <Card>
          <CardHeader className="pb-3">
            <div className="flex items-center justify-between">
              <div className="flex items-center gap-2">
                <CardTitle className="text-base">旅行状态</CardTitle>
                <Badge
                  variant="outline"
                  className={`border px-2 py-0 text-xs font-medium ${STATE_COLORS[t.state ?? ""] ?? ""}`}
                >
                  {stateLabel(t.state)}
                </Badge>
              </div>
              {t.error ? null : (
                <span className="text-xs text-muted-foreground">
                  {status?.email || "当前账号"}
                </span>
              )}
            </div>
          </CardHeader>
          <CardContent className="flex flex-col gap-4">
            {t.error ? (
              <Alert variant="destructive">
                <CircleAlert className="size-4" />
                <AlertTitle>查询失败</AlertTitle>
                <AlertDescription>{t.error}</AlertDescription>
              </Alert>
            ) : (
              <>
                <div className="grid grid-cols-2 gap-3 text-sm sm:grid-cols-3">
                  <div className="flex flex-col gap-1">
                    <span className="text-xs text-muted-foreground">当前地点</span>
                    <span className="flex items-center gap-1.5 font-medium">
                      <MapPin className="size-3.5 text-muted-foreground" />
                      {t.locationName || t.locationCode || "—"}
                    </span>
                  </div>
                  <div className="flex flex-col gap-1">
                    <span className="text-xs text-muted-foreground">预计到达</span>
                    <span className="font-medium">{formatDateTime(t.arriveAt)}</span>
                  </div>
                  <div className="flex flex-col gap-1">
                    <span className="text-xs text-muted-foreground">奖励积分</span>
                    <span className="font-medium">
                      {t.rewardCredit != null ? `${t.rewardCredit} 分` : "—"}
                    </span>
                  </div>
                </div>

                {t.dailyLimitReached && (
                  <Alert>
                    <CircleAlert className="size-4" />
                    <AlertTitle>今日已达派遣上限</AlertTitle>
                    <AlertDescription>今日旅行派遣次数已用完，请明日再试。</AlertDescription>
                  </Alert>
                )}

                {t.hasLetter && (
                  <Alert className="border-primary/25 bg-primary/[0.06]">
                    <PackageOpen className="size-4" />
                    <AlertTitle>有旅行来信</AlertTitle>
                    <AlertDescription>Buddy 带回了信件，点击「领取奖励」查看。</AlertDescription>
                  </Alert>
                )}

                <Separator />

                <div className="flex flex-wrap items-center gap-3">
                  <Button
                    onClick={doDepart}
                    disabled={actionLoading !== null || t.state === "traveling"}
                  >
                    {actionLoading === "depart" ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <PlaneTakeoff className="size-4" />
                    )}
                    {actionLoading === "depart" ? "派遣中…" : "派遣旅行"}
                  </Button>
                  <Button
                    variant="secondary"
                    onClick={doClaim}
                    disabled={actionLoading !== null}
                  >
                    {actionLoading === "claim" ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <PackageOpen className="size-4" />
                    )}
                    {actionLoading === "claim" ? "领取中…" : "领取奖励"}
                  </Button>
                  {t.state === "traveling" && (
                    <span className="text-xs text-muted-foreground">
                      旅行中，到达后会自动可领取
                    </span>
                  )}
                </div>
              </>
            )}
          </CardContent>
        </Card>
      )}
    </div>
  );
}
