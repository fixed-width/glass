# Test an Electron or hybrid app

Keep renderer checks and packaged-application acceptance as separate tests of the same build.
The example below uses the repository's Linux [packaged Electron fixture](../../examples/electron-interaction-fixture/README.md):
a form saves an account name, then an app-owned native dialog confirms that saved value. For tool
selection and coverage limits, see [Glass and Playwright](../explanation/glass-and-playwright.md).

## Check the renderer with Playwright

Build the fixture with `npm ci` and `npm run package` in `examples/electron-interaction-fixture`.
In a separate test project with a locked `@playwright/test` dependency, save this as
`electron-renderer.spec.ts`. It uses Playwright's [experimental Electron API](https://playwright.dev/docs/api/class-electron)
and [firstWindow](https://playwright.dev/docs/api/class-electronapplication#electron-application-first-window)
to test the packaged renderer. Packages that disable Electron's debugging support may need a
separate test build; record which build was tested.

```typescript
import { test, expect, _electron as electron } from '@playwright/test';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

test('the renderer saves the account exactly once', async () => {
  test.setTimeout(60_000);
  const executablePath = process.env.ELECTRON_APP;
  if (!executablePath) throw new Error('Set ELECTRON_APP to the packaged executable');
  const profile = await mkdtemp(join(tmpdir(), 'electron-renderer-'));
  let app;
  try {
    app = await electron.launch({
      executablePath,
      args: ['--ozone-platform=x11', '--disable-gpu', `--user-data-dir=${profile}`],
    });
    const page = await app.firstWindow();
    await expect(page.getByRole('textbox', { name: 'Fixture ready', exact: true }))
      .toHaveValue('ready');
    await page.getByRole('textbox', { name: 'Account name', exact: true }).fill('Ada');
    await page.getByRole('button', { name: 'Save account', exact: true }).click();
    await expect(page.getByRole('textbox', { name: 'Saved value', exact: true }))
      .toHaveValue('Ada');
    await expect(page.getByRole('textbox', { name: 'Submission count', exact: true }))
      .toHaveValue('1');
  } finally {
    try { await app?.close(); } finally { await rm(profile, { recursive: true, force: true }); }
  }
});
```

On Linux, run from that test project on an owned Xvfb display:

```bash
ELECTRON_APP=/checkout/glass/examples/electron-interaction-fixture/dist/interaction-fixture-linux/interaction-fixture \
  xvfb-run -a npx playwright test electron-renderer.spec.ts --workers=1 --retries=0
```

This checks the renderer's saved value and count. It does not open or prove interaction with the
native confirmation dialog. Playwright can [stub Electron dialog methods](https://playwright.dev/docs/api/class-electron)
when testing application logic; a stubbed response is a different result from operating the real dialog.

## Check the native boundary with Glass

Use a fresh application instance. The existing `electron-form` runner performs the full external
workflow through Glass MCP, including the real native dialog. Follow the Linux/X11 prerequisites in
[measure interactions](measure-interactions.md), build `glass-mcp`, and save this configuration
outside the checkout with your absolute paths:

```json
{
  "cases": ["electron-form"],
  "repetitions": 1,
  "warmups": 0,
  "allow_dirty": true,
  "sandbox": "off",
  "action_timeout_ms": 20000,
  "attempt_timeout_ms": 240000,
  "applications": {
    "electron": {
      "executable": "/checkout/glass/examples/electron-interaction-fixture/dist/interaction-fixture-linux/interaction-fixture",
      "bundle": "/checkout/glass/examples/electron-interaction-fixture/dist/interaction-fixture-linux"
    }
  }
}
```

Run from the Glass checkout:

```bash
cargo build --release --locked -p glass-mcp
python3 tools/interaction-bench/run.py preflight --config /absolute/path/electron.json
python3 tools/interaction-bench/run.py run --config /absolute/path/electron.json
```

This is one diagnostic attempt of an owned local fixture. The runner uses isolated displays and
profiles, turns Glass containment off, and passes Electron's `--no-sandbox` and accessibility flags.
Those settings are recorded; this run does not validate production containment or baseline
accessibility publication. For your app, use its normal launch and containment settings and verify
its published controls before selecting an action recipe.

The acceptance sequence is:

1. Observe the form, confirm native focus, type `Ada`, save once, and verify **Saved value = Ada**
   and **Submission count = 1**.
2. Invoke **Review saved value**, observe exactly one **Confirm account** window, select and focus
   that observed window, then press Return for its declared default **Confirm value** action.
3. Observe dialog closure, select the main window and verify **Confirmed value = Ada** and
   **Confirmation count = 1**, including a follow-up observation that the confirmation count stays `1`.
4. Stop the owned app and retain the outcome, wire evidence and cleanup result.

The dialog check proves native-window/default-key completion. It does not establish semantic
accessibility of the dialog's buttons, and its Return action is specific to this fixture. When
adapting the sequence, use fresh `glass_list_windows` IDs and verify the selected window and intended
default action before input. A failed or lost reply must not cause the Save action to be replayed.

The runner prints its results directory. Run `validate` and `summarize` as described in
[measure application boundaries](measure-application-boundaries.md). Require the declared outcome
and healthy cleanup; successful evidence validation can also describe a failed task. Preserve that
failure before starting a fresh attempt.

## Extend the pattern to a hybrid app

Keep page logic in renderer tests where that runtime offers a supported automation surface.
Playwright also has [experimental Android browser/WebView support](https://playwright.dev/docs/api/class-android),
with its own device and ADB requirements. Give Glass a separate native → embedded form → native result
test, checking the exact transferred value
and each side-effect count. The existing `android-boundary` case demonstrates that sequence; the
[application-boundary guide](measure-application-boundaries.md) supplies its setup. The iOS
publication probe establishes which controls can be observed and does not claim a working WKWebView
interaction flow.

Link each run to the same application build and scenario, while retaining each tool's evidence and
outcome separately. Use an optional [Glass session trace](record-session-evidence.md) for requested
observations, and [bounded screenshots](capture-a-smaller-image.md) when visual evidence is useful.
Native baselines and exact comparisons remain at capture resolution. Expand the host/engine matrix
only after checking publication, action mode, native-window behaviour and cleanup on each target.
