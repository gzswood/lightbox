import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { api, BoxView as BoxViewData } from "./api";
import SettingsDialog from "./SettingsDialog";
import { Theme, useTheme } from "./theme";

/**
 * 「设置」独立窗口。
 *
 * 以前设置是画在面板里的一个遮罩层，看着像面板的一部分；现在它是自己的窗口：
 * 圆角、阴影、失焦关闭都按窗口来，面板不用为了露出设置把内容挡住一半。
 *
 * 窗口是复用的（后端启动时建好、之后只 show/hide），所以要靠 `settings:target`
 * 事件和后端的 `settings_target` 知道「现在在给哪个盒子做设置」。
 *
 * URL 带 `?g=1` 时是**全局设置**（托盘「设置…」进的）：内容和单盒设置共用，
 * 但隐藏「名称 / 置于顶层」两个单盒项，也不监听任何盒子的数据。
 */
export default function SettingsView() {
  const global = new URLSearchParams(window.location.search).get("g") === "1";
  const [id, setId] = useState("");
  const [view, setView] = useState<BoxViewData | null>(null);
  const [theme, setTheme] = useTheme();
  /** 取色弹窗在改哪个色块。状态提到这一层：Esc 要先关弹窗、再关设置窗口 */
  const [colorTarget, setColorTarget] = useState<"box" | "panel" | null>(null);
  const colorTargetRef = useRef(colorTarget);
  useEffect(() => {
    colorTargetRef.current = colorTarget;
  }, [colorTarget]);
  const themeRef = useRef(theme);
  useEffect(() => {
    themeRef.current = theme;
  }, [theme]);

  const timer = useRef<number | undefined>(undefined);
  const changeTheme = useCallback(
    (patch: Partial<Theme>) => {
      const next = { ...themeRef.current, ...patch };
      themeRef.current = next;
      setTheme(next);
      window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => {
        api.setTheme(next).catch(() => {});
      }, 120);
    },
    [setTheme],
  );
  useEffect(() => () => window.clearTimeout(timer.current), []);

  const load = useCallback((boxId: string) => {
    if (!boxId) return;
    api
      .view(boxId)
      .then(setView)
      .catch(() => {});
  }, []);

  // 目标盒子：后端每次弹出都会推一次；重载（F5）时从后端要回来（全局模式没有目标盒子）
  useEffect(() => {
    if (global) return;
    let alive = true;
    let un: (() => void) | undefined;
    listen<string>("settings:target", (e) => {
      setId(e.payload);
      load(e.payload);
    }).then((f) => {
      if (alive) un = f;
      else f();
    });
    api
      .settingsTarget()
      .then((t) => {
        if (alive && t) {
          setId(t);
          load(t);
        }
      })
      .catch(() => {});
    return () => {
      alive = false;
      un?.();
    };
  }, [global, load]);

  // 盒子改名 / 切置顶后同步（后端会广播 box:changed）；全局模式没有目标盒子，不用听
  useEffect(() => {
    if (global || !id) return;
    let alive = true;
    let un: (() => void) | undefined;
    listen<string>("box:changed", (e) => {
      if (e.payload === id) load(id);
    }).then((f) => {
      if (alive) un = f;
      else f();
    });
    return () => {
      alive = false;
      un?.();
    };
  }, [id, load]);

  // 失焦**不再关闭**设置：设置是独立窗口，用户点桌面往往只是想收起面板，
  // 正在改的设置不该跟着消失。关闭只走右上角 ✕ 或 Esc。
  // 但点桌面时面板要收（那是另一条规则，面板自己失焦不管用 —— 焦点在设置上），
  // 所以这里只把面板收掉，自己也留着。
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    const w = getCurrentWindow();
    w.onFocusChanged(({ payload }) => {
      if (payload) return;
      window.setTimeout(async () => {
        try {
          if (await w.isFocused()) return;
          // 切「置于顶层」会重建盒子窗口、抢走前台；那不是用户点了别处
          if (await api.foregroundIsSelf()) return;
        } catch {
          /* ignore */
        }
        // 点桌面 / 别的程序：面板该收起，设置留着
        api.closePanel().catch(() => {});
      }, 90);
    })
      .then((f) => {
        if (alive) un = f;
        else f();
      })
      .catch(() => {});

    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        // 输入框里的 Esc 先还给输入框（取消改名）
        const el = document.activeElement;
        if (el && (el as HTMLElement).tagName === "INPUT" && (el as HTMLInputElement).type !== "range") {
          (el as HTMLInputElement).blur();
          return;
        }
        // 取色弹窗开着：第一下 Esc 只关弹窗，别把整个设置页收掉
        if (colorTargetRef.current) {
          setColorTarget(null);
          return;
        }
        api.hideSettings().catch(() => {});
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      alive = false;
      un?.();
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  // 高度自适应：内容高度一变就告诉后端改窗口尺寸（同一个高度只发一次）
  const lastH = useRef(0);
  const applyHeight = useCallback((h: number) => {
    const v = Math.round(h);
    if (v < 80 || Math.abs(v - lastH.current) < 2) return;
    lastH.current = v;
    api.fitSettings(v).catch(() => {});
  }, []);

  return (
    <div className="dlg-win">
      {global ? (
        <SettingsDialog
          global
          theme={theme}
          onTheme={changeTheme}
          onChanged={() => {}}
          onClose={() => api.hideSettings().catch(() => {})}
          onHeight={applyHeight}
          colorTarget={colorTarget}
          onColorTarget={setColorTarget}
        />
      ) : view ? (
        <SettingsDialog
          id={view.id}
          name={view.name}
          topmost={view.topmost}
          theme={theme}
          onTheme={changeTheme}
          onChanged={() => load(view.id)}
          onClose={() => api.hideSettings().catch(() => {})}
          onHeight={applyHeight}
          colorTarget={colorTarget}
          onColorTarget={setColorTarget}
        />
      ) : (
        <div className="dlg-win-empty">…</div>
      )}
    </div>
  );
}
