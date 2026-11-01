import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { useSessionRegistry, type SessionRegistry } from "./useSessionRegistry";
import { useMediaQuery } from "./useMediaQuery";

vi.mock("./useBackendPing", () => ({
  useBackendPing: () => ({ state: "connected", version: "0.1.0" }),
}));
vi.mock("./useSessionRegistry", () => ({ useSessionRegistry: vi.fn() }));
vi.mock("./useMediaQuery", () => ({ useMediaQuery: vi.fn() }));
vi.mock("./tauriWindowHost", () => ({ tauriWindowHost: () => null }));
// xterm needs a browser; the empty workspace must never mount it.
vi.mock("../components/terminal/TerminalHost", () => ({
  default: () => createElement("div", { "data-testid": "terminal-host" }),
}));

let registry: SessionRegistry;
beforeEach(() => {
  registry = {
    sessions: [],
    source: "backend",
    live: true,
    loading: false,
    error: null,
    errorAt: 0,
    initializationError: null,
    configReport: { fileStatus: "missing", configPath: "config.yaml", sessions: [], errors: [] },
    configReportError: null,
    activate: vi.fn(),
    resolveOpen: vi.fn(),
    stop: vi.fn(),
    restart: vi.fn(),
    forceStop: vi.fn(),
    openUrl: vi.fn(),
    openDirectory: vi.fn(),
    createTerminal: vi.fn(),
    addApplication: vi.fn(),
    saveTerminal: vi.fn(),
    closeTerminal: vi.fn(),
    closingSessionIds: new Set(),
    removeApplication: vi.fn(),
  };
  vi.mocked(useSessionRegistry).mockReturnValue(registry);
  vi.mocked(useMediaQuery).mockReturnValue(false);
});

function render() {
  return renderToStaticMarkup(createElement(App));
}

describe("empty workspace shell", () => {
  it.each(["missing", "loaded"] as const)(
    "keeps navigation and both creation entries with a %s config",
    (fileStatus) => {
      registry.configReport!.fileStatus = fileStatus;
      const markup = render();
      expect(markup).toContain('class="app-main__rail"');
      expect(markup).toContain('aria-label="搜索会话"');
      expect(markup).toContain("新建 PowerShell");
      expect(markup).toContain("添加应用");
      expect(markup).toContain("左下方");
      expect(markup).not.toContain('aria-label="配置诊断"');
      expect(markup).not.toContain('class="session-row');
      expect(markup).not.toContain('role="tab"');
      expect(markup).not.toContain("terminal-host");
      expect(markup).not.toContain("重启应用");
    },
  );

  it("retains the shell during synchronization without announcing an empty registry", () => {
    registry.loading = true;
    const markup = render();
    expect(markup).toContain('class="app-main__rail"');
    expect(markup).toContain('aria-label="会话同步状态"');
    expect(markup).not.toContain('class="workspace__empty-hint"');
  });

  it("still reports real configuration failures beside the usable sidebar", () => {
    registry.configReportError = "Cannot read configuration";
    const markup = render();
    expect(markup).toContain('class="app-main__rail"');
    expect(markup).toContain('role="alert"');
    expect(markup).toContain("Cannot read configuration");
  });

  it("points narrow windows to the existing sidebar drawer", () => {
    vi.mocked(useMediaQuery).mockReturnValue(true);
    const markup = render();
    expect(markup).toContain('aria-label="打开会话列表"');
    expect(markup).toContain("请打开左上角会话列表");
    expect(markup).not.toContain("左下方");
  });
});
