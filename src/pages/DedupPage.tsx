import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import {
  AlertTriangle,
  CheckCircle2,
  CopyX,
  Loader2,
  RefreshCw,
  Search,
  Trash2,
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
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Separator } from "@/components/ui/separator";
import * as api from "@/lib/api";
import type { DedupPreviewResult, DupGroup } from "@/lib/types";
import { cn } from "@/lib/utils";

type LoadState =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "done"; data: DedupPreviewResult };

export default function DedupPage() {
  const [state, setState] = useState<LoadState>({ status: "idle" });
  const [executing, setExecuting] = useState(false);
  const [confirmOpen, setConfirmOpen] = useState(false);

  const runPreview = useCallback(async () => {
    setState({ status: "loading" });
    try {
      const data = await api.dedupPreview();
      if (!data.ok) {
        setState({ status: "error", message: data.error ?? "预览失败" });
        return;
      }
      setState({ status: "done", data });
    } catch (e) {
      setState({ status: "error", message: e instanceof Error ? e.message : String(e) });
    }
  }, []);

  useEffect(() => {
    void runPreview();
  }, [runPreview]);

  const totalToDelete = state.status === "done" ? state.data.totalToDelete : 0;
  const totalGroups = state.status === "done" ? state.data.totalGroups : 0;
  const groups: DupGroup[] = state.status === "done" ? state.data.groups : [];

  const handleExecute = useCallback(async () => {
    setConfirmOpen(false);
    setExecuting(true);
    try {
      const res = await api.dedupExecute();
      if (res.ok) {
        toast.success(res.message ?? `已软删 ${res.deleted ?? 0} 条重复会话`);
      } else {
        toast.error(res.error ?? "清理失败");
      }
      // 执行后刷新预览
      await runPreview();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setExecuting(false);
    }
  }, [runPreview]);

  return (
    <div className="mx-auto flex w-full max-w-3xl flex-col gap-4 px-4 py-6">
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <CopyX className="size-5" />
            会话去重清理
          </CardTitle>
          <CardDescription>
            账号迁移 / 云端同步可能把同一批会话复制成多份（同秒、同标题、同目录）。
            本页会扫描当前账号的重复会话，并可按需软删（保留每组最早一条，可回滚）。
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-4">
          <div className="flex flex-wrap items-center gap-2">
            <Button
              variant="outline"
              onClick={runPreview}
              disabled={state.status === "loading"}
            >
              {state.status === "loading" ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <Search className="size-4" />
              )}
              扫描重复
            </Button>
            <Button
              onClick={() => setConfirmOpen(true)}
              disabled={totalToDelete === 0 || state.status === "loading" || executing}
              variant="destructive"
            >
              {executing ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <Trash2 className="size-4" />
              )}
              软删重复
            </Button>
            <Button variant="ghost" size="icon" onClick={runPreview} disabled={state.status === "loading"}>
              <RefreshCw className={cn("size-4", state.status === "loading" && "animate-spin")} />
            </Button>
          </div>

          {state.status === "loading" && (
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="size-4 animate-spin" /> 正在扫描当前账号会话…
            </div>
          )}

          {state.status === "error" && (
            <Alert variant="destructive">
              <AlertTriangle className="size-4" />
              <AlertTitle>扫描失败</AlertTitle>
              <AlertDescription>{state.message}</AlertDescription>
            </Alert>
          )}

          {state.status === "done" && totalToDelete === 0 && (
            <Alert>
              <CheckCircle2 className="size-4" />
              <AlertTitle>未发现重复会话</AlertTitle>
              <AlertDescription>
                当前账号（{state.data.uid ?? "未知"}）没有重复会话，无需清理。
              </AlertDescription>
            </Alert>
          )}

          {state.status === "done" && totalToDelete > 0 && (
            <>
              <div className="flex items-center gap-2 text-sm">
                <Badge variant="destructive">{totalGroups} 组重复</Badge>
                <Badge variant="secondary">共 {totalToDelete} 条可清理</Badge>
                <span className="text-muted-foreground">
                  每组保留 1 条最早会话，其余软删
                </span>
              </div>

              <Separator />

              <div className="flex max-h-96 flex-col gap-2 overflow-y-auto pr-1">
                {groups.map((g, i) => (
                  <div
                    key={`${g.keepId}-${i}`}
                    className="rounded-lg border bg-card px-3 py-2"
                  >
                    <div className="flex items-center justify-between gap-2">
                      <span className="min-w-0 flex-1 truncate text-sm font-medium">
                        {g.title || "(无标题)"}
                      </span>
                      <Badge variant="secondary">{g.count} 条</Badge>
                    </div>
                    <div className="mt-1 flex items-center gap-2 text-xs text-muted-foreground">
                      <span className="min-w-0 truncate">{g.cwd}</span>
                      <span className="shrink-0">
                        待删 {g.dupIds.length} 条 · 保留 {g.keepId.slice(0, 8)}…
                      </span>
                    </div>
                  </div>
                ))}
              </div>
            </>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">说明</CardTitle>
        </CardHeader>
        <CardContent className="text-sm leading-relaxed text-muted-foreground">
          <ul className="list-inside list-disc space-y-1">
            <li>
              <strong className="text-foreground">判据：</strong>
              <code>(账号, 最后活动时间, 标题, 目录)</code> 四字段精确一致才判定为重复，不会误删。
            </li>
            <li>
              <strong className="text-foreground">方式：</strong>软删（标记删除，非物理删），
              数据仍可回滚，且不会损坏数据库。
            </li>
            <li>
              <strong className="text-foreground">提示：</strong>清理后请重启 WorkBuddy 客户端
              让侧栏刷新生效。
            </li>
          </ul>
        </CardContent>
      </Card>

      <Dialog open={confirmOpen} onOpenChange={setConfirmOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认清理重复会话？</DialogTitle>
            <DialogDescription>
              将软删 <strong>{totalToDelete}</strong> 条重复会话（共 {totalGroups} 组），
              每组保留最早一条。此操作可回滚，但建议先通过「账号管理 → 导出」做好备份。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmOpen(false)}>
              取消
            </Button>
            <Button variant="destructive" onClick={handleExecute}>
              确认软删
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
