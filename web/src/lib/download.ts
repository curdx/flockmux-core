import { api } from "@/api/http";
import { toast } from "@/lib/toast";
import i18n from "@/i18n";

/**
 * Download a recording's asciicast as a real file.
 *
 * Why not a plain `<a href={castUrl} download>`? In the packaged Tauri app the
 * recording URL is cross-origin (webview is `tauri.localhost`, backend is
 * `127.0.0.1:7777`) and the backend serves it as `application/x-asciicast`
 * with NO `Content-Disposition` header — so the browser ignores the `download`
 * attribute and the webview *navigates* to the raw JSON-lines, stranding the
 * user with no back button (P0-3). It only "works" in dev because there the
 * URL is same-origin. Fetching as a blob and saving via a same-origin
 * `blob:` object URL makes the download behave identically everywhere.
 */
export async function downloadRecordingCast(id: string): Promise<void> {
  try {
    const res = await fetch(api.recordingCastUrl(id));
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const blob = await res.blob();
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `${id}.cast`;
    document.body.appendChild(a);
    a.click();
    a.remove();
    // Revoke after a tick so the download has a chance to grab the blob.
    window.setTimeout(() => URL.revokeObjectURL(url), 10_000);
  } catch (e) {
    toast.error(i18n.t("recordings.downloadFailed", { defaultValue: "下载录像失败" }), {
      description: (e as Error)?.message,
    });
  }
}

/**
 * Save a text payload (markdown / json / …) as a file. Same blob + object-URL
 * trick as downloadRecordingCast above, so downloads behave identically in the
 * Tauri webview and the browser. Fire-and-forget: nothing here can throw
 * meaningfully, so callers don't await a result.
 */
export function downloadTextFile(
  filename: string,
  content: string,
  mime = "text/plain",
): void {
  const blob = new Blob([content], { type: `${mime};charset=utf-8` });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  // Revoke after a tick so the download has a chance to grab the blob.
  window.setTimeout(() => URL.revokeObjectURL(url), 10_000);
}
