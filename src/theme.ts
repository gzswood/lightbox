/**
 * 外观主题：背景色 + 自动对比的文字色 + 毛玻璃 + 透明度。
 *
 * 一条硬规则：**文字颜色永远由背景色算出来**（取深/浅里对比度更高的一方）。
 * 用户随便把背景调成深色还是浅色，标题、图标名、按钮文字都不会和背景撞色 ——
 * 这也是「自定义背景色」能放开给用户的前提。
 */
import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api } from "./api";

export type Theme = {
  box_bg: string;
  panel_bg: string;
  acrylic: boolean;
  /** 透明度百分比：0 = 不透明，50 = 最透。数字越大越透明。 */
  transparency: number;
  /** 弹出面板是否画圆角（只管面板；收纳盒、菜单不受影响） */
  radius: boolean;
  /** 嵌入任务栏时隐藏名称：打开后任务栏里的盒子只显示图标（正方形） */
  bar_hide_label: boolean;
};

export const DEFAULT_THEME: Theme = {
  // 默认底板色：收纳盒和弹出窗口统一 #5A606B（和后端 store.rs 的默认值保持一致）。
  // 文字色由背景亮度自动翻白。
  box_bg: "#5A606B",
  panel_bg: "#5A606B",
  // 默认关毛玻璃：实色底板下更干净，弹出也更快
  acrylic: false,
  transparency: 0,
  // 默认直角（对应后端 Config::panel_radius 默认 false）
  radius: false,
  // 默认显示名字（对应后端 Config::bar_hide_label 默认 false）：任务栏里是胶囊 + 两个字
  bar_hide_label: false,
};

type RGB = { r: number; g: number; b: number };

