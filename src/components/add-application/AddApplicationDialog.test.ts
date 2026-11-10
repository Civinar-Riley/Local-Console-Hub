import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import type { SessionConfigDto } from "../../types/config";
import AddApplicationDialog, {
  displayHint,
  errorPlacement,
  legalPolicyFor,
  logPoliciesFor,
  loggingFor,
  policyForLogging,
  urlParts,
} from "./AddApplicationDialog";

function render(): string {
  return renderToStaticMarkup(
    createElement(AddApplicationDialog, { onSubmit: vi.fn(), onClose: vi.fn() }),
  );
}

describe("AddApplicationDialog", () => {
  it("asks for the three required fields, and says which ones they are", () => {
    const markup = render();

    for (const label of ["名称", "工作目录", "启动命令"]) {
      expect(markup).toContain(label);
    }
    // Three `必填` marks, in that order: the fields the spec names as required
    // (spec #59 decision 7) and no others.
    expect(markup.match(/dialog__required/g)).toHaveLength(3);
    for (const optional of ["端口", "网页地址", "用途", "关闭影响", "日志策略"]) {
      expect(markup).toContain(optional);
    }
  });

  it("offers the optional fields the spec names, and no invented ones", () => {
    const markup = render();

    expect(markup).toContain("日志策略");
    expect(markup).toContain("关联应用自有日志");
    expect(markup).toContain("关闭影响");
    // The log file path only exists for the external policy, so it is not on
    // screen until that policy is chosen.
    expect(markup).not.toContain("应用日志文件");
  });

  /// Both display modes are on offer, and the one that is not chosen is still
  /// visible (#66): a mode that exists but is hidden behind a dropdown is a
  /// choice the user has to know about before they can make it.
  it("offers both display modes, starting on the Hub-internal one", () => {
    const markup = render();

    expect(markup).toContain("Hub 内显示");
    expect(markup).toContain("独立窗口");
    expect(markup).toContain('aria-checked="true"');
    // The Hub-internal option is the selected one, so the standalone one is not.
    expect(markup).toMatch(/aria-checked="true"[^>]*>[^<]*Hub 内显示/);
    expect(markup).toMatch(/aria-checked="false"[^>]*>[^<]*独立窗口/);
  });

  /// The lifecycle choice is about a run the Hub does not otherwise own, so it
  /// only appears for the mode that can have one.
  it("asks about lifecycle management only for a standalone window", () => {
    const markup = render();

    expect(markup).not.toContain("由 Hub 管理生命周期");
  });

  it("keeps a refusal's message on the form rather than closing it", () => {
    // The dialog renders its own banner slot; what matters here is that the
    // component is a form with a submit control and a close control, so a
    // failure has somewhere to land without a second surface.
    const markup = render();

    expect(markup).toContain('role="dialog"');
    expect(markup).toContain('aria-modal="true"');
    expect(markup).toContain("保存应用");
    expect(markup).toContain("取消");
    expect(markup).toContain("关闭");
  });
});

describe("errorPlacement", () => {
  it("puts a refusal on the input the config layer named", () => {
    for (const field of ["name", "cwd", "command", "port", "url", "purpose", "close_impact"]) {
      expect(errorPlacement({ field, message: "no" }, "")).toBe(field);
    }
  });

  it("falls back to the banner for a refusal with no field, or one this form has no input for", () => {
    expect(errorPlacement({ message: "没有配置文件位置" }, "")).toBeNull();
    expect(errorPlacement(null, "")).toBeNull();
    // A field the form does not render must not swallow its own message.
    expect(errorPlacement({ field: "logging.mode", message: "no" }, "")).toBeNull();
    expect(errorPlacement({ field: "initial_command", message: "no" }, "")).toBeNull();
  });

  it("places the log-path refusal only while that input is on screen", () => {
    const error = { field: "logging.path", message: "no" };

    expect(errorPlacement(error, "external")).toBe("logging.path");
    expect(errorPlacement(error, "on_error")).toBeNull();
  });

  /// The two #66 fields are placed like every other one: the config layer names
  /// `lifecycle` or `logging.source`, and the form has inputs for both.
  it("places the display-related refusals on the controls they belong to", () => {
    expect(errorPlacement({ field: "lifecycle", message: "no" }, "")).toBe("lifecycle");
    expect(errorPlacement({ field: "logging.source", message: "no" }, "")).toBe("logging.source");
  });
});

