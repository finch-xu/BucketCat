import { describe, expect, it } from "vitest";
import { groupOptions, type SelectOption } from "./select-options";

const OPTIONS: SelectOption[] = [
  { value: "a", label: "甲", group: "g1" },
  { value: "b", label: "乙", group: "g2" },
  { value: "c", label: "丙", group: "g1" },
];

describe("groupOptions", () => {
  it("returns one ungrouped section when no groups are given", () => {
    expect(groupOptions(OPTIONS)).toEqual([{ group: null, options: OPTIONS }]);
  });

  it("orders sections by the groups array, not by option order", () => {
    const sections = groupOptions(OPTIONS, [
      { key: "g2", label: "第二组" },
      { key: "g1", label: "第一组" },
    ]);
    expect(sections.map((s) => s.group?.key)).toEqual(["g2", "g1"]);
    expect(sections[1].options.map((o) => o.value)).toEqual(["a", "c"]);
  });

  // 没有任何选项的分组不该渲染出一个空标题。
  it("drops groups that have no options", () => {
    const sections = groupOptions(OPTIONS, [
      { key: "g1", label: "第一组" },
      { key: "empty", label: "空组" },
    ]);
    // "b" (group "g2") is unknown to this call's groups list, so it still
    // surfaces in a leading ungrouped section per the documented contract
    // (see the next test) -- only the empty "empty" group is dropped.
    expect(sections.map((s) => s.group?.key)).toEqual([undefined, "g1"]);
  });

  // 分组模式下，group 不在 groups 里（或没有 group）的选项必须仍然可选，
  // 否则"保留当前值"这类动态选项会凭空消失。
  it("keeps ungrouped options in a leading ungrouped section", () => {
    const withLoose: SelectOption[] = [{ value: "x", label: "散项" }, ...OPTIONS];
    const sections = groupOptions(withLoose, [{ key: "g1", label: "第一组" }]);
    expect(sections[0].group).toBeNull();
    expect(sections[0].options.map((o) => o.value)).toEqual(["x", "b"]);
    expect(sections[1].group?.key).toBe("g1");
  });
});
