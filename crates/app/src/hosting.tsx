import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";

import {
  appVersion as fetchAppVersion,
  checkForUpdate,
  clientStatus as fetchClientStatus,
  configLoadClient,
  configLoadHost,
  hostAddresses as fetchHostAddresses,
  hostStatus as fetchHostStatus,
  installUpdateAndRelaunch,
  logsBuffer as fetchLogsBuffer,
  onAppLog,
  onClientState,
  onHostState,
  recentsList as fetchRecents,
  systemProbe,
  type ClientConfig,
  type ClientState,
  type ClientStateEvent,
  type HostConfig,
  type HostState,
  type HostStateEvent,
  type LogLine,
  type RecentEntry,
  type ShareAddresses,
  type SystemProbe,
  type Update,
} from "./api";
import { addToast } from "@heroui/react";

const LOG_RING_SIZE = 1024;

interface AppStateShape {
  system: SystemProbe | null;
  hostState: HostState;
  hostError: string | null;
  hostAddresses: ShareAddresses | null;
  hostConfig: HostConfig | null;
  clientState: ClientState;
  clientAddress: string | null;
  clientError: string | null;
  clientConfig: ClientConfig | null;
  recents: RecentEntry[];
  logs: LogLine[];
  appVersion: string | null;
  /** Set after a successful `checkForUpdate()` call when a newer version
   *  is available. `null` means "no update queued" — could be either
   *  "up to date" or "haven't checked yet"; use `updateChecked` to
   *  distinguish. */
  update: Update | null;
  updateChecked: boolean;
  updateInstalling: boolean;
}

interface AppStateApi extends AppStateShape {
  /** Convenience boolean derived from hostState. */
  hosting: boolean;
  refreshSystem: () => Promise<void>;
  refreshHostAddresses: () => Promise<void>;
  refreshRecents: () => Promise<void>;
  refreshHostConfig: () => Promise<void>;
  refreshClientConfig: () => Promise<void>;
  setRecents: (r: RecentEntry[]) => void;
  setHostConfig: (c: HostConfig) => void;
  setClientConfig: (c: ClientConfig) => void;
  /** Imperatively flip the local hosting flag — used by legacy callers
   * that don't have a backend session yet. Most code should rely on the
   * `host:state` event flow instead. */
  setHosting: (v: boolean) => void;
  /** Re-probe the configured update endpoint. */
  refreshUpdate: () => Promise<void>;
  /** Download + install the queued update, then relaunch. No-op when
   *  no update is queued. */
  installUpdate: () => Promise<void>;
}

const AppStateCtx = createContext<AppStateApi | null>(null);