export function parseHex(v: string): RGB | null {
  if (!v) return null;
  const t = v.trim().replace(/^#/, "");
  if (t.length !== 6 || !/^[0-9a-fA-F]{6}$/.test(t)) return null;
  return {
    r: parseInt(t.slice(0, 2), 16),
    g: parseInt(t.slice(2, 4), 16),
    b: parseInt(t.slice(4, 6), 16),
  };
}

const hex2 = (n: number) => Math.max(0, Math.min(255, Math.round(n))).toString(16).padStart(2, "0");

export function toHex(c: RGB) {
  return `#${hex2(c.r)}${hex2(c.g)}${hex2(c.b)}`.toUpperCase();
}

/** 相对亮度（WCAG），0 = 纯黑，1 = 纯白 */
function relLum(c: RGB): number {
  const f = (v: number) => {
    const s = v / 255;
    return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b);
}

/** 对比度（WCAG），1:1 ~ 21:1 */
export function contrastRatio(a: RGB, b: RGB): number {
  const la = relLum(a);
  const lb = relLum(b);
  const hi = Math.max(la, lb);
  const lo = Math.min(la, lb);
  return (hi + 0.05) / (lo + 0.05);
}

const INK = { r: 0x1b, g: 0x1b, b: 0x1f };
const SNOW = { r: 0xff, g: 0xff, b: 0xff };

/** 这块背景上该用深色字还是浅色字：取对比度高的那一方 */
export function prefersLightText(bg: RGB): boolean {
  return contrastRatio(bg, SNOW) > contrastRatio(bg, INK);
}

const rgba = (c: RGB, a: number) =>
  `rgba(${Math.round(c.r)}, ${Math.round(c.g)}, ${Math.round(c.b)}, ${Math.round(a * 1000) / 1000})`;

/** 两色混合：t=0 取 a，t=1 取 b */
export function mix(a: RGB, b: RGB, t: number): RGB {
  return {
    r: a.r + (b.r - a.r) * t,
    g: a.g + (b.g - a.g) * t,
    b: a.b + (b.b - a.b) * t,
  };
}

/** 文字/描边/悬浮色一整套：跟随背景的明暗翻转 */
function palette(bg: RGB) {
  const light = prefersLightText(bg);
  return {
    text: light ? "#FFFFFF" : "#1B1B1F",
    dim: light ? "rgba(255, 255, 255, 0.7)" : "rgba(27, 27, 31, 0.56)",
    hair: light ? "rgba(255, 255, 255, 0.18)" : "rgba(0, 0, 0, 0.08)",
    hover: light ? "rgba(255, 255, 255, 0.14)" : "rgba(0, 0, 0, 0.055)",
    accent: light ? "#9EC3FF" : "#2F6FEB",
    accentSoft: light ? "rgba(158, 195, 255, 0.2)" : "rgba(47, 111, 235, 0.12)",
    scroll: light ? "rgba(255, 255, 255, 0.22)" : "rgba(0, 0, 0, 0.14)",
    scrollHover: light ? "rgba(255, 255, 255, 0.34)" : "rgba(0, 0, 0, 0.24)",
  };
}

/**
 * 面板背景的最终 alpha。
 *
 * 滑块是「透明度百分比」：0 → 不透明。但毛玻璃开着时不能真的 100% 不透明，
 * 否则玻璃被底板完全挡住、等于没开，所以留出空隙（0% 时 alpha 0.64 → 36% 的玻璃）。
 * 下限压在 0.5：再透下去文字浮在壁纸上就没法保证读得清了。
 */
export function panelAlpha(t: Theme) {
  const base = t.acrylic ? 0.64 - (t.transparency / 100) * 0.28 : 1 - t.transparency / 100;
  return Math.max(0.5, Math.min(1, base));
}

/** 把主题写进 CSS 变量（所有窗口都用同一套变量） */
export function applyTheme(t: Theme) {
  const box = parseHex(t.box_bg) ?? { r: 255, g: 255, b: 255 };
  const panel = parseHex(t.panel_bg) ?? { r: 251, g: 251, b: 254 };
  const bp = palette(box);
  const pp = palette(panel);
  const a = panelAlpha(t);
  const st = document.documentElement.style;

  // 收纳盒
  st.setProperty("--tile-bg", rgba(box, 1));
  // 悬浮色：浅底往深里压 5%，深底往亮里提 12%（浅底压太多会显脏）
  const light = prefersLightText(box);
  st.setProperty("--tile-hover-bg", rgba(mix(box, light ? SNOW : INK, light ? 0.12 : 0.05), 1));
  st.setProperty("--tile-text", bp.text);
  st.setProperty("--tile-dim", bp.dim);
  st.setProperty("--tile-border", bp.hair);
  st.setProperty("--tile-inset", bp.hover);
  st.setProperty("--tile-accent", bp.accent);
  st.setProperty("--tile-badge-bg", bp.accentSoft);
  // 盒子底板：box_bg 直接就是收纳盒的实色背景（设置页「收纳盒」取色格改它）。
  // 拖入高亮：把背景往 accent 方向推一点，深浅背景都看得出「亮了」
  st.setProperty(
    "--tile-drop-bg",
    rgba(mix(box, parseHex(bp.accent) ?? { r: 47, g: 111, b: 235 }, prefersLightText(box) ? 0.18 : 0.28), 1),
  );

  // 面板
  st.setProperty("--panel-bg", rgba(panel, a));
  st.setProperty("--panel-border", pp.hair);
  st.setProperty("--panel-text", pp.text);
  st.setProperty("--panel-dim", pp.dim);
  st.setProperty("--panel-accent", pp.accent);
  st.setProperty("--panel-hover", pp.hover);
  st.setProperty("--panel-hair", pp.hair);
  st.setProperty("--scroll-thumb", pp.scroll);
  st.setProperty("--scroll-thumb-hover", pp.scrollHover);
  // 卡片（设置弹层 / 右键菜单）：比底板更实一点。
  // 默认（毛玻璃 0% 透明）直接给足不透明，不然盖在玻璃上会透出底下的图标名，看着发脏。
  st.setProperty("--card-bg", rgba(panel, Math.min(1, a + 0.42)));
  st.setProperty("--card-border", pp.hair);

  // 面板圆角：窗口区域由后端按同一个值裁（SetWindowRgn），两边必须一致，
  // 否则圆角外面会露出区域没裁掉的玻璃矩形。
  st.setProperty("--panel-radius", t.radius ? "20px" : "0px");

  st.setProperty("--acrylic", t.acrylic ? "1" : "0");
}

/** 读一次主题并订阅变化；`setTheme` 会立刻应用到本窗口（不负责落盘） */
export function useTheme(): [Theme, (t: Theme) => void] {
  const [theme, setTheme] = useState<Theme>(DEFAULT_THEME);
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    listen<Theme>("theme:changed", (e) => {
      if (alive) setTheme(e.payload);
    })
      .then((f) => {
        if (alive) un = f;
        else f();
      })
      .catch(() => {});
    api
      .theme()
      .then((t) => {
        if (alive) setTheme(t);
      })
      .catch(() => {});
    return () => {
      alive = false;
      un?.();
    };
  }, []);
  useEffect(() => {
    applyTheme(theme);
  }, [theme]);
  return [theme, setTheme];
}
