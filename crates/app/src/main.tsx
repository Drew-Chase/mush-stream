import React from "react";
import ReactDOM from "react-dom/client";
import { BrowserRouter, Route, Routes, useNavigate } from "react-router-dom";
import { HeroUIProvider, ToastProvider } from "@heroui/react";
import $ from "jquery";

import "./css/index.css";
import App from "./App";
import Home from "./pages/Home";
import Host from "./pages/Host";
import Connect from "./pages/Connect";
import Logs from "./pages/Logs";
import Settings from "./pages/Settings";

const rootEl = document.getElementById("root");
if (!rootEl) throw new Error("#root not found");

ReactDOM.createRoot(rootEl).render(
  <React.StrictMode>
    <BrowserRouter>
      <Providers>
        <Routes>
          <Route element={<App />}>
            <Route path="/" element={<Home />} />
            <Route path="/host" element={<Host />} />
            <Route path="/connect" element={<Connect />} />
            <Route path="/logs" element={<Logs />} />
            <Route path="/settings" element={<Settings />} />
          </Route>
        </Routes>
      </Providers>
    </BrowserRouter>
  </React.StrictMode>,
);

function Providers({ children }: { children: React.ReactNode }) {
  const navigate = useNavigate();
  $(window).on("contextmenu", (e) => e.preventDefault());
  return (
    <HeroUIProvider navigate={navigate}>
      <ToastProvider
        placement="bottom-right"
        toastProps={{
          shouldShowTimeoutProgress: true,
          timeout: 3000,
          variant: "flat",
        }}
      />
      {children}
    </HeroUIProvider>
  );
}