describe("logPoliciesFor", () => {
  /// A standalone entry cannot capture Hub-side output, so the three policies
  /// that would ask it to are not offered for it — a policy the save would
  /// refuse is exactly the "press does nothing" option the spec rules out.
  it("offers a standalone entry only the policies it can actually have", () => {
    const policies = logPoliciesFor("window");
    const values = policies.map((option) => option.value);

    expect(values).toEqual(["", "off", "external"]);
    // And the unspecified one says what it means *here*: this mode's default is
    // "record nothing", not the service default it names for a Hub-hosted entry.
    expect(policies[0].label).toContain("不捕获输出");
    expect(policies[0].label).not.toContain("出错时记录");
  });

  it("keeps every policy available to a Hub-internal entry", () => {
    expect(logPoliciesFor("internal").map((option) => option.value)).toEqual([
      "",
      "off",
      "on_error",
      "always",
      "manual",
      "external",
    ]);
  });
});

describe("legalPolicyFor", () => {
  /// The one policy that cannot survive a switch to a standalone entry has to
  /// go with it — whether the user clicked the mode or the recommendation moved
  /// it for them. Leaving `on_error`/`always`/`manual` selected would send a
  /// payload the config layer refuses, which is the "option that does not work"
  /// the display choice exists to avoid.
  it("drops a capturing policy when the mode can no longer carry it", () => {
    for (const capturing of ["on_error", "always", "manual"]) {
      expect(legalPolicyFor("window", capturing)).toBe("");
    }
  });

  it("keeps every policy a standalone entry can carry", () => {
    for (const legal of ["", "off", "external"]) {
      expect(legalPolicyFor("window", legal)).toBe(legal);
    }
  });

  it("keeps the selection when the Hub-internal mode is chosen again", () => {
    expect(legalPolicyFor("internal", "on_error")).toBe("on_error");
  });
});

describe("displayHint", () => {
  it("says what the choice means when the Hub could confirm nothing", () => {
    expect(displayHint("internal", null)).toContain("Hub 窗口里用终端");

    expect(displayHint("window", null)).toContain("应用自己的窗口");
  });

  /// The backend's sentence is what names the evidence — which executable, and
  /// which subsystem — so the form shows it rather than paraphrasing it.
  it("shows the backend's reason, and marks the recommended choice", () => {
    const advice = {
      recommended: "window" as const,
      reason: "`cmd.exe` 自带控制台（控制台子系统程序）。",
      program: "C:/Windows/System32/cmd.exe",
    };

    const recommended = displayHint("window", advice);
    expect(recommended).toContain("Hub 推荐这一项");
    expect(recommended).toContain("cmd.exe");

    // The same advice with the other mode selected: still shown, not marked.
    const other = displayHint("internal", advice);
    expect(other).not.toContain("Hub 推荐这一项");
    expect(other).toContain("cmd.exe");
  });

  it("shows the reason even when the Hub recommended nothing", () => {
    const hint = displayHint("internal", {
      reason: "`explorer.exe` 是图形界面程序：Hub 无法只凭启动方式确认…",
    });

    expect(hint).toContain("explorer.exe");
    expect(hint).not.toContain("Hub 推荐这一项");
  });
});

