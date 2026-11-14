import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import {
  candidateResolution,
  type ExternalCandidateDto,
  type OpenChoiceDto,
} from "../../types/runtime";
import OpenChoiceDialog from "./OpenChoiceDialog";

/** One instance the Hub found, as the backend describes it. */
function candidate(overrides: Partial<ExternalCandidateDto> = {}): ExternalCandidateDto {
  return {
    pid: 4212,
    createdAt: "133956789012345678",
    fileName: "python.exe",
    imagePath: "D:\\ComfyUI\\python.exe",
    hasWindow: true,
    associable: true,
    reason: "还有别的进程在运行同一个程序",
    ...overrides,
  };
}

function render(choice: OpenChoiceDto): string {
  return renderToStaticMarkup(
    createElement(OpenChoiceDialog, {
      sessionName: "ComfyUI",
      choice,
      onResolve: vi.fn(),
      onClose: vi.fn(),
    }),
  );
}

describe("OpenChoiceDialog", () => {
  it("names the session and says why the Hub is asking", () => {
    const markup = render({
      reason: "有 2 个进程都在运行这条配置指定的程序，Hub 无法确定哪一个才是你要打开的。",
      candidates: [candidate(), candidate({ pid: 4300 })],
    });

    expect(markup).toContain("ComfyUI");
    expect(markup).toContain("无法确定哪一个");
    expect(markup).toContain("Hub 不会因此获得结束它的权限");
  });

  it("shows the evidence it looked at for every instance it found", () => {
    const markup = render({
      reason: "有 2 个进程都在运行这条配置指定的程序。",
      candidates: [
        candidate({ title: "ComfyUI — 秋叶整合包" }),
        candidate({ pid: 4300, title: undefined, imagePath: "D:\\ComfyUI\\python.exe" }),
      ],
    });

    // The caption where there is one, the file name where there is not, and
    // the pid and image path underneath either way.
    expect(markup).toContain("ComfyUI — 秋叶整合包");
    expect(markup).toContain("python.exe");
    expect(markup).toContain("4212");
    expect(markup).toContain("4300");
    expect(markup).toContain("D:\\ComfyUI\\python.exe");
  });

  it("lists an instance it could not inspect without offering to associate it", () => {
    const markup = render({
      reason: "另有 1 个同名进程，Hub 无法确认它们是不是同一个程序。",
      candidates: [
        candidate({
          associable: false,
          imagePath: undefined,
          createdAt: undefined,
          reason: "无法读取这个进程的映像路径（通常是权限不足）",
        }),
      ],
    });

    expect(markup).toContain("权限不足");
    // A disabled radio: the row is there so the user knows something is
    // running, and the control that would be refused is not offered.
    expect(markup).toContain("disabled");
    expect(markup).toContain('value="4212:"');
    expect(markup).toContain("没有可以关联的实例");
  });

  it("offers exactly the three answers the user has", () => {
    const markup = render({
      reason: "有 2 个进程都在运行这条配置指定的程序。",
      candidates: [candidate(), candidate({ pid: 4300 })],
    });

    for (const answer of ["取消", "新开一份", "关联选中的实例"]) {
      expect(markup).toContain(answer);
    }
  });

  it("preselects nothing, so the answer is the user's", () => {
    const markup = render({
      reason: "有 2 个进程都在运行这条配置指定的程序。",
      candidates: [candidate(), candidate({ pid: 4300 })],
    });

    // The primary control is inert until a row is picked: a default would let
    // one stray press decide which running program the entry means.
    // The primary control is inert until a row is picked: a default would let
    // one stray press decide which running program the entry means.
    expect(markup).toMatch(/disabled[^>]*>关联选中的实例/);
  });

  it("says when a candidate has no window to bring forward", () => {
    const markup = render({
      reason: "有 2 个进程都在运行这条配置指定的程序。",
      candidates: [candidate({ hasWindow: false })],
    });

    expect(markup).toContain("现在没有可以唤起的窗口");
  });

  it("carries the identity the backend needs back, not a re-derived one", () => {
    // The creation time is the half that survives the pid being reused, so it
    // travels exactly as it arrived — as a decimal string, because it does not
    // fit in a JavaScript number.
    expect(candidateResolution(candidate())).toEqual({
      kind: "associate",
      pid: 4212,
      createdAt: "133956789012345678",
    });
  });
});
