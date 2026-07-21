import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import { filterByPrefix, sortEntries } from "@/lib/entries";
import { MOCK_TREE, treeKey, type ObjectEntry } from "@/lib/mock-data";
import {
  applyThemeMode,
  getThemeMode,
  onSystemThemeChange,
  resolvesDark,
  type ThemeMode,
} from "@/lib/theme";

export type ViewMode = "list" | "grid";

export interface Transfer {
  id: number;
  name: string;
  dir: "up" | "down";
  pct: number;
  size: string;
  speed: string;
  status: "active" | "done";
}

export interface TransferSettings {
  concurrency: number;
  partSizeMb: number;
  verify: boolean;
  overwrite: boolean;
}

interface AppStore {
  themeMode: ThemeMode;
  dark: boolean;
  setThemeMode: (mode: ThemeMode) => void;
  toggleTheme: () => void;

  view: ViewMode;
  setView: (view: ViewMode) => void;
  defaultView: ViewMode;
  setDefaultView: (view: ViewMode) => void;

  activeConn: string;
  activeBucket: string;
  path: string[];
  expanded: Record<string, boolean>;
  toggleConn: (id: string) => void;
  selectBucket: (connId: string, bucket: string) => void;
  openFolder: (name: string) => void;
  gotoCrumb: (index: number) => void;

  search: string;
  setSearch: (value: string) => void;
  entries: ObjectEntry[];
  rawEntries: ObjectEntry[];

  selected: string | null;
  selectEntry: (name: string | null) => void;

  transfers: Transfer[];
  transferOpen: boolean;
  toggleTransferPanel: () => void;
  removeTransfer: (id: number) => void;
  startMockUpload: () => void;

  showAdd: boolean;
  addStep: 1 | 2;
  addProvider: string | null;
  openAdd: () => void;
  closeAdd: () => void;
  chooseProvider: (id: string) => void;
  backToProviders: () => void;

  showSettings: boolean;
  openSettings: () => void;
  closeSettings: () => void;

  transferSettings: TransferSettings;
  setTransferSettings: (patch: Partial<TransferSettings>) => void;
}

const AppStoreContext = createContext<AppStore | null>(null);

const INITIAL_TRANSFERS: Transfer[] = [
  { id: 1, name: "hero-banner.png", dir: "up", pct: 63, size: "1.4 MB", speed: "2.1 MB/s", status: "active" },
  { id: 2, name: "backup-2026-07.tar.gz", dir: "down", pct: 100, size: "318 MB", speed: "", status: "done" },
];

