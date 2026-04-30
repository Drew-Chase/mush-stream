import { getCurrentWindow } from "@tauri-apps/api/window";
import { IcLogo, IcMin, IcMax, IcX } from "./Icons";

export default function Titlebar({ route }: { route: string }) {
  const appWindow = getCurrentWindow();
  return (
    <div className="titlebar" data-tauri-drag-region>
      <div className="tb__l" data-tauri-drag-region>
        <div className="tb__logo" data-tauri-drag-region>
          <IcLogo size={16} />
          Mush Stream
        </div>
        <div className="tb__sep" />
        <div className="tb__crumb" data-tauri-drag-region>
          {route}
        </div>
      </div>
      <div className="tb__r">
        <button
          className="tb__btn"
          aria-label="Minimize"
          onClick={() => appWindow.minimize()}
        >
          <IcMin size={14} />
        </button>
        <button
          className="tb__btn"
          aria-label="Maximize"
          onClick={() => appWindow.toggleMaximize()}
        >
          <IcMax size={12} />
        </button>
        <button
          className="tb__btn tb__btn--close"
          aria-label="Close"
          onClick={() => appWindow.close()}
        >
          <IcX size={14} />
        </button>
      </div>
    </div>
  );
}
