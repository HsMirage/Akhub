import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./styles/tokens.css";
import "./styles/components.css";
import App from "./App";

const root = document.getElementById("root");
if (!root) throw new Error("找不到挂载点 #root");

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
