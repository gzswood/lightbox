import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { listen } from "@tauri-apps/api/event";
import { api, BoxView as BoxViewData } from "./api";
import { useTheme } from "./theme";

/** 内置的「收纳盒」图标：一个带盖子的盒子，和桌面图标同一视觉量级 */
function BoxGlyph() {
  return (
    <svg viewBox="0 0 48 48" width="46" height="46" aria-hidden="true">
      <defs>
        <linearGradient id="lbBody" x1="0" y1="0" x2="0.35" y2="1">
          <stop offset="0" stopColor="#6FA4FF" />
          <stop offset="1" stopColor="#2F6FEB" />
        </linearGradient>
        <linearGradient id="lbLid" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" stopColor="#A8C8FF" />
          <stop offset="1" stopColor="#6B9CFA" />
        </linearGradient>
      </defs>
      {/* 盒身 */}
      <rect x="6" y="17" width="36" height="25" rx="7" fill="url(#lbBody)" />
      {/* 盒身的高光面 */}
      <path d="M13 17h10v25h-3a7 7 0 0 1-7-7V17z" fill="#fff" opacity="0.12" />
      {/* 盒盖 */}
      <rect x="3.5" y="9" width="41" height="12" rx="6" fill="url(#lbLid)" />
      {/* 卡扣 */}
      <rect x="19" y="13" width="10" height="4.2" rx="2.1" fill="#fff" opacity="0.9" />
    </svg>
  );
}

