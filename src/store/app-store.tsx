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

/** Which category the settings modal is showing. Mirrors the `CategoryId`
 * union `src/components/modals/settings/index.tsx` used to own locally --
 * lifted up here so the tray's "Settings…"/"Check for Updates…" items can
 * land the modal on a specific pane from outside the modal's own tree (see
 * `TrayEventBridge`). */
export type SettingsPane = "general" | "transfers" | "advanced" | "update" | "about";

/** How a row click modifies the selection: plain click replaces it,
 * cmd/ctrl-click toggles, shift-click extends a range from the anchor. */
export type SelectMode = "single" | "toggle" | "range";

/** Transfer engine tuning knobs shown in the settings modal. NOT YET WIRED
 * to the backend: `enqueue_uploads` and the transfer engine (see
 * `src/lib/api.ts`) don't currently take a concurrency/verify/overwrite
 * input, so changing these has no effect on real transfers yet. Kept here
 * only so the settings modal has somewhere to read/write them without
 * inventing a second half-wired store; a later milestone is expected to
 * thread them through `enqueueUploads` and friends.
 *
 * `partSizeMb` used to live here too, backing a "分片大小" (8/16/64 MB)
 * segmented control in the Transfers pane. It was a placebo -- nothing ever
 * read it -- superseded by the real, backend-wired upload/download
 * threshold + part-floor selects (spec §4.7), so it and its control were
 * removed rather than left to mislead users into thinking it did something. */
export interface TransferSettings {
  concurrency: number;
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
  settingsPane: SettingsPane;
  /** Opens the settings modal, landing it on `pane` (default "general").
   * Sets the pane on *every* call, including while the modal is already
   * open -- that's what lets the tray's "Check for Updates…" item retarget
   * an already-open modal onto the update pane. */
  openSettings: (pane?: SettingsPane) => void;
  setSettingsPane: (pane: SettingsPane) => void;
  closeSettings: () => void;

  transferSettings: TransferSettings;
  setTransferSettings: (patch: Partial<TransferSettings>) => void;
}

const AppStoreContext = createContext<AppStore | null>(null);

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

  const [showAdd, setShowAdd] = useState(false);
  const [editingConnection, setEditingConnection] = useState<ConnectionDto | null>(null);
  const [deletingConnection, setDeletingConnection] = useState<ConnectionDto | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [settingsPane, setSettingsPaneState] = useState<SettingsPane>("general");

  const [transferSettings, setTransferSettingsState] = useState<TransferSettings>({
    concurrency: 4,
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

  const clearSelection = useCallback(() => {
    setSelectedKeys([]);
    setAnchorKey(null);
  }, []);

  // Stable references, like `setThemeMode` above: `TrayEventBridge` depends
  // on `openSettings` to (re)register its Tauri event listeners, and a
  // reference that changed on every provider render (as an inline closure
  // in `value` below would) forces that effect to tear down and re-`listen`
  // on every unrelated re-render of anything under `AppStoreProvider` --
  // widening the "tray event arrives while nobody is listening" gap from
  // "just after launch" to "constantly". `setSettingsPaneState` and
  // `setShowSettings` are `useState` setters, themselves stable across
  // renders, so `[]` is a correct dependency array here.
  const openSettings = useCallback((pane: SettingsPane = "general") => {
    setSettingsPaneState(pane);
    setShowSettings(true);
  }, []);

  const setSettingsPane = useCallback((pane: SettingsPane) => {
    setSettingsPaneState(pane);
  }, []);

  const closeSettings = useCallback(() => {
    setShowSettings(false);
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
    settingsPane,
    openSettings,
    setSettingsPane,
    closeSettings,
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
