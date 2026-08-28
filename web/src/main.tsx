import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { Gallery } from "./gallery/Gallery";
import "./styles/tokens.css";
import "./styles/base.css";
import { applyTheme, useTheme } from "./theme/useTheme";

// 中性色的色相由发行版派生，所以首帧之前必须先把变量写进 :root，
// 否则会闪一下没有 token 的裸页面。
const { mode, distro } = useTheme.getState();
applyTheme(mode, distro);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <Gallery />
  </StrictMode>,
);