export default function BoxView({ id }: { id: string }) {
  const [view, setView] = useState<BoxViewData | null>(null);
  const [over, setOver] = useState(false);
  /** 正在拖动（系统的窗口拖动期间收不到 DOM 事件，所以要自己记状态） */
  const [dragging, setDragging] = useState(false);

  const drag = useRef<{ x: number; y: number; moved: boolean } | null>(null);
  /** 是否嵌在任务栏里（拖动落位要走 snap_bar，不能走 snap_box） */
  const barRef = useRef(false);
  const dragStartedRef = useRef(0);
  /** 上一次「点开/收起面板」的时间：双击的第二下不能算又一次开关 */
  const lastClickRef = useRef(0);
  /** 是否属于某个胶囊团（拖动时要先算整团偏移，拖着一起走） */
  const inGroupRef = useRef(false);
  const readySent = useRef(false);

  // 盒子所在显示器的缩放系数（后端按盒子**位置**查，盒子的窗口 DPI 不可信）。
  //
  // 为什么要这么做：盒子挂在 Progman 下面，WebView 的 DPR 是 tao 建窗口那一刻定下来的，
  // 之后换屏也不变（实测窗口在 100% 屏上，页面仍然按 1.25 排版）。
  // 页面只认 DPR 的话，125% 屏上 80px 的内容会画成 100px 物理像素、被圆角区域裁掉
  // （「名字下半截不见了」）。这里拿后端的 scale，除以真实 DPR，
  // 直接把 80×80 的舞台缩放到「80 × scale 物理像素」，和窗口尺寸、区域三者对齐。
  const [monScale, setMonScale] = useState(1);
  const [dpr, setDpr] = useState(() => window.devicePixelRatio || 1);
  /** 名字要不要用深色字（盒子底下那块壁纸是浅色）。见下面 labelLuma 那段说明。 */
  const [labelDark, setLabelDark] = useState(false);
  /** 重新测一次背景亮度：拖动落位后立刻调，不用等 3 秒的轮询 */
  const lumaTick = useRef<() => void>(() => {});

  // 背景色由全局设置决定；盒子窗口是独立 webview，靠 theme:changed 广播同步
  useTheme();

  useEffect(() => {
    let alive = true;
    api.boxScale(id).then((s) => {
      if (alive && s > 0) setMonScale(s);
    }).catch(() => {});
    let un: (() => void) | undefined;
    listen<{ id: string; scale: number }>("box:scale", (e) => {
      if (e.payload && e.payload.id === id && e.payload.scale > 0) setMonScale(e.payload.scale);
    }).then((f) => {
      if (alive) un = f;
      else f();
    });
    const onResize = () => setDpr(window.devicePixelRatio || 1);
    window.addEventListener("resize", onResize);
    return () => {
      alive = false;
      un?.();
      window.removeEventListener("resize", onResize);
    };
  }, [id]);

  // 名字颜色跟着壁纸走 —— 和桌面图标一样。
  //
  // 盒子没有底板，名字直接压在壁纸上：白字 + 深色描边在深色壁纸 / 照片上很清楚，
  // 但在浅色（尤其是纯白）壁纸上，白字和背景一样亮，只剩一圈模糊的深色描边，
  // 12px 的中文就是一团灰（用户实测白色壁纸下「冷宫」两个字看不清）。
  // 桌面图标是同一问题的官方解法：**壁纸亮就用深色字**。这里抄这个思路。
  //
  // 采样在**后端**做（要读屏幕像素，页面里拿不到背后的壁纸），前端只做两件事：
  //  1. 定时问（1.5 秒）——拖动落位、换壁纸、换屏、Explorer 重启重建窗口，
  //     一个轮询口子全罩住，比给每个触发点加钩子省事，开销是几个 GetPixel；
  //     间隔原来是 3 秒，换壁纸后最多要等三秒才变字色（用户反馈「有点延迟」），
  //     采样成本可以忽略，就压到 1.5 秒。
  //  2. **带滞回**地决定：亮度卡在阈值附近时，一点点变化就会让字黑白来回闪。
  //     已经是深色字就得更亮一些（0.62）才切回白字，反过来只要暗到 0.52 就切。
  useEffect(() => {
    let alive = true;
    let prev = false;
    const tick = () => {
      api
        .labelLuma(id)
        .then((l) => {
          // null = 读不到（窗口刚销毁 / 采样点全在屏幕外）：保持现状
          if (!alive || l === null) return;
          const next = prev ? l > 0.52 : l > 0.62;
          if (next !== prev) {
            prev = next;
            setLabelDark(next);
          }
        })
        .catch(() => {});
    };
    lumaTick.current = tick;
    tick();
    const timer = window.setInterval(tick, 1500);
    return () => {
      alive = false;
      lumaTick.current = () => {};
      window.clearInterval(timer);
    };
  }, [id]);

  const refresh = useCallback(() => {
    api
      .view(id)
      .then((v) => {
        // 形态变了（嵌入 / 移出任务栏）：上一条拖动会话到此结束。
        // 后面那些「程序自己摆窗口」引起的 onMoved 不该再触发吸附。
        if (v.in_bar !== barRef.current) dragStartedRef.current = 0;
        setView(v);
        barRef.current = v.in_bar;
        inGroupRef.current = !!v.in_group;
      })
      .catch(() => {});
  }, [id]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // 数据到手 → 等两帧（确保这一帧的像素真的提交了）→ 再告诉后端「可以显示了」。
  // 切「置于顶层」时后端就是等这个信号才把新窗口露出来，否则会先闪一块空白。
  useEffect(() => {
    if (!view || readySent.current) return;
    let a = 0;
    let b = 0;
    a = requestAnimationFrame(() => {
      b = requestAnimationFrame(() => {
        readySent.current = true;
        api.boxReady(id).catch(() => {});
      });
    });
    return () => {
      cancelAnimationFrame(a);
      cancelAnimationFrame(b);
    };
  }, [view, id]);

  // 内容变化（拖入 / 放回桌面 / 从面板移除 / 改名）由后端广播，
  // 这样角标和名字不用等下一次点击才更新
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    listen<string>("box:changed", (e) => {
      if (e.payload === id) refresh();
    }).then((f) => {
      if (alive) un = f;
      else f();
    });
    return () => {
      alive = false;
      un?.();
    };
  }, [id, refresh]);

  // 从桌面把快捷方式拖进来
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    getCurrentWebview()
      .onDragDropEvent(async (e) => {
        const p = e.payload;
        if (p.type === "over" || p.type === "enter") {
          setOver(true);
        } else if (p.type === "leave") {
          setOver(false);
        } else if (p.type === "drop") {
          setOver(false);
          if (p.paths && p.paths.length) {
            await api.importPaths(id, p.paths).catch(() => {});
            refresh();
          }
        }
      })
      .then((f) => {
        if (alive) un = f;
        else f();
      });
    return () => {
      alive = false;
      un?.();
    };
  }, [id, refresh]);

  // 拖动停下来（100ms 内没再移动）→ 吸附 + 落盘坐标。
  //
  // 两个坑：
  //  1. 拖动期间收不到 pointerup，光靠「没动静了」不成立 —— 用户可能只是停住手、
  //     鼠标还按着。这时动窗口会和系统拖动循环抢位置（抖 / 拖不动），所以后端
  //     `snap_box` 会查左键状态，还按着就回 null，这里过 150ms 再问。
  //  2. 吸附会 setPosition，又触发一次 onMoved；靠 snapping 标志挡住重入，
  //     后端返回 false（位置没变）时也不再重截背景。
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    let timer: number | undefined;
    let fallback: number | undefined;
    let retry = 0;
    let snapping = false;

    const run = () => {
      if (!alive || snapping) return;
      snapping = true;
      // 任务栏盒子有自己的落位规则：横坐标夹在任务栏内、纵坐标钉在任务栏上沿
      const snap = barRef.current ? api.snapBar(id) : api.snapBox(id);
      snap
        .then((moved) => {
          if (!alive) return;
          if (moved === null) {
            // 鼠标还按着，等它松开
            if (retry++ < 60) timer = window.setTimeout(run, 150);
            return;
          }
          // 吸附已经写了新坐标，别让 400ms 的兜底把旧坐标盖回去
          if (moved) window.clearTimeout(fallback);
        })
        .catch(() => {})
        .finally(() => {
          snapping = false;
        });
    };

    getCurrentWindow()
      .onMoved(() => {
        // 只有「用户这次拖动」引起的移动才需要吸附 / 落盘。
        //
        // 程序自己摆窗口（改设置后重排、嵌入/移出任务栏时重建、显示配置变化、
        // 任务栏纠偏）同样会触发 onMoved。这时去问后端，`snapBar` 会拿**光标**
        // 判断「是不是被拖出去了」—— 光标可能正停在设置页上，于是把胶囊误判成
        // 拖出任务栏、变回普通盒子落到光标处（「盒子全跳到设置窗口下面」）。
        //
        // 拖动开始时才写这个时间戳；新页面（重建出来的）初值是 0，8 秒也没人
        // 拖过，所以程序摆位一律不触发吸附。
        const started = dragStartedRef.current;
        if (!started || Date.now() - started > 8000) return;
        // 胶囊团：其它成员由后端的跟随线程按「锚点 + 偏移」摆位（拖动开始时就已经起好线程，
        // 这里不再逐帧发 invoke —— 拖动期间主线程待在系统的拖动模态循环里，跨线程摆位
        // 会互相等死，用户实测就是「一拖就卡死」）。
        // 兜底落盘：万一吸附那边一直没轮到（比如用户按着鼠标停了很久），
        // 至少这个 400ms 后会把拖动后的坐标写进 config，不会丢位置。
        // 它只写坐标、不动窗口，所以拖动途中调用也安全。
        window.clearTimeout(fallback);
        fallback = window.setTimeout(() => {
          // 位置由后端用 Win32 读：payload 是父客户区坐标，直接存会把配置写坏
          api.moveBox(id).catch(() => {});
          // 换了地方 → 底下那块壁纸的明暗可能也变了，名字颜色立刻重判一次，
          // 不用等 3 秒的轮询（拖到白色区域松手，字要马上变深）
          lumaTick.current();
        }, 400);

        window.clearTimeout(timer);
        retry = 0;
        timer = window.setTimeout(run, 100);
      })
      .then((f) => {
        if (alive) un = f;
        else f();
      });
    return () => {
      alive = false;
      un?.();
      window.clearTimeout(timer);
      window.clearTimeout(fallback);
    };
  }, [id]);

  const onPointerDown = (e: React.PointerEvent) => {
    if (e.button !== 0) return;
    drag.current = { x: e.screenX, y: e.screenY, moved: false };
  };

  const onPointerMove = (e: React.PointerEvent) => {
    const d = drag.current;
    if (!d || d.moved) return;
    if (Math.hypot(e.screenX - d.x, e.screenY - d.y) > 4) {
      d.moved = true;
      dragStartedRef.current = Date.now();
      setDragging(true);
      // 确认是拖动（不是点击）→ 先把面板收掉。
      //
      // 面板是挂在盒子旁边的，且**不会跟着盒子走**；盒子一旦移动，面板就悬在旧位置上了。
      // 更重要的是它会把后面的「点桌面自动收起」给堵死：
      //   拖动时左键按住 → 面板失焦 → 前端等松手 → 松手时光标落在**新的**盒子位置上,
      //   `cursor_on_box` 判为 true（那条规则是给「再点一次盒子收起面板」用的）→ 不复判关闭。
      //   而此后焦点是从盒子转到桌面，**面板自己没有再失焦**，`onFocusChanged` 不会再来,
      //   于是点桌面空白处也永远收不回来（用户反馈的就是这个）。
      // 必须在 startDragging 之前发：系统拖动循环会一直占着前端，之后再发指令到不了。
      api.closePanel().catch(() => {});
      if (inGroupRef.current) {
        // 胶囊团：先把「其它成员的偏移」算好再进系统拖动循环。
        // 顺序不能反 —— startDragging 会一直占着，之后前端再发指令就到不了了。
        api
          .groupDragBegin(id)
          .catch(() => {})
          .finally(() => {
            getCurrentWindow().startDragging().catch(() => {});
          });
      } else {
        getCurrentWindow().startDragging().catch(() => {});
      }
    }
  };

  const onPointerUp = () => {
    const d = drag.current;
    drag.current = null;
    setDragging(false);
    if (d && !d.moved) {
      // 面板是「点一次弹出、再点一次收起」的开关。双击的第二下间隔往往不到 300ms，
      // 直接放过会让面板刚打开就被关上（看起来像点了没反应）。
      const now = Date.now();
      if (now - lastClickRef.current < 300) return;
      lastClickRef.current = now;
      api.openPanel(id).catch(() => {});
    }
  };

  // 拖动途中「悬停切换形态」：每 60ms 问一次后端。
  //
  // 用户要求：拖到任务栏上一半以上停住一会儿就直接嵌进去、把胶囊往上顶住一会儿
  // 就直接退回桌面 —— 都不用等松手。判断逻辑全在后端（要读光标、系统拖动状态），
  // 具体的判定时长也由后端记（`HOVER_MS`，前端只负责定时问）。
  //
  // 返回 "idle" 说明这次拖动结束了、或者后端甩不掉系统拖动循环：停下并复位拖动标记；
  // "embed"/"unembed" 说明已经切换，本页面马上会被销毁，也不用再问了。
  useEffect(() => {
    if (!dragging) return;
    let alive = true;
    let busy = false;
    const timer = window.setInterval(() => {
      if (busy) return;
      busy = true;
      api
        .barHover(id)
        .then((r) => {
          if (!alive || !r) return;
          window.clearInterval(timer);
          if (r === "idle") setDragging(false);
        })
        .catch(() => {})
        .finally(() => {
          busy = false;
        });
    }, 60);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [dragging, id]);

  // 系统拖动结束后不会有 pointerup（鼠标被窗口移动循环接管），
  // 用「拖动结束后的第一次 mousemove」把标记清掉 —— 那时光标还在盒子上，
  // 靠 :hover 继续显示灰底；等光标移开，两层状态都没了才回到透明。
  useEffect(() => {
    const release = (e: Event) => {
      if (Date.now() - dragStartedRef.current <= 350) return;
      // 左键还按着就说明拖动还没结束：拖动途中万一真收到了 mousemove，
      // 也不能把标记清掉 —— 清了悬停切换那套轮询就停了。
      const ev = e as MouseEvent;
      if (typeof ev.buttons === "number" && ev.buttons === 1) return;
      setDragging(false);
      // 胶囊团：拖动结束兜底收个尾（正常由后端吸附时的 group_settle 完成，
      // 这里只在「没走到吸附」的情况下补一次对齐 + 落盘，没有会话时是空操作）
      if (inGroupRef.current) api.groupEnd(id).catch(() => {});
      // 拖动会话到此结束：2 秒后把「拖动开始时间」也清掉，之后任何窗口移动
      // （改设置重排、重建、任务栏纠偏）都不再被当成用户拖动去调吸附。
      // 留 2 秒是因为松手到吸附真正执行之间还有 100~400ms 的空档。
      const startedAt = dragStartedRef.current;
      window.setTimeout(() => {
        if (dragStartedRef.current === startedAt) dragStartedRef.current = 0;
      }, 2000);
    };
    window.addEventListener("pointerup", release);
    window.addEventListener("mousemove", release);
    return () => {
      window.removeEventListener("pointerup", release);
      window.removeEventListener("mousemove", release);
    };
  }, []);

  // 舞台缩放：目标缩放 / 真实 DPR。见上面 monScale 的说明。
  //
  // 舞台的**设计尺寸**由后端给（view.w / view.h）：
  //  - 普通盒子 80×80；
  //  - 任务栏盒子是横向胶囊，高 = 任务栏厚度、宽 = 厚度 + 右侧两个汉字的位置。
  // 两端都按同一套逻辑像素排版，乘上 monScale/dpr 之后正好等于窗口的物理像素，
  // 图标/文字不会被圆角区域裁掉。
  const inBar = !!view?.in_bar;
  const designW = view?.w && view.w > 0 ? view.w : 80;
  const designH = view?.h && view.h > 0 ? view.h : 80;
  const stageScale = monScale > 0 && dpr > 0 ? monScale / dpr : 1;
  // 任务栏胶囊里图标占高度的比例（剩下的是右侧文字区）
  const barIcon = designH * 0.66;
  // 设置里开了「嵌入任务栏时隐藏名称」：胶囊变成正方形，只画图标
  const barSolo = inBar && !!view?.bar_hide_label;
  // 右侧只放两个汉字：名字去掉空白后取前两个字符
  const shortName = (view?.name ?? "").replace(/\s+/g, "").slice(0, 2) || "…";

  return (
    <div
      className={"box-stage" + (inBar ? " bar" : "")}
      style={{
        width: designW,
        height: designH,
        transform: `scale(${stageScale})`,
      }}
    >
      <div
        className={
          "box-tile" +
          (inBar ? " bar" : "") +
          (barSolo ? " solo" : "") +
          // 浅色壁纸 → 深色字（胶囊有自己的底色，不受影响）
          (!inBar && labelDark ? " label-dark" : "") +
          (over ? " drop" : "") +
          (dragging ? " dragging" : "")
        }
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onContextMenu={(e) => {
          e.preventDefault();
          // 盒子只有 80×80 且被裁成圆角，菜单必须另外开窗口
          api.showBoxMenu(id).catch(() => {});
        }}
      >
        {view && view.count > 0 && <div className="box-count">{view.count}</div>}

        <div
          className="box-icon"
          style={inBar ? { width: barIcon, height: barIcon } : undefined}
        >
          {view?.icon ? (
            <img src={view.icon} alt="" draggable={false} />
          ) : (
            <BoxGlyph />
          )}
        </div>

        {inBar && !barSolo ? (
          <div
            className="box-label"
            style={{
              fontSize: designH * 0.3,
              maxWidth: Math.max(designW - barIcon - 6, 10),
            }}
          >
            {shortName}
          </div>
        ) : inBar ? null : (
          <div className="box-name">{view?.name ?? "…"}</div>
        )}
      </div>
    </div>
  );
}
