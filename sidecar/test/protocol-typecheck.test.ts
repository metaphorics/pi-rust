import { describe, expect, test } from "bun:test";
import type { AgentToolResult } from "@earendil-works/pi-coding-agent";
import type { ToolExecuteResultDto } from "../src/protocol.ts";

describe("protocol typecheck mirror", () => {
  test("ToolExecuteResultDto matches AgentToolResult fields", () => {
    // 1. Verify exact field type alignment for terminate and addedToolNames
    type TargetFields = "addedToolNames" | "terminate";
    type DtoSubset = Pick<ToolExecuteResultDto, TargetFields>;
    type AgentSubset = Pick<AgentToolResult<unknown>, TargetFields>;

    // Assert bidirectional assignability
    const _checkDtoToAgent = (_val: DtoSubset): AgentSubset => _val;
    const _checkAgentToDto = (_val: AgentSubset): DtoSubset => _val;

    // 2. Prove ToolExecuteResultDto satisfies a literal containing both fields
    const _literalCheck = {
      content: [],
      isError: false,
      addedToolNames: ["my-tool"],
      terminate: true,
    } satisfies ToolExecuteResultDto;

    expect(true).toBe(true);
  });
});
