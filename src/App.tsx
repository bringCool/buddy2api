import { useEffect, useState } from "react";
import { BrowserRouter, HashRouter, Navigate, NavLink, Outlet, Route, Routes } from "react-router-dom";
import { ArrowUp, Package, Plug, MessagesSquare, Settings, Sparkles, User } from "lucide-react";

import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import type { UpdateInfo } from "@/lib/types";
import AccountsPage from "@/pages/AccountsPage";
import CreditStatsPage from "@/pages/CreditStatsPage";
import TokenStatsPage from "@/pages/TokenStatsPage";
import ProxyPage from "@/pages/ProxyPage";
import SettingsPage from "@/pages/SettingsPage";
import { AppIconMark } from "@/components/product-marks";
import { UpdateInstallDialog } from "@/components/update-install-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Toaster } from "@/components/ui/sonner";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { demoModeEnabled, pagesDemoHostingEnabled } from "@/lib/demo-mode";
import { useCreditAutoRefresh } from "@/lib/use-credit-auto-refresh";
import { useRotateDeferredNotice } from "@/lib/use-rotate-deferred-notice";
import { useWorkbuddyStatusRefresh } from "@/lib/use-workbuddy-status-refresh";
import { useAccountsStore } from "@/stores/accounts";

function UpdateCenter() {
  const version = useAccountsStore((s) => s.status?.version);
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);

  useEffect(() => {
    let disposed = false;

    async function checkForUpdate() {
      try {
        const result = await api.checkUpdate();
        if (!disposed) setInfo(result.ok ? result : null);
      } catch {
        // 左下角只展示可操作的升级状态，网络错误不打扰正常使用。
      }
    }

    void checkForUpdate();
    const timer = window.setInterval(() => void checkForUpdate(), 30 * 60 * 1000);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, []);

  const hasUpdate = Boolean(info?.ok && info.hasUpdate && info.latest);

  return (
    <>
      <section aria-label="应用版本" className="mt-auto border-t border-sidebar-border px-3 pt-4">
        <div className="flex min-h-9 items-center gap-2.5">
          <div className="flex size-8 shrink-0 items-center justify-center rounded-lg border border-sidebar-border bg-background/60 text-muted-foreground">
            <Package className="size-4" strokeWidth={1.5} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <p className="text-[10px] leading-4 text-muted-foreground">当前版本</p>
            <p className="truncate text-xs font-medium leading-5 tabular-nums text-sidebar-foreground">{version ? `v${version}` : "读取中…"}</p>
          </div>
          {hasUpdate && (
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="size-7 shrink-0 rounded-lg bg-primary/10 text-primary hover:bg-primary/20 hover:text-primary"
                  aria-label={`升级到 v${info?.latest}`}
                  onClick={() => setDialogOpen(true)}
                >
                  <ArrowUp className="size-3.5" strokeWidth={2} aria-hidden="true" />
                </Button>
              </TooltipTrigger>
              <TooltipContent side="top">升级到 v{info?.latest}</TooltipContent>
            </Tooltip>
          )}
        </div>
      </section>
      <UpdateInstallDialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        update={info}
      />
    </>
  );
}

function Layout() {
  const hasUnifiedTitleBar =
    api.isDesktop() && typeof navigator !== "undefined" && navigator.userAgent.includes("Macintosh");
  useCreditAutoRefresh();
  useWorkbuddyStatusRefresh();
  useRotateDeferredNotice();

  return (
    <div className="flex h-screen min-h-0 overflow-hidden bg-background">
      {hasUnifiedTitleBar ? (
        <div
          data-tauri-drag-region
          className="fixed inset-x-0 top-0 z-50 h-8"
          aria-hidden="true"
        />
      ) : null}
      <aside
        className={cn(
          "flex min-h-0 w-[188px] lg:w-[204px] shrink-0 flex-col border-r border-sidebar-border bg-sidebar px-3 pb-4",
          hasUnifiedTitleBar ? "pt-14" : "pt-7",
        )}
      >
        <div className="flex items-center gap-2.5 px-3 pb-8">
          <AppIconMark size={30} />
          <div className="min-w-0">
            <div
              className="truncate text-[15px] leading-5 tracking-[-0.02em] text-sidebar-foreground/90"
              style={{
                fontFamily: '"Bricolage Grotesque Variable", "SF Pro Display", ui-sans-serif, sans-serif',
                fontWeight: 640,
              }}
            >
              Buddy2API
            </div>
            {demoModeEnabled && (
              <Badge variant="secondary" className="mt-1 h-5 border-0 px-1.5 text-[10px] text-sidebar-foreground/60 shadow-none">
                演示模式
              </Badge>
            )}
          </div>
        </div>
        <nav className="flex min-h-0 flex-1 flex-col gap-1 overflow-y-auto" aria-label="主导航">
          <p className="mb-1 px-3 text-[11px] font-medium tracking-wider text-muted-foreground">工作空间</p>
          {[
            { to: "/", label: "账号管理", icon: User },
            { to: "/2api", label: "2API", icon: Plug },
            { to: "/token-stats", label: "Token 统计", icon: MessagesSquare },
            { to: "/credit-stats", label: "积分统计", icon: Sparkles },
          ].map(({ to, label, icon: Icon }) => (
            <NavLink key={to} to={to} end={to === "/"} className="app-nav-link">
              <Icon className="size-4" strokeWidth={1.75} aria-hidden="true" />
              {label}
            </NavLink>
          ))}
          <div className="mx-3 my-3 border-t border-sidebar-border" />
          <NavLink to="/settings" className="app-nav-link">
            <Settings className="size-4" strokeWidth={1.75} aria-hidden="true" />
            设置
          </NavLink>
        </nav>
        {api.isWebui() && !demoModeEnabled ? null : <UpdateCenter />}
      </aside>
      <main
        className={cn(
          "min-w-0 flex-1 overflow-y-auto bg-background overscroll-contain",
          hasUnifiedTitleBar && "pt-8",
        )}
      >
        <Outlet />
      </main>
    </div>
  );
}

export default function App() {
  const Router = pagesDemoHostingEnabled ? HashRouter : BrowserRouter;

  return (
    <TooltipProvider delayDuration={250}>
      <Router>
        <Routes>
          <Route element={<Layout />}>
            <Route path="/" element={<AccountsPage />} />
            <Route path="/credit-stats" element={<CreditStatsPage />} />
            <Route path="/token-stats" element={<TokenStatsPage />} />
            <Route path="/2api" element={<ProxyPage />} />
            <Route path="/settings" element={<SettingsPage />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Route>
        </Routes>
        <Toaster />
      </Router>
    </TooltipProvider>
  );
}
