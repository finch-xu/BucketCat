import { useTranslation } from "react-i18next";
import type { AppError } from "@/lib/api";

/** Maps a rejected query/mutation's `AppError` to display text via the
 * `errors.*` i18n namespace (mirrors `src-tauri/src/error.rs`'s `code()`),
 * falling back to the generic `errors.internal` copy for codes this build's
 * dictionary doesn't have an entry for yet. Shared by the sidebar's
 * connection/bucket queries and the add-connection wizard's test button. */
export function useErrorText() {
  const { t, i18n } = useTranslation();
  return (error: AppError): string => {
    const key = `errors.${error.code}`;
    return i18n.exists(key) ? t(key, error.params) : t("errors.internal", error.params);
  };
}
