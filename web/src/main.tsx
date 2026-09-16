import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./app/App";
import "./styles/tokens.css";
import "./styles/base.css";
import { applyTheme, useTheme } from "./theme/useTheme";

// 首帧之前先把 token 写进 :root，否则会闪一下没有变量的裸页面。
const { mode, theme, distro } = useTheme.getState();
applyTheme(mode, theme, distro);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
