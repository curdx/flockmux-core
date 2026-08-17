/**
 * ErrorBoundary — contains a render-time throw so one bad view doesn't
 * white-screen the whole app.
 *
 * The app renders dynamic, sometimes-malformed content (ReactMarkdown over
 * blackboard text, the ReactFlow/dagre canvas, future swarm-event variants).
 * Before this boundary, any uncaught error during render unmounted the entire
 * React root to a blank page with no recovery. Now the failing subtree is
 * replaced by a small fallback card with a retry, and the rest of the chrome
 * (header / sidebar / tabs, depending on where the boundary sits) survives.
 *
 * `resetKey` (typically the route pathname) clears the error when the user
 * navigates elsewhere — otherwise the fallback would stick across routes.
 *
 * Must be a class component: React only exposes error catching via
 * getDerivedStateFromError / componentDidCatch, which have no hook equivalent.
 */

import { Component, type ErrorInfo, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle } from "lucide-react";

interface Props {
  children: ReactNode;
  /** When this value changes, a held error is cleared (e.g. route change). */
  resetKey?: string;
}

interface State {
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // eslint-disable-next-line no-console
    console.error("ErrorBoundary caught a render error", error, info);
  }

  componentDidUpdate(prev: Props) {
    if (prev.resetKey !== this.props.resetKey && this.state.error) {
      this.setState({ error: null });
    }
  }

  private reset = () => this.setState({ error: null });

  render() {
    if (this.state.error) {
      return <ErrorFallback error={this.state.error} onReset={this.reset} />;
    }
    return this.props.children;
  }
}

function ErrorFallback({
  onReset,
}: {
  error: Error;
  onReset: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex h-full min-h-0 flex-1 flex-col items-center justify-center gap-4 bg-surface-primary p-8 text-center">
      <span className="flex size-12 items-center justify-center rounded-full bg-status-danger-soft text-state-danger">
        <AlertTriangle className="size-6" />
      </span>
      <div className="flex max-w-md flex-col gap-1">
        <h2 className="font-heading text-base font-semibold text-foreground-primary">
          {t("error.title")}
        </h2>
        <p className="font-caption text-xs text-foreground-tertiary">
          {t("error.body")}
        </p>
      </div>
      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={onReset}
          className="rounded-md bg-accent-primary px-3 py-1.5 text-xs font-semibold text-foreground-on-accent hover:bg-accent-primary-deep"
        >
          {t("error.retry")}
        </button>
        <button
          type="button"
          onClick={() => window.location.reload()}
          className="rounded-md border border-border-subtle px-3 py-1.5 text-xs text-foreground-secondary hover:bg-surface-tertiary"
        >
          {t("error.reload")}
        </button>
      </div>
    </div>
  );
}
