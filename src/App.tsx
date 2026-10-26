import { useEffect } from "react";
import BoxView from "./BoxView";
import PanelView from "./PanelView";
import MenuView from "./MenuView";
import PickView from "./PickView";
import SettingsView from "./SettingsView";
import UpdateView from "./UpdateView";

export default function App() {
  const params = new URLSearchParams(window.location.search);
  const mode = params.get("mode") ?? "box";
  const id = params.get("id") ?? "";

  // 窗口已被系统裁成圆角，页面自己要不透明地铺满整块矩形，
  // 否则 WebView 宿主窗口的默认背景（白色 + 窗口标题）会从圆角内侧透出来。
  useEffect(() => {
    document.documentElement.dataset.mode = mode;
  }, [mode]);

  if (mode === "panel") return <PanelView />;
  if (mode === "menu") return <MenuView />;
  if (mode === "settings") return <SettingsView />;
  if (mode === "update") return <UpdateView />;
  if (mode === "pick") return <PickView />;
  if (!id) return <div style={{ padding: 12, fontSize: 12 }}>LightBox</div>;
  return <BoxView id={id} />;
}
