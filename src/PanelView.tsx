import { useCallback, useEffect, useRef, useState } from "react";
import { flushSync } from "react-dom";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { listen } from "@tauri-apps/api/event";
import { api, BoxView as BoxViewData, Item, PanelTarget } from "./api";
import type { UpdateState } from "./api";
import { useTheme } from "./theme";

type MenuState =
  | { x: number; y: number; kind: "header" }
  /** items = 图标上的菜单，对选中集生效（单选 1 个，多选 N 个） */
  | { x: number; y: number; kind: "items"; files: string[] };

const MENU_W = 178;

/// 「按名称排序」用的比较器：中文按拼音、数字按数值（「第2项」排在「第10项」前）
const NAME_CMP = new Intl.Collator("zh-Hans-CN", { numeric: true, sensitivity: "base" });

export default function PanelView() {
  const [boxId, setBoxId] = useState("");
  const [view, setView] = useState<BoxViewData | null>(null);
  const [items, setItems] = useState<Item[]>([]);
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [toast, setToast] = useState("");
  const [over, setOver] = useState(false);
  /** 更新状态：面板窗口是复用的、页面只挂载一次，所以「有更新」不能靠挂载时读一次，
   *  要挂 update:data 事件（和设置页页脚同一条数据源）。 */
  const [upd, setUpd] = useState<UpdateState | null>(null);
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    api
      .updateState()
      .then((v) => {
        if (alive) setUpd(v);
      })
      .catch(() => {});
    listen<UpdateState>("update:data", (e) => {
      if (alive) setUpd(e.payload);
    })
      .then((f) => {
        if (alive) un = f;
        else f();
      })
      .catch(() => {});
    return () => {
      alive = false;
      un?.();
    };
  }, []);
  /** 毛玻璃底图：面板背后那块屏幕的模糊快照（后端在窗口显示前截好） */
  const [backdrop, setBackdrop] = useState<string | null>(null);
  /** 多选（Ctrl+点击 / 空白处框选）的文件名；右键菜单对选中集生效 */
  const [sel, setSelState] = useState<string[]>([]);
  const selRef = useRef<string[]>([]);
  const setSel = (files: string[]) => {
    selRef.current = files;
    setSelState(files);
  };
  /** Ctrl+点击：在选中集里加上 / 去掉一个（不启动程序） */
  const toggleSel = (file: string) => {
    const cur = selRef.current;
    setSel(cur.includes(file) ? cur.filter((f) => f !== file) : [...cur, file]);
  };
  /** 框选矩形（client 坐标，null = 没在框） */
  const [band, setBand] = useState<{ x0: number; y0: number; x1: number; y1: number } | null>(null);
  const bandRef = useRef<{
    x0: number;
    y0: number;
    x1: number;
    y1: number;
    on: boolean;
    /** 按下那一刻的 Ctrl 状态：追加还是替换由「按下时」定，别看松手时 */
    ctrl: boolean;
  } | null>(null);
  /**
   * 「Ctrl 是否按着」的**同步可读镜像**。
   *
   * 多选必须在 `pointerdown` 那一刻就决定是「打开程序」还是「切选」，不能等一下 IPC。
   * 三路来源合成，只要有一路活着，点击前这里就是准的：
   *  ① 鼠标事件自带的修饰键（`e.ctrlKey`）—— WebView2 在部分窗口状态下不给，靠不住；
   *  ② 窗口的 keydown/keyup —— 面板聚焦时精确，窗口没焦点时收不到；
   *  ③ 后端每 40ms 轮询物理键后广播的 `ctrl:changed` —— 不依赖焦点，兜住 ①②。
   */
  const ctrlHeldRef = useRef(false);
  /** 本帧事件或镜像里任一路说 Ctrl 按着，就按 Ctrl 处理 */
  const ctrlOn = (e: { ctrlKey?: boolean; metaKey?: boolean }) =>
    !!(e.ctrlKey || e.metaKey || ctrlHeldRef.current);

  useEffect(() => {
    const sync = (v: boolean) => {
      ctrlHeldRef.current = v;
    };
    const onDown = (e: KeyboardEvent) => {
      if (e.key === "Control" || e.key === "Meta") sync(true);
    };
    const onUp = (e: KeyboardEvent) => {
      if (e.key === "Control" || e.key === "Meta") sync(false);
    };
    // 窗口失焦时收不到 keyup，Ctrl 状态会卡在 true —— 失焦直接复位
    const onBlur = () => sync(false);
    window.addEventListener("keydown", onDown, true);
    window.addEventListener("keyup", onUp, true);
    window.addEventListener("blur", onBlur);

    let alive = true;
    let un: (() => void) | undefined;
    listen<boolean>("ctrl:changed", (e) => {
      if (alive) sync(!!e.payload);
    })
      .then((f) => {
        if (alive) un = f;
        else f();
      })
      .catch(() => {});
    // 挂载时先拉一次：之前按下的键不会有事件补发
    api
      .ctrlDown()
      .then((v) => {
        if (alive) sync(v);
      })
      .catch(() => {});

    return () => {
      alive = false;
      window.removeEventListener("keydown", onDown, true);
      window.removeEventListener("keyup", onUp, true);
      window.removeEventListener("blur", onBlur);
      un?.();
    };
  }, []);
  /** 手势序号：异步回来的物理键状态只认最后一次 pointerdown */
  const gestureRef = useRef(0);
  /** items 的最新值（快捷键里读，避免闭包拿到旧值） */
  const itemsRef = useRef(items);
  useEffect(() => {
    itemsRef.current = items;
  }, [items]);
  const renameRef = useRef<HTMLInputElement>(null);
  // 拖动状态：指针按下后先「观望」，移动超过阈值再决定是「窗内排序」还是「拖出到桌面」
  const dragRef = useRef<{
    file: string;
    from: number;
    sx: number;
    sy: number;
    mode: "idle" | "reflow" | "out";
    to: number;
    /** 指针最后一次位置（物理量级的 client 坐标），松手时判断落点用 */
    lx: number;
    ly: number;
    /** 按下那一刻格子占的矩形：松手还在里面就算是「点击」而不是「拖动」 */
    rect: { l: number; t: number; r: number; b: number };
    /** 按下这一刻的选中集：多选后拖动 = 整批拖出 */
    sel: string[];
  } | null>(null);
  const gridRef = useRef<HTMLDivElement>(null);
  /** 拖动开始时的格子几何（格子宽高 / 间距 / 列数），拖动过程中不变 */
  const metricsRef = useRef<{ cw: number; ch: number; gapX: number; gapY: number; cols: number; baseX: number; baseY: number } | null>(null);
  /** 排序预览：被拖的格子跟手，其它格子让位 */
  const [sort, setSort] = useState<{ file: string; from: number; to: number; dx: number; dy: number } | null>(null);
  /** 落地瞬间关掉过渡，避免「回到原格再飞过去」的错觉 */
  const [noAnim, setNoAnim] = useState(false);
  /** 正在把图标拖到桌面：这时面板失焦是「拖拽过程」的一部分，不能收起面板 */
  const outDragRef = useRef(false);
  /**
   * 点击抑制：拖动 / 多选手势之后**不要**顺手把程序打开。
   *
   * 存的是「抑制到什么时候」而不是「布尔 + setTimeout」—— 面板窗口是隐藏起来复用的，
   * Chromium 对隐藏页的定时器会降频（最长 1 分钟才跑一次），布尔标记一旦卡在 true，
   * 之后所有点击都变成「有按下效果但什么也不做」。
   */
  const suppressUntil = useRef(0);
  const suppress = (ms = 250) => {
    suppressUntil.current = Date.now() + ms;
  };
  const isSuppressed = () => Date.now() < suppressUntil.current;
  /** 这次手势已经按「多选」处理过了：后面的 click 不能再切一次（否则等于没选） */
  const cellHandled = useRef(false);
  /**
   * 按下时按着 Ctrl，但**还没**切选：到底是「Ctrl+点击（切换选中）」还是
   * 「Ctrl+拖动（把这批一起拖出去）」，要等松手才知道。
   *
   * 以前是按下就切选，于是「按住 Ctrl 拖已选中的图标」先把这一下从选中集里踢掉，
   * 再拖就只剩它自己（用户反馈：多选后拖动只拖出去一个）。
   */
  const pendingToggle = useRef<string | null>(null);
  /** 当前展示的盒子 id（事件回调里读，避免闭包拿到旧值） */
  const boxIdRef = useRef("");
  useEffect(() => {
    boxIdRef.current = boxId;
  }, [boxId]);
  /** 后端「这次显示」的序号，回报时原样带回去 */
  const seqRef = useRef(0);

  // 外观主题：面板只读（改是在设置窗口里改，改动通过 theme:changed 广播过来）
  const [theme] = useTheme();

  const close = useCallback(() => {
    api.closePanel().catch(() => {});
  }, []);

  const refresh = useCallback(async (id: string) => {
    if (!id) return;
    // 内容变了（拖入/拖出/移除/放回）：选择集作废重来
    setSel([]);
    try {
      const [v, list] = await Promise.all([api.view(id), api.items(id)]);
      setView(v);
      setItems(list);
    } catch {
      /* 盒子可能已被删除 */
    }
  }, []);

  /**
   * 切换到「另一个」盒子：**立刻**清空旧内容，再取新数据，画完首帧后回报后端。
   *
   * 面板窗口是复用的，如果不先清空，后端把窗口显示出来的那一帧里还是上一个盒子的
   * 图标 —— 看起来就像「面板缓存了上一次的内容」。后端会等这次回报才 show。
   */
  const loadFresh = useCallback(async (id: string, seq: number) => {
    seqRef.current = seq;
    setView(null);
    setItems([]);
    setSel([]);
    setMenu(null);
    setRenaming(null);
    try {
      const [v, list, bg] = await Promise.all([api.view(id), api.items(id), api.backdrop().catch(() => null)]);
      if (seqRef.current !== seq) return; // 已经切走了，别用旧结果覆盖
      // 同步提交：DOM 里立刻换成新盒子的内容，然后马上回报。
      // 这里**不能**用 requestAnimationFrame / setTimeout 等「下一帧」——
      // 面板此刻是隐藏窗口（visibilityState = hidden），Chromium 会把 rAF 停掉、
      // 把 setTimeout 降频，等它来回报会让面板晚 200~300ms 才出现。
      // 隐藏窗口也不需要提前「画好」：show 出来时浏览器会按当前 DOM 直接出帧（实测 ~30ms）。
      flushSync(() => {
        setView(v);
        setItems(list);
        setBackdrop(bg);
      });
    } catch {
      /* 盒子可能已被删除 */
    }
    if (seqRef.current === seq) api.panelReady(id, seq).catch(() => {});
  }, []);

  // 主进程通知当前要展示哪个盒子。
  // 注意：这里必须**主动 refresh**。只 setBoxId 是不够的 —— 再次打开同一个盒子时
  // id 没变，`useEffect([boxId])` 不会重跑，面板就一直显示上次的旧数据
  //（拖进来新图标却不显示，就是这个原因）。
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    listen<PanelTarget>("panel:target", async (e) => {
      const { id, seq } = e.payload;
      setBoxId(id);
      if (id === boxIdRef.current) {
        // 还是同一个盒子：只刷新数据，不清空（清了图标会闪一下消失再出现）
        await refresh(id);
        api.panelReady(id, seq).catch(() => {});
        return;
      }
      await loadFresh(id, seq);
    }).then((f) => {
      if (alive) un = f;
      else f();
    });
    return () => {
      alive = false;
      un?.();
    };
  }, [refresh, loadFresh]);

  // 开关毛玻璃时后端会重截底图并推过来（它得先把面板藏一下，所以位置由后端决定）
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    listen<string | null>("panel:backdrop", (e) => {
      if (alive) setBackdrop(e.payload);
    })
      .then((f) => {
        if (alive) un = f;
        else f();
      })
      .catch(() => {});
    return () => {
      alive = false;
      un?.();
    };
  }, []);

  // 页面被重载（F5）后状态全丢，向后端要回当前盒子，避免变成空面板
  useEffect(() => {
    api
      .panelTarget()
      .then((t) => {
        if (!t) return;
        if (t.id === boxIdRef.current) return;
        boxIdRef.current = t.id;
        setBoxId(t.id);
        loadFresh(t.id, t.seq);
      })
      .catch(() => {});
  }, [loadFresh]);

  useEffect(() => {
    refresh(boxId);
  }, [boxId, refresh]);

  // 盒子内容变化（拖入/拖出/改名）由后端广播，面板正在显示这个盒子就立刻刷新
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    listen<string>("box:changed", (e) => {
      if (e.payload === boxId) refresh(boxId);
    }).then((f) => {
      if (alive) un = f;
      else f();
    });
    return () => {
      alive = false;
      un?.();
    };
  }, [boxId, refresh]);

  // 失焦 / Esc 收起面板
  useEffect(() => {
    let alive = true;
    let unFocus: (() => void) | undefined;

    /** 失焦后的最终复判：还开着、没焦点、前台不是自己人 → 收起 */
    const decideClose = async () => {
      const w = getCurrentWindow();
      try {
        // 换盒子时窗口会先藏起来再显示，藏起来那一拍的失焦不是用户操作。
        // 用「窗口可见性」而不是「宽限期」来判断：宽限期会把用户刚弹出就点桌面
        // 的那一下也吞掉，表现就是「点了桌面收不回来」。
        if (!(await w.isVisible())) return;
        if (await w.isFocused()) return;
        if (await api.foregroundIsSelf()) return;
        // 松手时光标落在面板里（刚把图标拖进来）→ 保持打开
        if (await api.cursorInPanel()) return;
        // 光标停在某个收纳盒上 → 用户是去点盒子了。点一次弹出、再点一次收起，
        // 这个「收起」必须由盒子那边判断（open_panel 看得到面板还开着）；
        // 如果这里先把它收掉，盒子再点就永远只会「打开」，看起来像点第二次没反应。
        if (await api.cursorOnBox()) return;
      } catch {
        /* ignore */
        /* 判断不出来就按原来的行为收起 */
      }
      close();
    };

    /** 等左键松手，再走一遍复判（拖拽过程中面板失焦不能立刻关） */
    const awaitReleaseThenClose = () => {
      let tries = 0;
      const tick = async () => {
        if (tries++ > 120) return; // 兜底：最多等 12 秒
        let down: boolean;
        try {
          down = await api.mouseLeftDown();
        } catch {
          await decideClose();
          return;
        }
        if (down) {
          window.setTimeout(tick, 100);
          return;
        }
        await decideClose();
      };
      window.setTimeout(tick, 100);
    };

    getCurrentWindow()
      .onFocusChanged(({ payload }) => {
        if (payload) return;
        setMenu(null);
        // 把图标拖到桌面的过程中面板必然会失焦，结束后也不该顺手收起
        if (outDragRef.current) return;
        // 点收纳盒、点右键菜单、切置顶重建盒子窗口都会让面板失焦，但那不是「离开」——
        // 这里隔一拍复判：焦点还在自己窗口上就不收起，避免再点盒子时闪一下。
        window.setTimeout(async () => {
          if (outDragRef.current) return;
          // 左键按着 = 用户可能正拖着东西（典型场景：从桌面拖图标往面板里放）。
          // 按下那一下面板就失焦了，但拖拽还没结束 —— 这时收起会把弹窗提前关掉，
          // 图标拖进来直接落空。等松手再复判。
          try {
            if (await api.mouseLeftDown()) {
              awaitReleaseThenClose();
              return;
            }
          } catch {
            /* ignore：判断不出来按老行为收起 */
          }
          await decideClose();
        }, 90);
      })
      .then((f) => {
        if (alive) unFocus = f;
        else f();
      });
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        if (menu) setMenu(null);
        else if (renaming !== null) setRenaming(null);
        // 有选中时先取消选择，再按一次才关面板（和资源管理器一致）
        else if (selRef.current.length > 0) setSel([]);
        else close();
      }
      // Ctrl+A 全选（改名输入框里的 Ctrl+A 是输入框自己的全选，不拦）
      if ((e.ctrlKey || e.metaKey) && (e.key === "a" || e.key === "A")) {
        const t = e.target as HTMLElement | null;
        if (t && (t.tagName === "INPUT" || t.isContentEditable)) return;
        e.preventDefault();
        setSel(itemsRef.current.map((x) => x.file));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      alive = false;
      unFocus?.();
      window.removeEventListener("keydown", onKey);
    };
  }, [close, menu, renaming]);

  // 面板内也能接住拖入
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    getCurrentWebview()
      .onDragDropEvent(async (e) => {
        const p = e.payload;
        if (p.type === "over" || p.type === "enter") setOver(true);
        else if (p.type === "leave") setOver(false);
        else if (p.type === "drop") {
          setOver(false);
          if (p.paths && p.paths.length && boxId) {
            const r = await api.importPaths(boxId, p.paths).catch(() => null);
            if (r && r.skipped.length) showToast(r.skipped[0]);
            refresh(boxId);
            // 拖进来了：焦点还给面板，不用再点一下才能继续操作
            getCurrentWindow().setFocus().catch(() => {});
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
  }, [boxId, refresh]);

  // 进入改名状态时全选，方便直接覆盖输入。
  // 依赖必须是「是否在改名」这个布尔值，**不能**是 renaming 本身 ——
  // 那样每敲一个字都会重跑一次 select()，把刚输入的内容全选，
  // 下一个字符进来就把它替换掉，表现为「新内容覆盖之前的内容 / 名字改不对」。
  const renameActive = renaming !== null;
  useEffect(() => {
    if (renameActive) renameRef.current?.select();
  }, [renameActive]);

  const toastTimer = useRef<number | undefined>(undefined);
  const showToast = (text: string) => {
    setToast(text);
    window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToast(""), 2600);
  };

  // —— 拖动：窗内拖动 = 调整顺序（安卓那种把别的图标挤走）；拖出窗口 = 放回桌面 ——
  const measureGrid = () => {
    const g = gridRef.current;
    if (!g) return null;
    const first = g.querySelector<HTMLElement>(".cell");
    const cs = getComputedStyle(g);
    const cols = cs.gridTemplateColumns.split(" ").filter((x) => x).length || 4;
    // 注意：offsetLeft/offsetTop 的参照物是最近的「定位祖先」(.panel)，不是 grid，
    // 直接用会把面板头的高度重复减一次 → 落点永远高一格。这里用 grid 自己的 padding。
    return {
      cw: first?.offsetWidth ?? 0,
      ch: first?.offsetHeight ?? 0,
      gapX: parseFloat(cs.columnGap) || 4,
      gapY: parseFloat(cs.rowGap) || 4,
      cols,
      baseX: parseFloat(cs.paddingLeft) || 0,
      baseY: parseFloat(cs.paddingTop) || 0,
    };
  };

  /** 指针落在第几格（按格子中心就近取，最后一格之后的空隙算最后一格） */
  const indexAt = (clientX: number, clientY: number, count: number) => {
    const g = gridRef.current;
    const m = metricsRef.current;
    if (!g || !m) return 0;
    const r = g.getBoundingClientRect();
    const x = clientX - r.left + g.scrollLeft - m.baseX;
    const y = clientY - r.top + g.scrollTop - m.baseY;
    const col = Math.max(0, Math.min(m.cols - 1, Math.floor((x + m.gapX / 2) / (m.cw + m.gapX))));
    const row = Math.max(0, Math.floor((y + m.gapY / 2) / (m.ch + m.gapY)));
    return Math.max(0, Math.min(count - 1, row * m.cols + col));
  };

  const startDragOut = (files: string[]) => {
    suppress(400);
    outDragRef.current = true;
    setSort(null);
    api
      .dragOut(boxId, files)
      .then((r) => {
        if (r === "moved") {
          showToast(files.length > 1 ? `已拖到桌面（${files.length} 个）` : "已拖到桌面");
        }
      })
      .catch((err) => showToast(String(err)))
      .finally(() => {
        // refresh 会一并清掉选择集
        refresh(boxId);
        window.setTimeout(() => {
          outDragRef.current = false;
          // 拖拽把前台交给了落点（桌面），面板此后再也收不到失焦事件 ——
          // 把前台抢回来，用户「点桌面」时才能照常收起面板。
          api.refocusPanel().catch(() => {});
        }, 250);
      });
  };

  const onCellPointerDown = (e: React.PointerEvent, it: Item, idx: number) => {
    if (e.button !== 0) return;
    // 先记下这一刻的 Ctrl 状态：事件里带的是首选来源，镜像（keydown / 后端推送）兜底
    const g = ++gestureRef.current;
    cellHandled.current = false;
    pendingToggle.current = null;
    const ctrlEvt = ctrlOn(e);
    if (ctrlEvt) {
      // Ctrl 按着 → 先**不**切选。
      //
      // 按下去这一刻还不知道用户要「点击（切换选中）」还是「拖动（把这批拖到桌面）」。
      // 立刻切选会把这一下从选中集里踢掉：按住 Ctrl 拖已选中的图标去桌面，
      // 结果只拖出去一个（用户实测）。先记下来，松手时确认没拖动再切。
      pendingToggle.current = it.file;
      cellHandled.current = true; // 后面的 click 别再处理一次
    } else {
      // 事件没带 Ctrl（WebView2 在部分窗口状态下不给鼠标事件带修饰键）→
      // 当场问一次后端物理键状态。回来是「按着」就按 Ctrl 手势处理（同样推迟到松手）。
      //
      // 以前是「立刻切选」：一旦用户其实是在拖动，这一下就把选中集改坏了。
      api
        .ctrlDown()
        .then((v) => {
          if (!v || gestureRef.current !== g) return;
          ctrlHeldRef.current = true;
          const d = dragRef.current;
          // 已经松手（dragRef 被清空）或者已经在拖了 → 交给 click / 拖动逻辑
          if (!d || d.file !== it.file || d.mode !== "idle") return;
          pendingToggle.current = it.file;
          cellHandled.current = true;
        })
        .catch(() => {});
    }
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    dragRef.current = {
      file: it.file,
      from: idx,
      sx: e.screenX,
      sy: e.screenY,
      mode: "idle",
      to: idx,
      lx: e.clientX,
      ly: e.clientY,
      rect: { l: r.left, t: r.top, r: r.right, b: r.bottom },
      sel: selRef.current,
    };
    (e.currentTarget as HTMLElement).setPointerCapture?.(e.pointerId);
  };

  /**
   * 这次拖动要带出去的图标：多选状态里拖其中一个 = **整批**。
   *
   * 现场读 `selRef` 而不是用按下时的快照：最后一次 Ctrl+点击的选中是异步落到
   * `selRef` 上的（要等一次 `ctrlDown` 往返），手快的话快照可能还是旧的
   * （用户反馈：多选后拖动只拖出去一个）。
   */
  const dragBatch = (file: string) => {
    const cur = selRef.current;
    return cur.length > 1 && cur.includes(file) ? cur.slice() : [file];
  };

  const onCellPointerMove = (e: React.PointerEvent, it: Item) => {
    const d = dragRef.current;
    if (!d || d.file !== it.file) return;
    d.lx = e.clientX;
    d.ly = e.clientY;
    if (d.mode === "out") return;
    const dx = e.screenX - d.sx;
    const dy = e.screenY - d.sy;
    // 指针跑到面板窗口外面 → 这是「拖到桌面」，交给原生 OLE 拖拽；
    // 还留在窗口里 → 窗内排序。留 14px 余量，避免贴边时误判。
    const outside =
      e.clientX < -14 ||
      e.clientY < -14 ||
      e.clientX > window.innerWidth + 14 ||
      e.clientY > window.innerHeight + 14;
    if (outside) {
      // 已经进入排序态又拖出窗口：撤掉排序预览，改走拖出。用户在窗口边缘犹豫时
      // 也能顺理成章地把图标拖到桌面上。
      d.mode = "out";
      setSort(null);
      // 多选后拖动其中一个 = 整批拖出去；没多选（或拖的是选中集之外的图标）
      // 就只拖这一个。多文件走 CF_HDROP，资源管理器按移动处理。
      const batch = dragBatch(it.file);
      d.sel = batch;
      startDragOut(batch);
      return;
    }
    if (d.mode === "idle") {
      if (Math.hypot(dx, dy) < 6) return;
      metricsRef.current = measureGrid();
      d.mode = "reflow";
    }
    if (d.mode !== "reflow") return;
    d.to = indexAt(e.clientX, e.clientY, items.length);
    setSort({ file: d.file, from: d.from, to: d.to, dx, dy });
  };

  const onCellPointerUp = () => {
    const d = dragRef.current;
    dragRef.current = null;
    // Ctrl 手势且没拖动 = 一次「Ctrl+点击」：现在才切选（拖动的话整批都保留）。
    const toggle = pendingToggle.current;
    pendingToggle.current = null;
    if (toggle && (!d || d.mode === "idle")) {
      toggleSel(toggle);
      suppress(); // 别顺手把程序打开
    }
    if (!d || d.mode !== "reflow") return;
    setSort(null);
    // 落回自己那一格：这里已经进入过 reflow 态（位移 ≥ 6px），说明是「拖了一下又放回」，
    // 不是点击 —— 绝不能让它冒泡成 click 把程序打开。以前这里 `return` 让 click 冒泡，
    // 表现就是「拖动一小段距离再放回原位，程序被打开了」。
    if (d.to === d.from) {
      const r = d.rect;
      const inside =
        d.lx >= r.l - 2 && d.lx <= r.r + 2 && d.ly >= r.t - 2 && d.ly <= r.b + 2;
      if (inside) {
        suppress(250); // 拖动了，吃掉紧随其后的 click
        return;
      }
    }
    // 拖动过就吃掉紧随其后的 click。
    // 以前只在「落点变了」时才抑制，于是「拖到空白处松手」漏了：空白处被
    // indexAt 夹到最后一个格子，如果拖的正好是最后一个图标，to === from 直接
    // return，click 没被吃掉 → 图标弹回原位的同时把程序打开了。
    suppress(250);
    if (d.to === d.from) return;
    const next = [...items];
    const [moved] = next.splice(d.from, 1);
    next.splice(d.to, 0, moved);
    setNoAnim(true);
    setItems(next);
    api
      .reorder(
        boxId,
        next.map((x) => x.file),
      )
      .catch(() => {});
    suppress(250);
    requestAnimationFrame(() => setNoAnim(false));
  };

  /** 拖动时每个格子的位移：被拖的跟手，被让位的平移一格 */
  const cellOffset = (i: number): string | null => {
    const m = metricsRef.current;
    if (!sort || !m) return null;
    if (i === sort.from) return `translate(${sort.dx}px, ${sort.dy}px)`;
    let target = i;
    if (sort.from < sort.to && i > sort.from && i <= sort.to) target = i - 1;
    else if (sort.from > sort.to && i >= sort.to && i < sort.from) target = i + 1;
    if (target === i) return null;
    const ox = (k: number) => (k % m.cols) * (m.cw + m.gapX);
    const oy = (k: number) => Math.floor(k / m.cols) * (m.ch + m.gapY);
    return `translate(${ox(target) - ox(i)}px, ${oy(target) - oy(i)}px)`;
  };

  // —— 框选：在网格空白处按下并拖动，画矩形选中一批图标 ——
  // 格子上按下的事件会冒泡到这里，用 closest(".cell") 排除掉。
  const onGridPointerDown = (e: React.PointerEvent) => {
    if (e.button !== 0) return;
    if ((e.target as HTMLElement).closest(".cell")) return;
    // Ctrl 状态同样兜底采样（框选时按着 Ctrl = 在原有选择上追加）
    const g = ++gestureRef.current;
    const ctrlEvt = ctrlOn(e);
    if (!ctrlEvt) {
      api
        .ctrlDown()
        .then((v) => {
          if (gestureRef.current === g && v) ctrlHeldRef.current = true;
        })
        .catch(() => {});
    }
    bandRef.current = {
      x0: e.clientX,
      y0: e.clientY,
      x1: e.clientX,
      y1: e.clientY,
      on: false,
      ctrl: ctrlEvt,
    };
    (e.currentTarget as HTMLElement).setPointerCapture?.(e.pointerId);
  };

  const onGridPointerMove = (e: React.PointerEvent) => {
    const b = bandRef.current;
    if (!b) return;
    b.x1 = e.clientX;
    b.y1 = e.clientY;
    // 先观望 6px：单击空白只是「取消选择」，别画出一个点的矩形
    if (!b.on && Math.hypot(b.x1 - b.x0, b.y1 - b.y0) > 6) b.on = true;
    if (b.on) setBand({ x0: b.x0, y0: b.y0, x1: b.x1, y1: b.y1 });
  };

  const onGridPointerUp = (e: React.PointerEvent) => {
    const b = bandRef.current;
    bandRef.current = null;
    setBand(null);
    if (!b) return;
    if (!b.on) {
      // 单击空白处：取消选择
      setSel([]);
      return;
    }
    const l = Math.min(b.x0, b.x1);
    const t = Math.min(b.y0, b.y1);
    const r = Math.max(b.x0, b.x1);
    const bo = Math.max(b.y0, b.y1);
    const hits: string[] = [];
    gridRef.current?.querySelectorAll<HTMLElement>(".cell").forEach((el) => {
      const c = el.getBoundingClientRect();
      if (c.right > l && c.left < r && c.bottom > t && c.top < bo) {
        const f = el.getAttribute("data-file");
        if (f) hits.push(f);
      }
    });
    // Ctrl + 框选 = 在原有选择上追加（以**按下时**的 Ctrl 为准）
    const withCtrl = b.ctrl || ctrlOn(e);
    setSel(withCtrl ? Array.from(new Set([...selRef.current, ...hits])) : hits);
  };

  const commitRename = async () => {
    if (!boxId || renaming === null) return;
    await api.renameBox(boxId, renaming).catch(() => {});
    setRenaming(null);
    refresh(boxId);
  };

  /** 「解散收纳盒」：整个盒子的图标全部移回桌面（盒子留着，变成空盒） */
  const restoreAll = async () => {
    setMenu(null);
    try {
      const done = await api.restoreAll(boxId);
      showToast(done > 0 ? `已放回 ${done} 个图标到桌面` : "盒子里没有图标");
    } catch (e) {
      showToast(String(e));
    }
    refresh(boxId);
  };

  /** 「按名称排序」：把当前顺序按文件名重排并落盘（和手动拖动排出来的顺序同一条路） */
  const sortByName = async () => {
    setMenu(null);
    const files = items
      .slice()
      .sort((a, b) => NAME_CMP.compare(a.name, b.name))
      .map((x) => x.file);
    try {
      await api.reorder(boxId, files);
      showToast("已按名称排序");
    } catch (e) {
      showToast(String(e));
    }
    refresh(boxId);
  };

  return (
    <div
      className={"panel" + (over ? " drop" : "")}
      onContextMenu={(e) => {
        e.preventDefault();
        setMenu({ x: e.clientX, y: e.clientY, kind: "header" });
      }}
    >
      {/* 毛玻璃：后面那块屏幕的模糊快照，垫在底板下面；色层单独一层压在上面
          （背景色必须是半透明的，玻璃才透得出来） */}
      {theme.acrylic && backdrop && (
        <>
          <div className="panel-glass" style={{ backgroundImage: `url("${backdrop}")` }} />
          <div className="panel-tint" />
        </>
      )}

      <div className="panel-head">
        {renaming !== null ? (
          <input
            ref={renameRef}
            className="rename-input"
            value={renaming}
            onChange={(e) => setRenaming(e.target.value)}
            onBlur={commitRename}
            onKeyDown={(e) => {
              if (e.key === "Enter") commitRename();
              if (e.key === "Escape") setRenaming(null);
            }}
          />
        ) : (
          <div
            className="panel-title"
            onClick={() => setRenaming(view?.name ?? "")}
            title="点击重命名"
          >
            {view?.name ?? "LightBox"}
          </div>
        )}
        <div className="panel-count">{items.length}</div>
        <div className="spacer" />
        {/* 有更新才出现：蓝底白字，样式直接借用更新提示页的主按钮 .upd-btn.primary */}
        {upd?.has_update && (
          <button
            className="upd-btn primary panel-updbtn"
            title={`发现新版本 v${upd.latest}\n点击查看更新说明`}
            onClick={() => api.openUpdateWindow().catch(() => {})}
          >
            有更新
          </button>
        )}
        {/* 「＋」已按用户要求移除：它紧挨着标题，容易被当成「往这个盒子里加图标」。
            新建收纳盒的入口在托盘菜单和盒子右键菜单里，功能没少。 */}
        <button
          className="icon-btn"
          title="更多"
          onClick={(e) => {
            const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
            setMenu({ x: r.right - MENU_W, y: r.bottom + 6, kind: "header" });
          }}
        >
          ⋯
        </button>
        <button className="icon-btn" title="关闭" onClick={close}>
          ✕
        </button>
      </div>

      {items.length === 0 ? (
        <div className="empty">
          把桌面上的快捷方式
          <br />
          拖到这个盒子里
        </div>
      ) : (
        <div
          className={"grid" + (noAnim ? " no-anim" : "")}
          ref={gridRef}
          onPointerDown={onGridPointerDown}
          onPointerMove={onGridPointerMove}
          onPointerUp={onGridPointerUp}
          onPointerCancel={onGridPointerUp}
        >
          {/* 框选矩形：坐标相对网格内容（含滚动偏移） */}
          {band && gridRef.current && (() => {
            const g = gridRef.current.getBoundingClientRect();
            return (
              <div
                className="band"
                style={{
                  left: Math.min(band.x0, band.x1) - g.left + gridRef.current.scrollLeft,
                  top: Math.min(band.y0, band.y1) - g.top + gridRef.current.scrollTop,
                  width: Math.abs(band.x1 - band.x0),
                  height: Math.abs(band.y1 - band.y0),
                }}
              />
            );
          })()}
          {items.map((it, i) => (
            <div
              className={
                "cell" +
                (sort && sort.from === i ? " dragging" : "") +
                (sel.includes(it.file) ? " sel" : "")
              }
              key={it.file}
              data-file={it.file}
              style={cellOffset(i) ? { transform: cellOffset(i)! } : undefined}
              title={`${it.name}　（Ctrl+点击多选 / 空白处拖动框选 / 拖动调序 / 拖出放回桌面）`}
              onPointerDown={(e) => onCellPointerDown(e, it, i)}
              onPointerMove={(e) => onCellPointerMove(e, it)}
              onPointerUp={onCellPointerUp}
              onClick={async (e) => {
                // 这次手势在 pointerdown 里已经按「多选」处理过了（切过选，别再切回去）
                if (cellHandled.current) {
                  cellHandled.current = false;
                  return;
                }
                if (isSuppressed()) return;
                // Ctrl 判定：事件 → 镜像 → 最后**等一次**后端的物理键状态。
                // 这里可以等：即便等出 20ms 也只是「选中晚一拍」，不像启动程序那样不可逆。
                // 顺手把手势序号推进一格，让 pointerdown 那次还没回来的查询作废。
                gestureRef.current++;
                let ctrl = ctrlOn(e);
                if (!ctrl) {
                  try {
                    ctrl = await api.ctrlDown();
                  } catch {
                    ctrl = false;
                  }
                  if (ctrl) ctrlHeldRef.current = true;
                }
                if (ctrl) {
                  toggleSel(it.file);
                  return;
                }
                // 普通点击 = 启动 + 取消选择（Ctrl 点击在上面已拦截）
                if (selRef.current.length > 0) setSel([]);
                // 启动成功即收起面板。点击图标是明确的操作意图，此时与其依赖
                // 失焦事件去猜（被启动的程序夺取前台的过程里 WM_ACTIVATE 可能
                // 丢失，表现为「程序起来后怎么点桌面都收不回」），不如主动收起：
                // 行为确定，也符合启动器类工具的常规交互。
                // 启动失败（文件不存在、快捷方式失效）不关——面板留着显示错误。
                api
                  .open(it.path)
                  .then(() => close())
                  .catch((e) => showToast(String(e)));
              }}
              onContextMenu={(e) => {
                e.preventDefault();
                e.stopPropagation();
                // 和资源管理器一致：右键点到未选中的图标上 = 只选中它；
                // 点到已选中的图标上 = 保持整个选中集，菜单对整批生效
                const files = selRef.current.includes(it.file)
                  ? selRef.current
                  : [it.file];
                setSel(files);
                setMenu({ x: e.clientX, y: e.clientY, kind: "items", files });
              }}
            >
              {it.icon ? <img src={it.icon} alt="" draggable={false} /> : <div className="cell-dot" />}
              <span>{it.name}</span>
            </div>
          ))}
        </div>
      )}

      {toast && <div className="toast">{toast}</div>}

      {menu && (
        <>
          <div className="menu-mask" onClick={() => setMenu(null)} onContextMenu={(e) => e.preventDefault()} />
          <div
            className="ctx-menu"
            style={{
              width: MENU_W,
              left: Math.min(menu.x, 432 - MENU_W - 8),
              // 高度要按实际项数估：单选比多选多「打开文件所在位置 / 属性」两项
              // 和两条分隔线，沿用旧的 130 会把最下面几项裁在窗口外。
              top: Math.min(
                menu.y,
                480 -
                  (menu.kind === "header"
                    ? 110
                    : menu.files.length === 1
                      ? 190
                      : 110),
              ),
            }}
          >
            {menu.kind === "header" ? (
              /* 面板里只留和「这一盒的内容」有关的两个动作：
                 盒子本身的增删改都在桌面盒子的右键菜单里（三点的菜单也指向这里）。
                 顺序按用户要求：常用的「按名称排序」在上，会清空盒子的「解散收纳盒」
                 放最下面，免得手滑点到。 */
              <>
                <div className="ctx-item" onClick={sortByName}>
                  按名称排序
                </div>
                <div
                  className={"ctx-item" + (items.length === 0 ? " disabled" : "")}
                  onClick={() => {
                    if (items.length === 0) return;
                    restoreAll();
                  }}
                >
                  解散收纳盒
                </div>
              </>
            ) : (
              /* 图标上的菜单：对选中集生效（多选时标题带个数） */
              <>
                <div
                  className="ctx-item"
                  onClick={async () => {
                    setMenu(null);
                    const paths = menu.files
                      .map((f) => items.find((x) => x.file === f))
                      .filter((x): x is Item => !!x)
                      .map((x) => x.path);
                    if (paths.length === 0) return;
                    // 和单击启动一样：发起成功就收起面板（全部 settle 后统一收，
                    // 失败的写 toast——面板即将关闭，用户能看到的是最后一个提示）
                    const rs = await Promise.allSettled(paths.map((p) => api.open(p)));
                    const ok = rs.filter((r) => r.status === "fulfilled").length;
                    if (ok > 0) close();
                    else showToast("启动失败");
                  }}
                >
                  {menu.files.length > 1 ? `打开全部（${menu.files.length}）` : "打开"}
                </div>
                {/* 系统的「打开文件所在位置」：.lnk 会跳到它指向的程序所在目录。
                    只对单个图标有意义（多选时没有「哪一个」的概念）。 */}
                {menu.files.length === 1 && (
                  <div
                    className="ctx-item"
                    onClick={async () => {
                      const it = items.find((x) => x.file === menu.files[0]);
                      setMenu(null);
                      if (!it) return;
                      try {
                        await api.reveal(it.path);
                      } catch (e) {
                        showToast(String(e));
                      }
                    }}
                  >
                    打开文件所在位置
                  </div>
                )}
                <div className="ctx-sep" />
                <div
                  className="ctx-item"
                  onClick={async () => {
                    const files = menu.files;
                    setMenu(null);
                    try {
                      const done = await api.restoreMany(boxId, files);
                      showToast(done > 0 ? `已放回 ${done} 个图标到桌面` : "放回失败");
                    } catch (e) {
                      showToast(String(e));
                    }
                    refresh(boxId);
                  }}
                >
                  {menu.files.length > 1 ? `放回桌面（${menu.files.length}）` : "放回桌面"}
                </div>
                <div
                  className="ctx-item danger"
                  onClick={async () => {
                    const files = menu.files;
                    setMenu(null);
                    try {
                      const done = await api.removeMany(boxId, files);
                      showToast(done > 0 ? `已移除 ${done} 个图标` : "移除失败");
                    } catch (e) {
                      showToast(String(e));
                    }
                    refresh(boxId);
                  }}
                >
                  {menu.files.length > 1 ? `从盒子移除（${menu.files.length}）` : "从盒子移除"}
                </div>
                {/* 系统的「属性」：走 ShellExecuteEx 的 properties verb，
                    弹出来和资源管理器右键完全一致（快捷方式 / 兼容性 / 详细信息）。 */}
                {menu.files.length === 1 && (
                  <>
                    <div className="ctx-sep" />
                    <div
                      className="ctx-item"
                      onClick={async () => {
                        const it = items.find((x) => x.file === menu.files[0]);
                        setMenu(null);
                        if (!it) return;
                        try {
                          await api.properties(it.path);
                        } catch (e) {
                          showToast(String(e));
                        }
                      }}
                    >
                      属性
                    </div>
                  </>
                )}
              </>
            )}
          </div>
        </>
      )}
    </div>
  );
}
