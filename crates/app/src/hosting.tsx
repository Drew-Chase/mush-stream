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
  clientStatus as fetchClientStatus,
  configLoadClient,
  configLoadHost,
  hostAddresses as fetchHostAddresses,
  hostStatus as fetchHostStatus,
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
} from "./api";

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
      refreshSystem,
      refreshHostAddresses,
      refreshRecents,
      refreshHostConfig,
      refreshClientConfig,
      setHosting,
    ],
  );

  return <AppStateCtx.Provider value={value}>{children}</AppStateCtx.Provider>;
}

export function useHosting(): AppStateApi {
  const v = useContext(AppStateCtx);
  if (!v) throw new Error("useHosting must be used inside HostingProvider");
  return v;
}