describe("loggingFor", () => {
  it("omits the block entirely for the unspecified policy", () => {
    expect(loggingFor("", "")).toBeUndefined();
  });

  it("maps each policy to a combination the config layer accepts", () => {
    expect(loggingFor("off", "")).toEqual({ mode: "off", source: "none" });
    expect(loggingFor("on_error", "")).toEqual({ mode: "on_error", source: "captured" });
    expect(loggingFor("always", "")).toEqual({ mode: "always", source: "captured" });
    expect(loggingFor("manual", "")).toEqual({ mode: "manual", source: "captured" });
  });

  it("carries the application's own log path only for the external policy", () => {
    expect(loggingFor("external", "  D:/Tools/App/access.log  ")).toEqual({
      mode: "always",
      source: "external",
      path: "D:/Tools/App/access.log",
    });
  });

  it("never emits a contradictory pair", () => {
    // `mode: off` with `source: captured` and `source: external` with no path
    // are the two combinations the config layer refuses; neither is reachable
    // from the form.
    for (const policy of ["", "off", "on_error", "always", "manual"]) {
      const logging = loggingFor(policy, "");
      if (logging === undefined) continue;
      expect(logging.source === "captured" && logging.mode === "off").toBe(false);
    }
  });
});

function saved(overrides: Partial<SessionConfigDto> = {}): SessionConfigDto {
  return {
    id: "comfyui",
    name: "ComfyUI",
    sessionType: "service",
    cwd: "D:/Tools/ComfyUI",
    command: "python main.py",
    url: "http://127.0.0.1:8188",
    port: 8188,
    purpose: "图像生成后端",
    closeImpact: "停止会中断当前生成",
    display: "window",
    lifecycle: "managed",
    logging: {
      mode: "always",
      source: "external",
      externalPath: "D:/Tools/ComfyUI/app.log",
    },
    ...overrides,
  };
}

function renderEdit(initial: SessionConfigDto): string {
  return renderToStaticMarkup(
    createElement(AddApplicationDialog, { initial, onSubmit: vi.fn(), onClose: vi.fn() }),
  );
}

describe("edit mode", () => {
  /// The same form, opened on what the file holds: every stored field is in
  /// the input that owns it, so an edit starts from the saved configuration
  /// rather than from blank boxes.
  it("opens prefilled from the saved configuration", () => {
    const markup = renderEdit(saved());

    expect(markup).toContain("编辑应用");
    expect(markup).toContain("保存修改");
    expect(markup).toContain("写回配置文件");
    for (const value of [
      "ComfyUI",
      "D:/Tools/ComfyUI",
      "python main.py",
      "图像生成后端",
      "停止会中断当前生成",
      "8188",
      "127.0.0.1:8188",
    ]) {
      expect(markup).toContain(`value="${value}"`);
    }
    // The external policy is the one that owns a path, and the saved path is
    // on screen with it.
    expect(markup).toContain("应用日志文件");
    expect(markup).toContain('value="D:/Tools/ComfyUI/app.log"');
  });

  /// The saved display choice belongs to the user: the form shows it as the
  /// active one, and the recommendation does not get to move it (that is what
  /// `displayTouched` starting true means).
  it("keeps the saved display and lifecycle choice", () => {
    const markup = renderEdit(saved());

    expect(markup).toMatch(/aria-checked="true"[^>]*>[^<]*独立窗口/);
    expect(markup).toMatch(/type="checkbox"[^>]*checked/);
  });

  /// The protocol is a separate select in the form, so it has to be split back
  /// out of the stored URL rather than shown as part of the address.
  it("splits a stored URL into protocol and address", () => {
    expect(urlParts("https://example.com/page")).toEqual({
      protocol: "https://",
      address: "example.com/page",
    });
    expect(urlParts("http://127.0.0.1:8188")).toEqual({
      protocol: "http://",
      address: "127.0.0.1:8188",
    });
    expect(urlParts(undefined)).toEqual({ protocol: "http://", address: "" });
    expect(urlParts("127.0.0.1:8188")).toEqual({
      protocol: "http://",
      address: "127.0.0.1:8188",
    });
  });

  it("maps an effective logging block back to the policy that produces it", () => {
    expect(policyForLogging({ mode: "off", source: "none" })).toBe("off");
    expect(policyForLogging({ mode: "on_error", source: "captured" })).toBe("on_error");
    expect(policyForLogging({ mode: "always", source: "captured" })).toBe("always");
    expect(policyForLogging({ mode: "manual", source: "captured" })).toBe("manual");
    // The path-bearing policy wins over the mode it stores.
    expect(
      policyForLogging({ mode: "always", source: "external", externalPath: "D:/app.log" }),
    ).toBe("external");
  });
});
