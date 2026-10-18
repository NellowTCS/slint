// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore unstub

import { test, expect, vi, beforeEach, afterEach } from "vitest";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { loadFile, private_api } from "../dist/index.js";

private_api.initTesting();

let dir: string;

beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "slint-live-preview-"));
    vi.stubEnv("SLINT_LIVE_PREVIEW", "1");
    vi.spyOn(console, "info").mockImplementation(() => {});
});

afterEach(() => {
    vi.unstubAllEnvs();
    vi.restoreAllMocks();
    rmSync(dir, { recursive: true });
});

/** Waits for a reload, which happens on another thread. */
function reloaded(check: () => void) {
    return vi.waitFor(check, { timeout: 10_000 });
}

function app(version: number, label = `"version ${version}"`) {
    return `import { Logic } from "logic.slint";
export { Logic }
export component App inherits Window {
    in-out property <string> name;
    out property <int> version: ${version};
    out property <string> label: ${label};
    callback clicked() -> int;
    public function click() -> int { return clicked(); }
}`;
}

test("live preview reloads a changed file and keeps what JavaScript set", async () => {
    const consoleError = vi
        .spyOn(console, "error")
        .mockImplementation(() => {});
    writeFileSync(
        join(dir, "logic.slint"),
        "export global Logic { in-out property <int> count; }",
    );
    writeFileSync(join(dir, "app.slint"), app(1));
    const ui = loadFile(join(dir, "app.slint")) as any;
    const instance = new ui.App({ name: "Ada" });
    instance.clicked = () => 42;
    instance.Logic.count = 7;
    expect(instance.version).toBe(1);

    writeFileSync(join(dir, "app.slint"), app(2));
    await reloaded(() => expect(instance.version).toBe(2));
    expect(instance.label).toBe("version 2");
    expect(instance.name).toBe("Ada");
    expect(instance.click()).toBe(42);
    expect(instance.Logic.count).toBe(7);
    expect(new ui.App().version).toBe(2);

    // A broken file keeps the previous version.
    writeFileSync(join(dir, "app.slint"), app(3, "oops"));
    await reloaded(() => expect(consoleError).toHaveBeenCalled());
    expect(instance.version).toBe(2);

    // An imported file is watched too.
    writeFileSync(
        join(dir, "logic.slint"),
        "export global Logic { in-out property <int> count; out property <int> extra: 1; }",
    );
    writeFileSync(join(dir, "app.slint"), app(4));
    await reloaded(() => expect(instance.version).toBe(4));
    expect(instance.Logic.count).toBe(7);
    expect(console.info).toHaveBeenCalledWith(
        expect.stringContaining("Live preview: reloaded"),
    );
});

test("live preview drops the instance it replaces", async () => {
    const timer = (version: number) => `export component App inherits Window {
    out property <int> version: ${version};
    callback tick();
    Timer { interval: 100ms; triggered => { root.tick(); } }
}`;
    writeFileSync(join(dir, "app.slint"), timer(1));
    const ui = loadFile(join(dir, "app.slint")) as any;
    const instance = new ui.App();
    let ticks = 0;
    instance.tick = () => ticks++;
    const elapse = () => {
        for (let i = 0; i < 10; i++) {
            private_api.mock_elapsed_time(100);
        }
    };
    elapse();
    expect(ticks).toBe(10);

    writeFileSync(join(dir, "app.slint"), timer(2));
    await reloaded(() => expect(instance.version).toBe(2));
    ticks = 0;
    elapse();
    // The timer of the replaced instance used to keep calling into JavaScript.
    expect(ticks).toBe(10);
});
