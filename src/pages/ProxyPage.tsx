import { useEffect, useState } from "react";
import { Copy, Loader2, RefreshCw } from "lucide-react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { DemoAction } from "@/components/demo-action";
import * as api from "@/lib/api";
import type { ProxyStatus } from "@/lib/types";

type Model = { id: string; owned_by?: string };

export default function ProxyPage() {
  const [status, setStatus] = useState<ProxyStatus | null>(null);
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  const [models, setModels] = useState<Model[]>([]);
  const [loading, setLoading] = useState(false);
  const [modelError, setModelError] = useState("");
  const [query, setQuery] = useState("");
  const [copied, setCopied] = useState("");

  async function loadStatus() {
    try { setStatus(await api.getProxyConfig()); setError(""); }
    catch (e) { setError(api.asError(e)); }
  }
  async function loadModels() {
    setLoading(true);
    setModelError("");
    try {
      const result = await api.getProxyModels();
      setModels(result.data);
      setModelError(result.errors?.join("；") || "");
    } catch (e) { setModelError(api.asError(e)); }
    finally { setLoading(false); }
  }
  useEffect(() => { void loadStatus(); void loadModels(); }, []);

  async function toggle(enabled: boolean) {
    if (!status || saving) return;
    setSaving(true);
    setError("");
    try { setStatus(await api.saveProxyConfig({ ...status.config, enabled })); }
    catch (e) { setError(api.asError(e)); }
    finally { setSaving(false); }
  }
  async function copy(value: string) {
    try { await navigator.clipboard.writeText(value); setCopied(value); }
    catch { setError("复制失败，请手动选择并复制。"); }
  }
  const baseUrl = status ? api.proxyBaseUrl(status) : "";
  const visibleModels = models.filter((model) => model.id.toLowerCase().includes(query.toLowerCase()));
  const running = status?.running ?? status?.enabled;

  return (
    <div className="mx-auto min-w-0 w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
      <header className="mb-10 sm:mb-12">
        <h1 className="text-2xl font-semibold tracking-tight">2API</h1>
        <p className="mt-2 text-sm leading-6 text-muted-foreground">将账号池接入支持 OpenAI 兼容接口的本机应用。</p>
      </header>
      <div className="space-y-8">
        <Card className="shadow-none">
          <CardContent className="space-y-5">
            <div className="flex items-center justify-between gap-4">
              <div className="space-y-1">
                <Label htmlFor="proxy-enabled">本地 API 服务</Label>
                <p className="text-xs text-muted-foreground">仅监听本机 127.0.0.1，应用退出后停止服务。</p>
              </div>
              <DemoAction><Switch id="proxy-enabled" checked={status?.enabled ?? false} disabled={!status || saving} onCheckedChange={(value) => void toggle(value)} /></DemoAction>
            </div>
            <div className="flex items-center gap-2">
              <Badge variant={running ? "default" : "secondary"}>{!status ? "状态未获取" : running ? "运行中" : status.enabled ? "启动失败" : "已关闭"}</Badge>
              <Button size="sm" variant="ghost" onClick={() => void loadStatus()}><RefreshCw className="size-3" />刷新状态</Button>
            </div>
            <div className="space-y-2">
              <Label htmlFor="proxy-base-url">Base URL</Label>
              <div className="flex gap-2">
                <Input id="proxy-base-url" readOnly value={baseUrl} className="font-mono text-xs" />
                <Button variant="outline" disabled={!baseUrl} onClick={() => void copy(baseUrl)} aria-label="复制 Base URL"><Copy />{copied === baseUrl && baseUrl ? "已复制" : "复制"}</Button>
              </div>
            </div>
            <p className="text-xs leading-6 text-muted-foreground">
              支持 <code>/v1/chat/completions</code> 和 <code>/v1/models</code>，按账号池策略转发。客户端若要求 API Key，可填写任意非空值。<br />
              本机程序可免凭据调用账号额度，请勿在共享电脑开启。
            </p>
            {(error || status?.error) && <Alert variant="destructive"><AlertDescription>{error || status?.error}</AlertDescription></Alert>}
          </CardContent>
        </Card>
        <section className="space-y-3" aria-labelledby="models-title">
          <div className="flex items-center justify-between gap-3">
            <h2 id="models-title" className="text-sm font-medium">模型列表 <span className="text-muted-foreground">({models.length})</span></h2>
            <Button size="sm" variant="outline" disabled={loading} onClick={() => void loadModels()}>{loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}刷新模型</Button>
          </div>
          <p className="text-xs leading-5 text-muted-foreground">从已添加账号的渠道获取。cn/ 指定国内站，intl/ 指定国际站，无前缀使用默认账号池。点击复制模型 ID 用于客户端配置。</p>
          <Input aria-label="搜索模型" placeholder="搜索模型 ID…" value={query} onChange={(event) => setQuery(event.target.value)} />
          {modelError && <Alert variant="destructive"><AlertDescription>{modelError}</AlertDescription></Alert>}
          <Card className="overflow-hidden py-0 shadow-none">
            <Table>
              <TableHeader><TableRow><TableHead>模型 ID</TableHead><TableHead className="w-24">渠道</TableHead><TableHead className="w-20"><span className="sr-only">复制</span></TableHead></TableRow></TableHeader>
              <TableBody>
                {visibleModels.map((model) => <TableRow key={model.id}>
                  <TableCell className="break-all whitespace-normal font-mono text-xs">{model.id}</TableCell>
                  <TableCell className="text-xs text-muted-foreground">{model.id.startsWith("cn/") ? "国内站" : model.id.startsWith("intl/") ? "国际站" : "默认池"}</TableCell>
                  <TableCell><Button variant="ghost" size="sm" onClick={() => void copy(model.id)} aria-label={`复制 ${model.id}`}>{copied === model.id ? "已复制" : <Copy className="size-3.5" />}</Button></TableCell>
                </TableRow>)}
                {!visibleModels.length && <TableRow><TableCell colSpan={3} className="py-8 text-center text-sm text-muted-foreground">{loading ? "正在获取模型…" : query ? "没有匹配的模型" : modelError ? "模型获取失败，请重试。" : "暂无可用模型，请先添加有效账号，再刷新。"}</TableCell></TableRow>}
              </TableBody>
            </Table>
          </Card>
        </section>
      </div>
    </div>
  );
}