export function AppStoreProvider({ children }: { children: ReactNode }) {
  const [themeMode, setThemeModeState] = useState<ThemeMode>(getThemeMode);
  const [dark, setDark] = useState(() => resolvesDark(getThemeMode()));

  const [view, setView] = useState<ViewMode>("list");
  const [defaultView, setDefaultView] = useState<ViewMode>("list");

  const [activeConn, setActiveConn] = useState("r2");
  const [activeBucket, setActiveBucket] = useState("assets");
  const [path, setPath] = useState<string[]>([]);
  const [expanded, setExpanded] = useState<Record<string, boolean>>({ r2: true });
  const [selected, setSelected] = useState<string | null>(null);
  const [search, setSearch] = useState("");

  const [transfers, setTransfers] = useState<Transfer[]>(INITIAL_TRANSFERS);
  const [transferOpen, setTransferOpen] = useState(false);
  const [nextTid, setNextTid] = useState(3);

  const [showAdd, setShowAdd] = useState(false);
  const [addStep, setAddStep] = useState<1 | 2>(1);
  const [addProvider, setAddProvider] = useState<string | null>(null);
  const [showSettings, setShowSettings] = useState(false);

  const [transferSettings, setTransferSettingsState] = useState<TransferSettings>({
    concurrency: 4,
    partSizeMb: 8,
    verify: true,
    overwrite: false,
  });

  const setThemeMode = useCallback((mode: ThemeMode) => {
    applyThemeMode(mode);
    setThemeModeState(mode);
    setDark(resolvesDark(mode));
  }, []);

  const toggleTheme = useCallback(() => {
    setThemeMode(resolvesDark(getThemeMode()) ? "light" : "dark");
  }, [setThemeMode]);

  useEffect(
    () =>
      onSystemThemeChange(() => {
        if (getThemeMode() === "system") setDark(resolvesDark("system"));
      }),
    [],
  );

  useEffect(() => {
    const hasActive = transfers.some((t) => t.status === "active");
    if (!hasActive) return;
    const iv = setInterval(() => {
      setTransfers((prev) =>
        prev.map((t) => {
          if (t.status !== "active") return t;
          const pct = t.pct + Math.round(5 + Math.random() * 8);
          return pct >= 100 ? { ...t, pct: 100, status: "done", speed: "" } : { ...t, pct };
        }),
      );
    }, 750);
    return () => clearInterval(iv);
  }, [transfers]);

  const rawEntries = useMemo(
    () => MOCK_TREE[treeKey(activeBucket, path)] ?? [],
    [activeBucket, path],
  );
  const entries = useMemo(
    () => filterByPrefix(sortEntries(rawEntries), search),
    [rawEntries, search],
  );

  const value: AppStore = {
    themeMode,
    dark,
    setThemeMode,
    toggleTheme,
    view,
    setView,
    defaultView,
    setDefaultView,
    activeConn,
    activeBucket,
    path,
    expanded,
    toggleConn: (id) => setExpanded((e) => ({ ...e, [id]: !e[id] })),
    selectBucket: (connId, bucket) => {
      setActiveConn(connId);
      setActiveBucket(bucket);
      setPath([]);
      setSelected(null);
      setSearch("");
      setExpanded((e) => ({ ...e, [connId]: true }));
    },
    openFolder: (name) => {
      setPath((p) => [...p, name]);
      setSelected(null);
      setSearch("");
    },
    gotoCrumb: (index) => {
      setPath((p) => (index < 0 ? [] : p.slice(0, index + 1)));
      setSelected(null);
      setSearch("");
    },
    search,
    setSearch,
    entries,
    rawEntries,
    selected,
    selectEntry: setSelected,
    transfers,
    transferOpen,
    toggleTransferPanel: () => setTransferOpen((o) => !o),
    removeTransfer: (id) => setTransfers((ts) => ts.filter((t) => t.id !== id)),
    startMockUpload: () => {
      setTransferOpen(true);
      setTransfers((ts) => [
        {
          id: nextTid,
          name: `new-upload-${nextTid}.dat`,
          dir: "up",
          pct: 2,
          size: `${Math.floor(4 + Math.random() * 40)} MB`,
          speed: "1.8 MB/s",
          status: "active",
        },
        ...ts,
      ]);
      setNextTid((n) => n + 1);
    },
    showAdd,
    addStep,
    addProvider,
    openAdd: () => {
      setShowAdd(true);
      setAddStep(1);
      setAddProvider(null);
    },
    closeAdd: () => setShowAdd(false),
    chooseProvider: (id) => {
      setAddProvider(id);
      setAddStep(2);
    },
    backToProviders: () => {
      setAddStep(1);
      setAddProvider(null);
    },
    showSettings,
    openSettings: () => setShowSettings(true),
    closeSettings: () => setShowSettings(false),
    transferSettings,
    setTransferSettings: (patch) => setTransferSettingsState((s) => ({ ...s, ...patch })),
  };

  return <AppStoreContext.Provider value={value}>{children}</AppStoreContext.Provider>;
}

export function useApp(): AppStore {
  const ctx = useContext(AppStoreContext);
  if (!ctx) throw new Error("useApp must be used within AppStoreProvider");
  return ctx;
}
