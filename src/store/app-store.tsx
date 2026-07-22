import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from "react";
import type { ConnectionDto, ObjectEntry } from "@/lib/api";
import { keyToPath } from "@/lib/entries";
import {
  applyThemeMode,
  getThemeMode,
  onSystemThemeChange,
  resolvesDark,
  type ThemeMode,
} from "@/lib/theme";

export type ViewMode = "list" | "grid";

/** How a row click modifies the selection: plain click replaces it,
 * cmd/ctrl-click toggles, shift-click extends a range from the anchor. */
export type SelectMode = "single" | "toggle" | "range";

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
  /** Navigates into a folder by its full key (e.g. `"sub/img/"`), not its
   * display name -- see the call site in `file-browser.tsx` for why. */
  openFolder: (key: string) => void;
  gotoCrumb: (index: number) => void;
  /** Called after a connection is successfully deleted -- clears the active
   * selection and expanded state for it if it was the one being browsed. */
  onConnectionDeleted: (id: string) => void;

  search: string;
  setSearch: (value: string) => void;

  /** Selected object keys (files only -- folders navigate, they don't
   * select). Order is insertion order for toggle, listing order for
   * range. */
  selectedKeys: string[];
  /** `orderedFileKeys` is the current listing's file keys in display
   * order -- required for `range` mode. */
  selectKey: (key: string, mode: SelectMode, orderedFileKeys: string[]) => void;
  clearSelection: () => void;

  /** Object-mutation dialog state (dialogs themselves mount in
   * `src/components/modals/object-dialogs.tsx`). */
  renameTarget: ObjectEntry | null;
  openRename: (entry: ObjectEntry) => void;
  closeRename: () => void;
  showNewFolder: boolean;
  openNewFolder: () => void;
  closeNewFolder: () => void;
  deleteTargets: string[] | null;
  openDeleteObjects: (keys: string[]) => void;
  closeDeleteObjects: () => void;

  transfers: Transfer[];
  transferOpen: boolean;
  toggleTransferPanel: () => void;
  removeTransfer: (id: number) => void;
  startMockUpload: () => void;

  showAdd: boolean;
  openAdd: () => void;
  closeAdd: () => void;

  editingConnection: ConnectionDto | null;
  openEditConnection: (conn: ConnectionDto) => void;
  closeEditConnection: () => void;

  deletingConnection: ConnectionDto | null;
  openDeleteConnection: (conn: ConnectionDto) => void;
  closeDeleteConnection: () => void;

  showSettings: boolean;
  openSettings: () => void;
  closeSettings: () => void;

  transferSettings: TransferSettings;
  setTransferSettings: (patch: Partial<TransferSettings>) => void;
}

const AppStoreContext = createContext<AppStore | null>(null);

// Mock transfers stay until M4 replaces them with real engine events
// (accepted residue, per the M2 final review ledger).
const INITIAL_TRANSFERS: Transfer[] = [
  { id: 1, name: "hero-banner.png", dir: "up", pct: 63, size: "1.4 MB", speed: "2.1 MB/s", status: "active" },
  { id: 2, name: "backup-2026-07.tar.gz", dir: "down", pct: 100, size: "318 MB", speed: "", status: "done" },
];

export function AppStoreProvider({ children }: { children: ReactNode }) {
  const [themeMode, setThemeModeState] = useState<ThemeMode>(getThemeMode);
  const [dark, setDark] = useState(() => resolvesDark(getThemeMode()));

  const [view, setView] = useState<ViewMode>("list");
  const [defaultView, setDefaultView] = useState<ViewMode>("list");

  const [activeConn, setActiveConn] = useState("");
  const [activeBucket, setActiveBucket] = useState("");
  const [path, setPath] = useState<string[]>([]);
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [search, setSearchState] = useState("");

  const [selectedKeys, setSelectedKeys] = useState<string[]>([]);
  const [anchorKey, setAnchorKey] = useState<string | null>(null);

  const [renameTarget, setRenameTarget] = useState<ObjectEntry | null>(null);
  const [showNewFolder, setShowNewFolder] = useState(false);
  const [deleteTargets, setDeleteTargets] = useState<string[] | null>(null);

  const [transfers, setTransfers] = useState<Transfer[]>(INITIAL_TRANSFERS);
  const [transferOpen, setTransferOpen] = useState(false);
  const [nextTid, setNextTid] = useState(3);

  const [showAdd, setShowAdd] = useState(false);
  const [editingConnection, setEditingConnection] = useState<ConnectionDto | null>(null);
  const [deletingConnection, setDeletingConnection] = useState<ConnectionDto | null>(null);
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

  const clearSelection = useCallback(() => {
    setSelectedKeys([]);
    setAnchorKey(null);
  }, []);

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
      clearSelection();
      setSearchState("");
      setExpanded((e) => ({ ...e, [connId]: true }));
    },
    openFolder: (key) => {
      // Derived from the entry's own key, not appended from its display
      // name onto the current `path`: the browsed listing prefix is
      // `pathToPrefix(path) + search` (design §6), so when the current
      // search term contains "/" the listed rows can live under a deeper
      // prefix than `pathToPrefix(path)` alone -- appending just the
      // display name there would navigate to a path that doesn't match
      // where the folder actually is. `keyToPath` reconstructs the correct
      // absolute path directly from the key regardless of how the listing
      // that produced this entry was reached.
      setPath(keyToPath(key));
      clearSelection();
      setSearchState("");
    },
    gotoCrumb: (index) => {
      setPath((p) => (index < 0 ? [] : p.slice(0, index + 1)));
      clearSelection();
      setSearchState("");
    },
    onConnectionDeleted: (id) => {
      if (activeConn === id) {
        setActiveConn("");
        setActiveBucket("");
        setPath([]);
        clearSelection();
      }
      setExpanded((e) => {
        if (!(id in e)) return e;
        const next = { ...e };
        delete next[id];
        return next;
      });
    },
    search,
    setSearch: (valueText) => {
      setSearchState(valueText);
      clearSelection();
    },
    selectedKeys,
    selectKey: (key, mode, orderedFileKeys) => {
      if (mode === "single") {
        setSelectedKeys([key]);
        setAnchorKey(key);
        return;
      }
      if (mode === "toggle") {
        setSelectedKeys((prev) =>
          prev.includes(key) ? prev.filter((k) => k !== key) : [...prev, key],
        );
        setAnchorKey(key);
        return;
      }
      // range
      const from = anchorKey ? orderedFileKeys.indexOf(anchorKey) : -1;
      const to = orderedFileKeys.indexOf(key);
      if (from === -1 || to === -1) {
        setSelectedKeys([key]);
        setAnchorKey(key);
        return;
      }
      const [lo, hi] = from < to ? [from, to] : [to, from];
      setSelectedKeys(orderedFileKeys.slice(lo, hi + 1));
    },
    clearSelection,
    renameTarget,
    openRename: (entry) => setRenameTarget(entry),
    closeRename: () => setRenameTarget(null),
    showNewFolder,
    openNewFolder: () => setShowNewFolder(true),
    closeNewFolder: () => setShowNewFolder(false),
    deleteTargets,
    openDeleteObjects: (keys) => setDeleteTargets(keys),
    closeDeleteObjects: () => setDeleteTargets(null),
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
    openAdd: () => setShowAdd(true),
    closeAdd: () => setShowAdd(false),
    editingConnection,
    openEditConnection: (conn) => setEditingConnection(conn),
    closeEditConnection: () => setEditingConnection(null),
    deletingConnection,
    openDeleteConnection: (conn) => setDeletingConnection(conn),
    closeDeleteConnection: () => setDeletingConnection(null),
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
