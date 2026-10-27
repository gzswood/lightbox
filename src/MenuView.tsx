import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { api, MenuData } from "./api";
import { useTheme } from "./theme";

/**
 * 收纳盒右键菜单。它是一个独立的小窗口（不是盒子窗口里的 DOM）：
 * 盒子只有 96×96 且被 SetWindowRgn 裁成圆角，菜单画在里面会被裁掉。
 */
export default function MenuView() {
  // 菜单也跟主题走（背景色可能是深色，白底菜单会突兀）
  useTheme();
  const [data, setData] = useState<MenuData | null>(null);
  const [err, setErr] = useState("");
  /** 「重命名」模式：同一个窗口缩成 96×26 的输入框，盖在盒子名字上 */
  const [rename, setRename] = useState<{ id: string; name: string } | null>(null);
  const [draft, setDraft] = useState("");
  const draftRef = useRef("");
  const renameRef = useRef<{ id: string; name: string } | null>(null);
  const focused = useRef(false);
  const busy = useRef(false);
  /** 正在切换「置顶」：盒子窗口会重建，这段时间内的失焦不收起菜单 */
  const switchingTop = useRef(false);
  /** 免收起的时间戳（切换置顶期间 + 之后一小段，等重建窗口的激活消息消化掉） */
  const holdFocus = useRef(0);

  const hide = () => {
    api.hideMenu().catch(() => {});
  };

  // 菜单条目是条件渲染的（自定义图标 / 非空提示 / 错误提示），窗口高度首次按公式
  // 估值显示。这里渲染完后测准真实高度回报后端修正——估值一旦偏小，底部条目会被
  // overflow:hidden 裁掉（v0.9.11 加「全部图标移回桌面」就裁掉过「退出 LightBox」）。
  // 用 useLayoutEffect：在浏览器绘制前完成修正，用户看不到中间帧。
  // 重命名模式下窗口由后端缩成输入条，不碰尺寸。
  const fitRef = useRef(-1);
  useLayoutEffect(() => {
    if (rename) {
      fitRef.current = -1;
      return;
    }
    const el = document.querySelector<HTMLElement>(".menu");
    if (!el) return;
    const h = Math.ceil(el.scrollHeight) + 2; // +2 = .menu 上下边框
    if (fitRef.current === h) return;
    fitRef.current = h;
    api.fitMenu(h).catch(() => {});
  }, [data, err, rename]);

  // 拉初始数据 + 每次弹出时刷新（主进程会 emit menu:data）
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    let un2: (() => void) | undefined;
    api.menuData().then((d) => {
      if (alive && d) setData(d);
    }).catch(() => {});
    listen<MenuData>("menu:data", (e) => {
      setData(e.payload);
      setErr("");
      setRename(null);
      renameRef.current = null;
      focused.current = false;
    })
      .then((f) => {
        if (alive) un2 = f;
        else f();
      });
    // 右键菜单「重命名」→ 窗口变成输入框
    listen<{ id: string; name: string }>("menu:rename", (e) => {
      const t = e.payload;
      renameRef.current = t;
      setRename(t);
      draftRef.current = t.name;
      setDraft(t.name);
      focused.current = false;
    })
      .then((f) => {
        if (alive) un = f;
        else f();
      });
    return () => {
      alive = false;
      un?.();
      un2?.();
    };
  }, []);

  // 失焦 / Esc 收起；万一没拿到焦点（极端情况）也不能一直浮在上面
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    getCurrentWindow()
      .onFocusChanged(({ payload }) => {
        if (payload) focused.current = true;
        else if (focused.current && Date.now() >= holdFocus.current) hide();
      })
      .then((f) => {
        if (alive) un = f;
        else f();
      });
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") hide();
    };
    window.addEventListener("keydown", onKey);
    return () => {
      alive = false;
      un?.();
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  // 兜底：万一弹出后没拿到焦点（不会失焦 → 收不起来），1.5s 内自己收掉
  useEffect(() => {
    if (!data) return;
    const t = window.setTimeout(() => {
      if (document.hasFocus()) {
        focused.current = true;
        return;
      }
      if (!focused.current) hide();
    }, 1500);
    return () => window.clearTimeout(t);
  }, [data]);

  /** 先收起菜单再执行动作：动作可能建窗/开面板，留着菜单会挡住并抢焦点 */
  const run = async (fn: () => Promise<unknown>) => {
    if (busy.current) return;
    busy.current = true;
    await api.hideMenu().catch(() => {});
    try {
      await fn();
    } catch {
      /* 兜底：错误无处展示，主进程侧已有保护 */
    } finally {
      window.setTimeout(() => {
        busy.current = false;
      }, 200);
    }
  };

  const commitRename = async () => {
    const t = renameRef.current;
    if (!t) return;
    renameRef.current = null;
    setRename(null);
    const v = draftRef.current.trim();
    if (v && v !== t.name) {
      await api.renameBox(t.id, v).catch(() => {});
    }
    api.hideMenu().catch(() => {});
  };

  const del = async () => {
    if (!data) return;
    if (data.count > 0) {
      setErr(`还有 ${data.count} 个快捷方式，请先移出再删除`);
      return;
    }
    try {
      await api.deleteBox(data.id);
      hide();
    } catch (e) {
      setErr(String(e));
    }
  };

  /**
   * 切换「置顶」。**不收起菜单**：开关立刻拨过去，用户可以接着点别的项，
   * 想关菜单就点空白处（失焦）或按 Esc。
   *
   * 后端起切换时是「另建一个盒子窗口再销毁旧的」（见 set_box_topmost 的注释），
   * 期间可能抢走前台，所以：
   *   1. 切换期间不响应失焦（holdFocus）；
   *   2. 切完把焦点抢回来，菜单继续可用，也顺带把菜单提到置顶区最前面
   *      （置顶的盒子是 topmost 窗口，会被后建的盒子挡住）。
   */
  const toggleTop = async () => {
    const d = data;
    if (!d || switchingTop.current) return;
    switchingTop.current = true;
    holdFocus.current = Date.now() + 3000;
    const next = !d.topmost;
    // 乐观更新：开关立刻动，不等后端（后端要几百毫秒重建窗口）
    setData({ ...d, topmost: next });
    try {
      await api.setTopmost(d.id, next);
    } catch (e) {
      setData((cur) => (cur ? { ...cur, topmost: !next } : cur));
      setErr(String(e));
    } finally {
      try {
        await getCurrentWindow().setFocus();
      } catch {
        /* 抢不回来就算了，用户点一下菜单就恢复 */
      }
      focused.current = true;
      // 留一点余量，让重建窗口引出的激活消息先到
      holdFocus.current = Date.now() + 600;
      switchingTop.current = false;
      // 免收起窗口期内如果用户手快点了桌面，失焦消息被吃掉、菜单会一直浮着。
      // 余量过后补一次真实焦点检查。
      window.setTimeout(() => {
        if (!switchingTop.current && !document.hasFocus()) hide();
      }, 700);
    }
  };

  if (rename) {
    return (
      <div className="menu-rename">
        <input
          className="menu-rename-input"
          value={draft}
          autoFocus
          maxLength={24}
          onFocus={(e) => e.currentTarget.select()}
          onChange={(e) => {
            setDraft(e.target.value);
            draftRef.current = e.target.value;
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              commitRename();
            } else if (e.key === "Escape") {
              e.preventDefault();
              renameRef.current = null;
              setRename(null);
              api.hideMenu().catch(() => {});
            }
          }}
          onBlur={() => commitRename()}
        />
      </div>
    );
  }

  if (!data) return <div className="menu" />;

  return (
    <div className="menu" onContextMenu={(e) => e.preventDefault()}>
      <div className="menu-head">
        <span className="menu-head-name">{data.name}</span>
        <span className="menu-head-count">{data.count}</span>
      </div>

      <div className="menu-item" onClick={() => run(() => api.createBox(""))}>
        新建收纳盒
      </div>
      {/* 直接在桌面上的盒子里改名（后端负责把前台/焦点抢过来，否则敲键盘到不了 child 窗口） */}
      <div className="menu-item" onClick={() => run(() => api.beginBoxRename(data.id))}>
        重命名
      </div>
      {/* 只认 .ico：对话框过滤器 + 后端扩展名校验两道 */}
      <div className="menu-item" onClick={() => run(() => api.pickBoxIcon(data.id))}>
        设置图标…
      </div>
      {data.has_icon && (
        <div className="menu-item" onClick={() => run(() => api.clearBoxIcon(data.id))}>
          恢复默认图标
        </div>
      )}
      {/* 置顶显示成开关：点整行就切换，**菜单不收起**（拨完开关还能接着点别的项）。
          嵌进任务栏的盒子是任务栏的子窗口，置顶没有意义，直接不显示。 */}
      {!data.in_bar && (
        <div
          className={"menu-item menu-switch" + (data.topmost ? " on" : "")}
          onClick={toggleTop}
        >
          <span>置顶</span>
          <span className="track" />
        </div>
      )}
      {/* 嵌入任务栏：盒子变成任务栏里的方形按钮（高 = 任务栏厚度），
          只能沿任务栏横向拖动，由用户自己挑不挡任务栏信息的位置。
          胶囊团成员是**整团**一起换形态（团是一个整体，只切一个会当场散架），
          所以文案也按团说。 */}
      <div
        className="menu-item"
        onClick={() => run(() => api.setBoxInBar(data.id, !data.in_bar))}
      >
        {data.grouped
          ? data.in_bar
            ? "整团移出任务栏"
            : "整团嵌入任务栏"
          : data.in_bar
            ? "移出任务栏"
            : "嵌入任务栏"}
      </div>
      {/* 胶囊团：几个吸附在一起的盒子可以组合成一个整体，拖一个就整团走。
          孤零零一个盒子（周围没有吸附的邻居）不显示这一项；已在团里就换成「解散」。 */}
      {data.grouped ? (
        <div className="menu-item" onClick={() => run(() => api.ungroupBox(data.id))}>
          解散胶囊团
        </div>
      ) : data.can_group ? (
        <div className="menu-item" onClick={() => run(() => api.groupBoxes(data.id))}>
          组合为胶囊团
        </div>
      ) : null}
      {/* 指向托盘的「设置…」：全局设置窗口（外观 / 开机自启 / 存储位置）。
          文案就用「设置...」——菜单里只有这一个设置入口，带上产品名反而啰嗦 */}
      <div className="menu-item" onClick={() => run(() => api.openGlobalSettings())}>
        设置...
      </div>

      <div className="menu-sep" />

      <div
        className={"menu-item danger" + (data.count > 0 ? " disabled" : "")}
        onClick={() => {
          if (data.count > 0) {
            setErr(`还有 ${data.count} 个快捷方式，请先移出再删除`);
            return;
          }
          del();
        }}
      >
        删除收纳盒
      </div>

      {(data.count > 0 || err) && (
        <div className={"menu-hint" + (err ? " bad" : "")}>
          {err || `收纳盒非空，不能删除`}
        </div>
      )}

      <div className="menu-sep" />

      <div className="menu-item" onClick={() => run(() => api.quit())}>
        退出 LightBox
      </div>
    </div>
  );
}
