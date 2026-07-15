import { describe, expect, test } from "bun:test";
import { main } from "./index";

describe("pi-sidecar stub", () => {
  test("main is callable", () => {
    expect(typeof main).toBe("function");
    main();
  });
});
