import { useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { api } from "./api";
import type { UpdateState } from "./api";
import type { Theme } from "./theme";

type Props = {
  id?: string;
  name?: string;
  topmost?: boolean;
  /** 全局设置（托盘进的）：隐藏「名称 / 置于顶层」等单盒项 */
  global?: boolean;
  theme: Theme;
  /** 改外观：立刻应用到所有窗口，落盘由外层统一防抖 */
  onTheme: (patch: Partial<Theme>) => void;
  /** 名称/置顶变化后通知外层刷新 */
  onChanged: () => void;
  /** 右上角 ✕ / Esc：关掉设置窗口 */
  onClose: () => void;
  /** 内容实际高度（逻辑像素）：外层拿去把窗口改成都高，不用滚动 */
  onHeight: (h: number) => void;
};

/** 设置窗口的内容：名称、外观（透明度/背景色/毛玻璃）、置顶、开机自启、存储目录
 *
 * 每一项都是**改完立即生效**（名称在失焦/回车时提交），所以没有「完成 / 取消」按钮 ——
 * 关掉窗口走右上角的 ✕ 或 Esc（点桌面不关，见 SettingsView 的说明）。
 *
 * `global` 模式（托盘「设置…」）：只保留所有收纳盒的公共项，「名称」和「置于顶层」
 * 是单个盒子的设置，不在这里出现。
 */
export default function SettingsDialog({
  id = "",
  name = "",
  topmost = false,
  global = false,
  theme,
  onTheme,
  onChanged,
  onClose,
  onHeight,
}: Props) {
  // 窗口高度跟着内容走：量 .dlg 自己（它是 height: fill-content，不受窗口高度影响）
  const rootRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = rootRef.current;
    if (!el) return;
    // 用 scrollHeight 而不是 rect.height：内容比当前窗口高时，.dlg 会被
    // max-height:100% 夹住，rect.height 永远是窗口高度，窗口就长不高了。
    // +2 是上下各 1px 边框（scrollHeight 不含 border）。
    const report = () => onHeight(el.scrollHeight + 2);
    report();
    const ro = new ResizeObserver(report);
    ro.observe(el);
    return () => ro.disconnect();
  }, [onHeight]);

  const [n, setN] = useState(name);
  const [onTop, setOnTop] = useState(topmost);
  /** 当前展开调色板的是哪个色块（null = 都收着） */
  const [edit, setEdit] = useState<"box" | "panel" | null>(null);
  const [auto, setAuto] = useState<boolean | null>(null);
  const [version, setVersion] = useState("");
  const [exe, setExe] = useState("");
  const [dataDir, setDataDir] = useState("");
  const [msg, setMsg] = useState("");
  /**
   * 更新状态。后端**不再弹窗**：查到新版本只把状态推过来，
   * 这里在版本号旁边点一个红点 + 一行红字，用户想更新自己去点。
   */
  const [upd, setUpd] = useState<UpdateState | null>(null);

  // 挂载时拉一次；之后后端每次检查完（自动 / 手动）推 `update:data` 过来。
  // 设置窗口是复用的、页面只挂载一次，所以必须靠事件，不能只靠挂载。
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

  useEffect(() => {
    setN(name);
    setOnTop(topmost);
  }, [name, topmost]);

  useEffect(() => {
    api
      .settings()
      .then((s) => {
        setAuto(s.autostart);
        setVersion(s.version);
        setExe(s.exe);
        setDataDir(s.data_dir);
      })
      .catch(() => setAuto(false));
  }, []);

  // 吸管取色回来：填给「点吸管那一刻」正在调的色块。
  //
  // 不能用 edit 状态来判断目标：① onTheme 每次渲染都是新函数，这个 effect 会跟着
  // 反复重订阅，事件可能落到正要失效的旧回调上（旧回调里的 edit 还是上一个色块）；
  // ② 取色期间设置窗口被后端藏起来，事件何时投递到页面并不确定。
  // 改成「点吸管时把目标锁进 ref，事件只看 ref」——语义上本来就该这样：
  // 用户点的是哪个色块的吸管，颜色就填哪个色块。
  const pickTarget = useRef<"box" | "panel" | null>(null);
  // onTheme 也走 ref：effect 不再因为父组件重渲染而重订阅
  const themeRef = useRef(onTheme);
  useEffect(() => {
    themeRef.current = onTheme;
  }, [onTheme]);

  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    listen<string>("pick:done", (e) => {
      const t = pickTarget.current;
      pickTarget.current = null; // 一次取色只消费一次
      if (t === "box") themeRef.current({ box_bg: e.payload });
      else if (t === "panel") themeRef.current({ panel_bg: e.payload });
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

  const commitName = async () => {
    const v = n.trim() || "收纳盒";
    setN(v);
    if (v !== name) {
      await api.renameBox(id, v).catch((e) => setMsg(String(e)));
      onChanged();
    }
  };

  const toggleTop = async () => {
    const next = !onTop;
    setOnTop(next);
    try {
      await api.setTopmost(id, next);
      onChanged();
    } catch (e) {
      setOnTop(!next);
      setMsg(String(e));
    }
  };

  const toggleAuto = async () => {
    if (auto === null) return;
    const next = !auto;
    setAuto(next);
    try {
      await api.setAutostart(next);
      setMsg(next ? "已设为开机自启" : "已取消开机自启");
    } catch (e) {
      setAuto(!next);
      setMsg(String(e));
    }
  };

  return (
    <div className="dlg" ref={rootRef} onContextMenu={(e) => e.preventDefault()}>
      {/* 标题栏就是拖动区：设置是独立窗口，用户可以拖到顺手的位置 */}
      {/* 拖动用 JS 的 startDragging（和收纳盒同一套），不用 data-tauri-drag-region ——
          两者同时生效会互相抢，出现「拖一下就松手」 */}
      <div
        className="dlg-title"
        onPointerDown={(e) => {
          if (e.button !== 0) return;
          getCurrentWindow().startDragging().catch(() => {});
        }}
      >
        <span>设置</span>
        <span className="dlg-title-name">{global ? "所有收纳盒" : name}</span>
        <button
          className="dlg-x"
          title="关闭（Esc）"
          onPointerDown={(e) => e.stopPropagation()}
          onClick={onClose}
        >
          ✕
        </button>
      </div>

      {!global && (
        <div className="dlg-row">
          <div className="dlg-label">名称</div>
          <input
            className="dlg-input"
            value={n}
            onChange={(e) => setN(e.target.value)}
            onBlur={commitName}
            onKeyDown={(e) => {
              if (e.key === "Enter") (e.currentTarget as HTMLInputElement).blur();
              if (e.key === "Escape") {
                setN(name);
                (e.currentTarget as HTMLInputElement).blur();
              }
            }}
          />
        </div>
      )}

        {/* 透明度：整行独立，滑块占满宽度（右侧数字才不会顶出边距） */}
        <div className="dlg-row col">
          <div className="dlg-line">
            <span className="dlg-label">透明度</span>
            <span className="dlg-op-val">{Math.round(theme.transparency)}%</span>
          </div>
          <input
            className="dlg-slider"
            type="range"
            min={0}
            max={50}
            step={1}
            value={Math.round(theme.transparency)}
            onChange={(e) => onTheme({ transparency: Number(e.target.value) })}
          />
          <div className="dlg-desc">0% 不透明 / 50% 最透，数字越大越透明</div>
        </div>

        <div className="dlg-row col">
          <div className="dlg-line">
            <span className="dlg-label">背景颜色</span>
            <button
              className="dlg-link"
              onClick={() => onTheme({ box_bg: "#5A606B", panel_bg: "#5A606B" })}
            >
              恢复默认
            </button>
          </div>
          <div className="dlg-colors">
            <Swatch
              label="收纳盒"
              value={theme.box_bg}
              active={edit === "box"}
              onClick={() => setEdit(edit === "box" ? null : "box")}
            />
            <Swatch
              label="弹出窗口"
              value={theme.panel_bg}
              active={edit === "panel"}
              onClick={() => setEdit(edit === "panel" ? null : "panel")}
            />
          </div>
          {edit && (
            <ColorPop
              value={edit === "box" ? theme.box_bg : theme.panel_bg}
              onChange={(v) => onTheme(edit === "box" ? { box_bg: v } : { panel_bg: v })}
              onClose={() => setEdit(null)}
              // 点吸管这一刻锁定目标色块：pick:done 回来时只认这个，
              // 不受后续状态/重订阅影响（之前会填到上一次改的那个色块上）
              onPick={() => {
                pickTarget.current = edit;
                api.beginPick().catch(() => {
                  pickTarget.current = null;
                });
              }}
            />
          )}
          <div className="dlg-desc">
            文字颜色按背景自动取深/浅。「收纳盒」是桌面上那些小格子的底板色。
            调色板里的小画笔是吸管，可以从屏幕上任意位置取色
          </div>
        </div>

        <div className="dlg-row">
          <div className="dlg-label">
            毛玻璃
            <div className="dlg-desc">弹出窗口背后做模糊，像磨砂玻璃</div>
          </div>
          <div
            className={"switch" + (theme.acrylic ? " on" : "")}
            onClick={() => onTheme({ acrylic: !theme.acrylic })}
            role="switch"
            aria-checked={theme.acrylic}
          >
            <div className="knob" />
          </div>
        </div>

        <div className="dlg-row">
          <div className="dlg-label">
            圆角窗口
            <div className="dlg-desc">弹出窗口四角为圆角；关掉是直角方窗</div>
          </div>
          <div
            className={"switch" + (theme.radius ? " on" : "")}
            onClick={() => onTheme({ radius: !theme.radius })}
            role="switch"
            aria-checked={theme.radius}
          >
            <div className="knob" />
          </div>
        </div>

        {/* 只对「嵌入任务栏」的盒子有意义：默认显示两个汉字，打开后只剩图标（正方形按钮） */}
        <div className="dlg-row">
          <div className="dlg-label">
            嵌入任务栏时隐藏名称
            <div className="dlg-desc">打开后任务栏里只显示图标，不显示两个汉字</div>
          </div>
          <div
            className={"switch" + (theme.bar_hide_label ? " on" : "")}
            onClick={() => onTheme({ bar_hide_label: !theme.bar_hide_label })}
            role="switch"
            aria-checked={theme.bar_hide_label}
          >
            <div className="knob" />
          </div>
        </div>

        {!global && (
          <div className="dlg-row">
            <div className="dlg-label">
              置于顶层
              <div className="dlg-desc">盒子浮在普通窗口之上</div>
            </div>
            <div
              className={"switch" + (onTop ? " on" : "")}
              onClick={toggleTop}
              role="switch"
              aria-checked={onTop}
            >
              <div className="knob" />
            </div>
          </div>
        )}

        <div className="dlg-row">
          <div className="dlg-label">
            开机自启
            <div className="dlg-desc">登录 Windows 后自动运行 LightBox</div>
          </div>
          <div
            className={"switch" + (auto ? " on" : "") + (auto === null ? " off" : "")}
            onClick={toggleAuto}
            role="switch"
            aria-checked={!!auto}
          >
            <div className="knob" />
          </div>
        </div>

        <div className="dlg-row">
          <div className="dlg-label">
            存储位置
            <div className="dlg-desc one" title={dataDir}>
              {dataDir || "…"}
            </div>
          </div>
          <button className="dlg-btn" onClick={() => api.openDataDir().catch(() => {})}>
            打开
          </button>
        </div>

        {msg && <div className="dlg-msg">{msg}</div>}

        {/* 没有「完成」按钮：所有设置项改完即生效，点空白处或 Esc 关闭 */}
        <div className="dlg-foot">
          {/* 版本号本身就是「版本」入口：
              - 平时：点一下打开更新窗口并联网检查
              - 有新版本：红点 + 红字提醒（后端不再弹窗打扰用户），点开看说明和下载 */}
          <button
            className={"dlg-ver" + (upd?.has_update ? " has-new" : "")}
            title={
              upd?.has_update
                ? `发现新版本 v${upd.latest}（当前 v${version}）\n点击查看更新说明`
                : `${exe}\n点击检查更新`
            }
            onClick={() => {
              if (upd?.has_update) {
                // 状态已经有了，直接亮窗口，不用再联网等一次
                api.openUpdateWindow().catch(() => {});
                return;
              }
              api
                .openUpdateWindow()
                .then(() => api.checkUpdate())
                .catch(() => {});
            }}
          >
            {upd?.has_update ? (
              <>
                <span className="dlg-dot" />
                v{version || "0.9.4"} → v{upd.latest} 可更新
              </>
            ) : (
              <>LightBox v{version || "0.9.4"}</>
            )}
          </button>
          <span className="dlg-hint">作者:Gathon 微信:gzswood</span>
        </div>
    </div>
  );
}

/** 一个颜色格：点一下在下面展开自绘调色板（不再用 <input type="color"> 的系统对话框） */
function Swatch({
  label,
  value,
  active,
  onClick,
}: {
  label: string;
  value: string;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className={"sw" + (active ? " on" : "")}
      title={`${label}背景色`}
      onClick={onClick}
    >
      <span className="sw-dot" style={{ background: value }} />
      <span className="sw-txt">
        <em>{label}</em>
        {value.toUpperCase()}
      </span>
    </button>
  );
}

/** 常用色（10 列 × 2 行） */
const PRESETS = [
  "#FFFFFF", "#F5F6F8", "#E7E9ED", "#CFD3DA", "#9AA0AA",
  "#5A606B", "#2B2F36", "#1B1B1F", "#0E1116", "#000000",
  "#E8F0FE", "#BFD6FF", "#2F6FEB", "#0B3D91", "#DFF3E6",
  "#3FA96A", "#FFF4E0", "#F0A22E", "#FDE8E8", "#E05252",
];

const clamp01 = (v: number) => (v < 0 ? 0 : v > 1 ? 1 : v);

function hexToHsv(hex: string) {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return { h: 0, s: 0, v: 100 };
  const n = parseInt(m[1], 16);
  const r = ((n >> 16) & 255) / 255;
  const g = ((n >> 8) & 255) / 255;
  const b = (n & 255) / 255;
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  const d = max - min;
  let h = 0;
  if (d > 0) {
    if (max === r) h = ((g - b) / d) % 6;
    else if (max === g) h = (b - r) / d + 2;
    else h = (r - g) / d + 4;
    h *= 60;
    if (h < 0) h += 360;
  }
  return {
    h: Math.round(h),
    s: Math.round((max > 0 ? d / max : 0) * 100),
    v: Math.round(max * 100),
  };
}

function hsvToHex(h: number, s: number, v: number) {
  const S = s / 100;
  const V = v / 100;
  const c = V * S;
  const x = c * (1 - Math.abs(((h / 60) % 2) - 1));
  const m = V - c;
  let r = 0;
  let g = 0;
  let b = 0;
  if (h < 60) [r, g, b] = [c, x, 0];
  else if (h < 120) [r, g, b] = [x, c, 0];
  else if (h < 180) [r, g, b] = [0, c, x];
  else if (h < 240) [r, g, b] = [0, x, c];
  else if (h < 300) [r, g, b] = [x, 0, c];
  else [r, g, b] = [c, 0, x];
  const to = (n: number) =>
    Math.round((n + m) * 255)
      .toString(16)
      .padStart(2, "0");
  return `#${to(r)}${to(g)}${to(b)}`.toUpperCase();
}

/** 吸管图标（和系统取色器里那个一样的小画笔） */
function Pipette() {
  return (
    <svg viewBox="0 0 24 24" width="13" height="13" aria-hidden>
      <g
        fill="none"
        stroke="currentColor"
        strokeWidth="1.7"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        <path d="M14.6 3.6l5.8 5.8-2.4 2.4-5.8-5.8z" />
        <path d="M12.2 6L4.4 13.8v2.6h2.6l8-8" />
        <path d="M13.4 8.2l2.4 2.4" />
        <path d="M4 20.2l1.4 1.4" />
      </g>
    </svg>
  );
}

/**
 * 自绘调色板（替代 `<input type="color">` 的系统对话框）。
 *
 * 系统对话框是个普通顶层窗口，会被 always_on_top 的设置窗口整个压在下面 ——
 * 用户看到的就是「点了没反应」，换台机器还不一样。自己画就没有这些问题。
 *
 * 布局：常用色 → 饱和/明度方块 → 色相条 → hex + 吸管。吸管走后端开全屏取色器，
 * 取到的颜色由 `pick:done` 事件送回（监听在 SettingsDialog 里）。
 */
function ColorPop({
  value,
  onChange,
  onClose,
  onPick,
}: {
  value: string;
  onChange: (v: string) => void;
  onClose: () => void;
  /** 吸管：外层负责「锁定目标色块 + 发起取色」，这里只管触发 */
  onPick: () => void;
}) {
  const [hsv, setHsv] = useState(() => hexToHsv(value));
  const [draft, setDraft] = useState(value.toUpperCase());

  // 外部改了颜色（预设 / 恢复默认 / 吸管）→ 同步本地状态。
  // 只在外来的值和本地推上去的不一致时才覆盖，否则拖动滑块会和自己打架。
  useEffect(() => {
    const v = value.toUpperCase();
    setDraft(v);
    setHsv((prev) => (hsvToHex(prev.h, prev.s, prev.v) === v ? prev : hexToHsv(v)));
  }, [value]);

  const apply = (h: number, s: number, v: number) => {
    const nh = ((h % 360) + 360) % 360;
    setHsv({ h: nh, s, v });
    onChange(hsvToHex(nh, s, v));
  };

  // —— 饱和/明度方块：x = 饱和，y = 明度（上高下低）——
  const svRef = useRef<HTMLDivElement>(null);
  const svAt = (clientX: number, clientY: number) => {
    const el = svRef.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    apply(
      hsv.h,
      Math.round(clamp01((clientX - r.left) / r.width) * 100),
      Math.round((1 - clamp01((clientY - r.top) / r.height)) * 100),
    );
  };

  // —— 色相条：x = 色相 ——
  const hueRef = useRef<HTMLDivElement>(null);
  const hueAt = (clientX: number) => {
    const el = hueRef.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    apply(Math.round(clamp01((clientX - r.left) / r.width) * 359), hsv.s, hsv.v);
  };

  return (
    <div className="sw-pop">
      <div className="sw-presets">
        {PRESETS.map((c) => (
          <button
            key={c}
            type="button"
            className={"sw-preset" + (c === value.toUpperCase() ? " on" : "")}
            style={{ background: c }}
            title={c}
            onClick={() => onChange(c)}
          />
        ))}
      </div>

      <div
        ref={svRef}
        className="sw-sv"
        style={{
          background: `linear-gradient(to top, #000, rgba(0,0,0,0)), linear-gradient(to right, #fff, hsl(${hsv.h} 100% 50%))`,
        }}
        onPointerDown={(e) => {
          (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
          svAt(e.clientX, e.clientY);
        }}
        onPointerMove={(e) => {
          if (e.buttons === 1) svAt(e.clientX, e.clientY);
        }}
      >
        <span
          className="sw-sv-thumb"
          style={{ left: `${hsv.s}%`, top: `${100 - hsv.v}%` }}
        />
      </div>

      <div
        ref={hueRef}
        className="sw-hue"
        onPointerDown={(e) => {
          (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
          hueAt(e.clientX);
        }}
        onPointerMove={(e) => {
          if (e.buttons === 1) hueAt(e.clientX);
        }}
      >
        <span className="sw-hue-thumb" style={{ left: `${(hsv.h / 359) * 100}%` }} />
      </div>

      <div className="sw-hexrow">
        <span className="sw-dot" style={{ background: value }} />
        <input
          className="sw-hex"
          value={draft}
          spellCheck={false}
          onChange={(e) => {
            const t = e.target.value.toUpperCase();
            setDraft(t);
            if (/^#[0-9A-F]{6}$/.test(t)) onChange(t);
          }}
          onBlur={() => setDraft(value.toUpperCase())}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.currentTarget as HTMLInputElement).blur();
          }}
        />
        <button
          type="button"
          className="sw-pipette"
          title="从屏幕上取色（吸管）"
          onClick={onPick}
        >
          <Pipette />
        </button>
        <button type="button" className="dlg-link" onClick={onClose}>
          收起
        </button>
      </div>
    </div>
  );
}
