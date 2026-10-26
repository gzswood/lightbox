import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { api, UpdateState } from "./api";

/**
 * 「版本更新」窗口。
 *
 * 窗口是复用的（后端启动时建好并隐藏），所以数据一律靠 `update:data` 事件推：
 * 每次打开窗口、每次检查完，后端都会推一次；页面挂载时额外主动拉一次兜底。
 *
 * 四种状态：检查中 / 有新版本 / 已是最新 / 出错（含「还没配置更新地址」）。
 */
export default function UpdateView() {
  const [s, setS] = useState<UpdateState | null>(null);
  const [err, setErr] = useState("");
  const [checking, setChecking] = useState(false);

  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    listen<UpdateState>("update:data", (e) => {
      if (alive) setS(e.payload);
    })
      .then((f) => {
        if (alive) un = f;
        else f();
      })
      .catch(() => {});
    api
      .updateState()
      .then((v) => {
        if (alive) setS(v);
      })
      .catch(() => {});
    return () => {
      alive = false;
      un?.();
    };
  }, []);

  // 关窗只藏窗口，不退出（窗口本身是复用的）
  const close = () => api.hideUpdate().catch(() => {});

  // Esc 关闭 + 更新提示窗口不参与「点心关闭」那套逻辑
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") api.hideUpdate().catch(() => {});
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const retry = () => {
    setErr("");
    setChecking(true);
    api
      .checkUpdate()
      .then(setS)
      .catch((e) => setErr(String(e)))
      .finally(() => setChecking(false));
  };

  const busy = checking || !!s?.busy;
  const has = !!s?.has_update;
  const version = s?.latest ? `v${s.latest}` : "";
  const current = s?.current ? `v${s.current}` : "";

  return (
    <div className="dlg upd" onContextMenu={(e) => e.preventDefault()}>
      <div
        className="dlg-title"
        onPointerDown={(e) => {
          if (e.button !== 0) return;
          getCurrentWindow().startDragging().catch(() => {});
        }}
      >
        <span>{has ? "发现新版本" : "版本更新"}</span>
        <button
          className="dlg-x"
          title="关闭（Esc）"
          onPointerDown={(e) => e.stopPropagation()}
          onClick={close}
        >
          ✕
        </button>
      </div>

      {busy && !has ? (
        <div className="upd-state">正在检查更新…</div>
      ) : has ? (
        <>
          <div className="upd-vers">
            {current} <span className="upd-arrow">→</span>{" "}
            <em>{version}</em>
          </div>
          {s?.date && <div className="upd-date">更新日期 {s.date}</div>}
          <div className="upd-head">更新内容</div>
          {s && s.notes.length > 0 ? (
            <ul className="upd-notes">
              {s.notes.map((n, i) => (
                <li key={i}>{n}</li>
              ))}
            </ul>
          ) : (
            <div className="upd-state flat">这个版本没有填写更新说明。</div>
          )}
          <div className="upd-actions">
            <button className="upd-btn" onClick={close}>
              稍后
            </button>
            {/* 「立即下载」和「更新说明」现在是同一个落点（站点首页：上面既有下载入口
                也有完整说明），所以只留一个按钮，免得两个按钮点出同一个页面。
                程序不再记某个 .exe 的文件名 —— 换网盘 / 换镜像只改网页。 */}
            <button
              className="upd-btn primary"
              title="在浏览器中打开更新网页，从那里下载"
              onClick={() => api.openUpdateSite().catch((e) => setErr(String(e)))}
            >
              立即下载
            </button>
          </div>
          {err && <div className="dlg-msg">{err}</div>}
        </>
      ) : (
        <>
          <div className="upd-vers">
            <em>已是最新版本</em> {current}
          </div>
          {(err || s?.error) && <div className="dlg-msg">{err || s?.error}</div>}
          {!s?.configured && !err && !s?.error && (
            <div className="upd-state flat">
              更新服务地址还没配置，暂时无法检查新版本。
            </div>
          )}
          {s?.checked_at ? (
            <div className="upd-date">上次检查 {fmtTime(s.checked_at)}</div>
          ) : null}
          <div className="upd-actions">
            <button className="upd-btn" onClick={close}>
              关闭
            </button>
            <button className="upd-btn primary" onClick={retry} disabled={busy}>
              重新检查
            </button>
          </div>
        </>
      )}
    </div>
  );
}

function fmtTime(ms: number) {
  if (!ms) return "";
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getMonth() + 1}月${d.getDate()}日 ${p(d.getHours())}:${p(d.getMinutes())}`;
}
