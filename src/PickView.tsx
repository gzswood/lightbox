import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, PickSample, PickStart } from "./api";

/**
 * 屏幕取色器（吸管）的全屏窗口。
 *
 * 后端已经把整个虚拟屏幕截成快照（`begin_pick`），这里把快照铺满窗口当背景 ——
 * 取色过程中画面是静止的，和系统取色器一个行为。鼠标移动时向后端要「光标周围 15×15
 * 的原图 + 中心点颜色」，在光标旁边画一个放大镜；左键点击由后端取当前屏幕上那个点的
 * **真实**颜色（不是快照里的，防止这期间画面变了），通过 `pick:done` 发给设置窗口。
 *
 * 画面上**不压暗**：GetPixel 取的是合成后的屏幕像素，压一层半透明黑会把取到的颜色
 * 全部加深（放大镜里看到的也会偏暗）。模式的提示交给顶部小纸条和放大镜本身。
 *
 * 坐标：事件里的 clientX/Y 是**本窗口的 CSS 坐标**，只用来摆放大镜、判断贴边翻转；
 * 取样和落色由后端按 `GetCursorPos` 的真实物理光标位置算 —— 窗口横跨所有显示器，
 * 混合缩放下用 CSS 坐标换算会在另一块屏上偏掉（取到的颜色和指针位置对不上）。
 */
export default function PickView() {
  const [start, setStart] = useState<PickStart | null>(null);
  const [sample, setSample] = useState<PickSample | null>(null);
  const [cur, setCur] = useState({ x: 0, y: 0 });
  const [win, setWin] = useState({ w: 0, h: 0 });
  const [done, setDone] = useState(false);

  const canvasRef = useRef<HTMLCanvasElement>(null);
  const rafRef = useRef(0);
  const lastRef = useRef({ x: 0, y: 0 });

  // 窗口 CSS 尺寸：放大镜要靠边翻转，得知道窗口有多大
  useEffect(() => {
    const measure = () => setWin({ w: window.innerWidth, h: window.innerHeight });
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, []);

  // 取色窗口是**复用**的（只在第一次创建），页面也就只挂载一次。
  // 所以快照不能靠「挂载时请求」，否则第二次点吸管就永远拿不到新快照、窗口也
  // 不会显示——「小画笔只能用一次」就是这么来的。现在两条路都走：
  //   1. 挂载时向后端要当前会话（第一次进入时事件已经发过了，收不到）；
  //   2. 之后每次点吸管由后端广播 `pick:start`。
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    api
      .pickStartNow()
      .then((s) => {
        if (alive && s) setStart(s);
      })
      .catch(() => {});
    listen<PickStart>("pick:start", (e) => {
      if (alive) setStart(e.payload);
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

  // 每次新会话：等快照解码完再让后端显示窗口（不会闪一下玻璃，也不会把自己截进快照）。
  //
  // 注意**不能**再靠 `<img onLoad>`：同一屏连续两次截屏的 data URL 可能完全一样，
  // src 没变浏览器就不会再触发 load，窗口就再也显示不出来。这里每次都用
  // 一个新 Image 预加载，天然带「新会话」语义。
  useEffect(() => {
    if (!start) return;
    let cancelled = false;
    // 上一次取色的残留状态必须清掉：done 不复位的话，第二次进来所有左键点击
    // 都会被 onDown 里的 `if (done) return` 吃掉（这也是「只能取一次」的元凶之一）。
    setDone(false);
    setSample(null);
    const img = new Image();
    const ready = () => {
      if (!cancelled) api.pickReady().catch(() => {});
    };
    img.onload = ready;
    img.onerror = ready; // 解码失败也要显示，否则用户对着黑屏
    img.src = start.shot;
    return () => {
      cancelled = true;
    };
  }, [start]);

  // 取样回来 → 画放大镜
  useEffect(() => {
    if (!sample) return;
    const cv = canvasRef.current;
    if (!cv) return;
    const ctx = cv.getContext("2d");
    if (!ctx) return;
    const img = new Image();
    img.onload = () => {
      ctx.imageSmoothingEnabled = false; // 放大镜要能看清像素格
      ctx.clearRect(0, 0, cv.width, cv.height);
      ctx.drawImage(img, 0, 0, cv.width, cv.height);
    };
    img.src = sample.shot;
  }, [sample]);

  const onMove = (e: React.PointerEvent) => {
    setCur({ x: e.clientX, y: e.clientY });
    lastRef.current = { x: e.clientX, y: e.clientY };
    if (rafRef.current) return; // 已经有一次在路上了：rAF 之前的位置没有意义
    rafRef.current = requestAnimationFrame(() => {
      rafRef.current = 0;
      const p = lastRef.current;
      api.pickSample(p.x, p.y).then(setSample).catch(() => {});
    });
  };

  // 左键 = 取这个点的颜色（后端取真实屏幕像素后发 pick:done，然后藏起本窗口）
  const onDown = (e: React.PointerEvent) => {
    if (e.button !== 0 || done) return;
    setDone(true);
    api.pickCommit(e.clientX, e.clientY).catch(() => {});
  };

  // 右键 / Esc = 取消
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") api.pickCancel().catch(() => {});
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // 放大镜的位置：默认在光标右下，靠右/下边时翻到另一侧
  const M = 135;
  const flipX = cur.x + 20 + M > win.w;
  const flipY = cur.y + 20 + M + 26 > win.h;
  const mx = flipX ? cur.x - 20 - M : cur.x + 20;
  const my = flipY ? cur.y - 20 - M - 26 : cur.y + 20;

  return (
    <div
      className="pick"
      onPointerMove={onMove}
      onPointerDown={onDown}
      onContextMenu={(e) => {
        e.preventDefault();
        api.pickCancel().catch(() => {});
      }}
    >
      {start && (
        <img className="pick-bg" src={start.shot} alt="" draggable={false} />
      )}
      <div className="pick-hint">点击取色 · 右键或 Esc 取消</div>

      {sample && (
        <>
          <div className="pick-mag" style={{ left: mx, top: my }}>
            <canvas ref={canvasRef} width={135} height={135} />
            {/* 中心十字线 */}
            <span className="pick-cross-h" />
            <span className="pick-cross-v" />
          </div>
          <div
            className="pick-chip"
            style={{
              left: flipX ? mx + M - 92 : mx,
              top: flipY ? my - 24 : my + M + 6,
            }}
          >
            <i style={{ background: sample.color }} />
            {sample.color}
          </div>
        </>
      )}
    </div>
  );
}