export function HostingProvider({ children }: { children: ReactNode }) {
  const [system, setSystem] = useState<SystemProbe | null>(null);
  const [hostState, setHostState] = useState<HostState>("idle");
  const [hostError, setHostError] = useState<string | null>(null);
  const [hostAddrs, setHostAddrs] = useState<ShareAddresses | null>(null);
  const [hostCfg, setHostCfg] = useState<HostConfig | null>(null);
  const [clientState, setClientState] = useState<ClientState>("idle");
  const [clientAddress, setClientAddress] = useState<string | null>(null);
  const [clientError, setClientError] = useState<string | null>(null);
  const [clientCfg, setClientCfg] = useState<ClientConfig | null>(null);
  const [recents, setRecents] = useState<RecentEntry[]>([]);
  const [logs, setLogs] = useState<LogLine[]>([]);
  const logsRingRef = useRef<LogLine[]>([]);
  const [appVersion, setAppVersion] = useState<string | null>(null);
  const [update, setUpdate] = useState<Update | null>(null);
  const [updateChecked, setUpdateChecked] = useState(false);
  const [updateInstalling, setUpdateInstalling] = useState(false);

  const pushLog = useCallback((line: LogLine) => {
    const ring = logsRingRef.current;
    ring.push(line);
    if (ring.length > LOG_RING_SIZE) ring.splice(0, ring.length - LOG_RING_SIZE);
    setLogs([...ring]);
  }, []);

  const refreshSystem = useCallback(async () => {
    try {
      setSystem(await systemProbe());
    } catch (e) {
      console.error("system probe failed", e);
    }
  }, []);

  const refreshHostAddresses = useCallback(async () => {
    try {
      setHostAddrs(await fetchHostAddresses());
    } catch (e) {
      console.error("host addresses failed", e);
    }
  }, []);

  const refreshRecents = useCallback(async () => {
    try {
      setRecents(await fetchRecents());
    } catch (e) {
      console.error("recents fetch failed", e);
    }
  }, []);

  const refreshHostConfig = useCallback(async () => {
    try {
      setHostCfg(await configLoadHost());
    } catch (e) {
      console.error("host config load failed", e);
    }
  }, []);

  const refreshClientConfig = useCallback(async () => {
    try {
      setClientCfg(await configLoadClient());
    } catch (e) {
      console.error("client config load failed", e);
    }
  }, []);

  const refreshUpdate = useCallback(async () => {
    try {
      const next = await checkForUpdate();
      setUpdate(next);
      setUpdateChecked(true);
      if (next) {
        addToast({
          title: `Update available — v${next.version}`,
          description: "Open Settings → Updates to install.",
          color: "primary",
          timeout: 6000,
        });
      }
    } catch (e) {
      // Don't surface a toast on failure — most users hit this on
      // first install before any release exists at the endpoint, or
      // when offline. Logged for debugging.
      console.error("update check failed", e);
      setUpdateChecked(true);
    }
  }, []);

  const installUpdate = useCallback(async () => {
    if (!update) return;
    setUpdateInstalling(true);
    try {
      await installUpdateAndRelaunch(update);
      // installUpdateAndRelaunch calls relaunch() — process exits
      // before we get here, so the next line is unreachable in
      // practice. Defensive in case of failure between download
      // and relaunch.
      setUpdateInstalling(false);
    } catch (e) {
      console.error("update install failed", e);
      addToast({
        title: "Update failed",
        description: String(e),
        color: "danger",
        timeout: 8000,
      });
      setUpdateInstalling(false);
    }
  }, [update]);

  // One-shot mount: probe + load configs + subscribe to events.
  useEffect(() => {
    let alive = true;
    let unlistenHost: (() => void) | null = null;
    let unlistenClient: (() => void) | null = null;
    let unlistenLog: (() => void) | null = null;

    const initial = async () => {
      await Promise.allSettled([
        refreshSystem(),
        refreshHostAddresses(),
        refreshRecents(),
        refreshHostConfig(),
        refreshClientConfig(),
        fetchAppVersion()
          .then((v) => {
            if (alive) setAppVersion(v);
          })
          .catch((e) => console.error("appVersion failed", e)),
        refreshUpdate(),
      ]);
      if (!alive) return;

      // Pull the current backend session status in case the page was
      // reloaded while a session was running.
      try {
        const hs = await fetchHostStatus();
        if (alive) setHostState(hs);
      } catch {
        /* ignore */
      }
      try {
        const addr = await fetchClientStatus();
        if (alive && addr) {
          setClientAddress(addr);
          setClientState("connected");
        }
      } catch {
        /* ignore */
      }

      try {
        const buf = await fetchLogsBuffer();
        if (alive) {
          logsRingRef.current = buf.slice(-LOG_RING_SIZE);
          setLogs([...logsRingRef.current]);
        }
      } catch {
        /* ignore */
      }
    };
    void initial();

    const subscribe = async () => {
      unlistenHost = await onHostState((ev: HostStateEvent) => {
        if (!alive) return;
        setHostState(ev.state);
        setHostError(ev.error);
      });
      unlistenClient = await onClientState((ev: ClientStateEvent) => {
        if (!alive) return;
        setClientState(ev.state);
        setClientAddress(ev.address);
        setClientError(ev.error);
      });
      unlistenLog = await onAppLog(pushLog);
    };
    void subscribe();

    return () => {
      alive = false;
      unlistenHost?.();
      unlistenClient?.();
      unlistenLog?.();
    };
    // refresh* are stable via useCallback; pushLog likewise.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Whenever host state flips into broadcasting, refresh addresses
  // (the listen port could have changed via Settings).
  useEffect(() => {
    if (hostState === "broadcasting") void refreshHostAddresses();
  }, [hostState, refreshHostAddresses]);

  // Local override used by code paths that pre-date backend wiring.
  const setHosting = useCallback((v: boolean) => {
    setHostState(v ? "broadcasting" : "idle");
  }, []);

  const value: AppStateApi = useMemo(
    () => ({
      system,
      hostState,
      hostError,
      hostAddresses: hostAddrs,
      hostConfig: hostCfg,
      clientState,
      clientAddress,
      clientError,
      clientConfig: clientCfg,
      recents,
      logs,
      appVersion,
      update,
      updateChecked,
      updateInstalling,
      hosting: hostState === "broadcasting",
      refreshSystem,
      refreshHostAddresses,
      refreshRecents,
      refreshHostConfig,
      refreshClientConfig,
      setRecents,
      setHostConfig: setHostCfg,
      setClientConfig: setClientCfg,
      setHosting,
      refreshUpdate,
      installUpdate,
    }),
    [
      system,
      hostState,
      hostError,
      hostAddrs,
      hostCfg,
      clientState,
      clientAddress,
      clientError,
      clientCfg,
      recents,
      logs,
      appVersion,
      update,
      updateChecked,
      updateInstalling,
      refreshSystem,
      refreshHostAddresses,
      refreshRecents,
      refreshHostConfig,
      refreshClientConfig,
      setHosting,
      refreshUpdate,
      installUpdate,
    ],
  );

  return <AppStateCtx.Provider value={value}>{children}</AppStateCtx.Provider>;
}

export function useHosting(): AppStateApi {
  const v = useContext(AppStateCtx);
  if (!v) throw new Error("useHosting must be used inside HostingProvider");
  return v;
}
