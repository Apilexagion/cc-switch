import { useCallback, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { useUnsavedChangesFlag } from "@/lib/unsavedChanges";

/** Share the shell's navigation guard with local editor back/file-change actions. */
export function useDraftLeaveConfirmation(dirty: boolean, saving = false) {
  useUnsavedChangesFlag(dirty);
  const { t } = useTranslation();
  const latest = useRef({ dirty, saving });
  latest.current = { dirty, saving };
  const [pendingLeave, setPendingLeave] = useState<(() => void) | null>(null);
  const requestLeave = useCallback((proceed: () => void) => {
    if (latest.current.saving) return;
    if (latest.current.dirty) setPendingLeave(() => proceed);
    else proceed();
  }, []);

  const confirmation = (
    <ConfirmDialog
      isOpen={pendingLeave !== null}
      title={t("common.unsavedLeaveTitle")}
      message={t("common.unsavedLeaveMessage")}
      confirmText={t("common.unsavedLeaveConfirm")}
      cancelText={t("common.unsavedLeaveCancel")}
      pending={saving}
      zIndex="top"
      onConfirm={() => {
        if (latest.current.saving) return;
        const proceed = pendingLeave;
        setPendingLeave(null);
        proceed?.();
      }}
      onCancel={() => setPendingLeave(null)}
    />
  );

  return { requestLeave, confirmation };
}
