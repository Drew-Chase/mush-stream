import { Outlet, useLocation } from "react-router-dom";
import Sidebar from "./components/Sidebar";
import Titlebar from "./components/Titlebar";
import { HostingProvider, useHosting } from "./hosting";

function routeLabel(pathname: string, hosting: boolean): string {
  if (pathname === "/") return "Home";
  if (pathname.startsWith("/host")) return hosting ? "Host · streaming" : "Host";
  if (pathname.startsWith("/connect")) return "Connect";
  if (pathname.startsWith("/logs")) return "Logs";
  if (pathname.startsWith("/settings")) return "Settings";
  return "";
}

function Shell() {
  const { pathname } = useLocation();
  const { hosting } = useHosting();
  return (
    <div className="app">
      <Titlebar route={routeLabel(pathname, hosting)} />
      <div className="body">
        <Sidebar />
        <div className="main">
          <Outlet />
        </div>
      </div>
    </div>
  );
}

export default function App() {
  return (
    <HostingProvider>
      <Shell />
    </HostingProvider>
  );
}
