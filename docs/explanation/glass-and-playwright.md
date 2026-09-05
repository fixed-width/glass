# Glass and Playwright

Use Playwright when the behaviour you need to test lives in a website or renderer. Use Glass when
you need to observe and drive the running application on a desktop, Android emulator or iOS Simulator.
An Electron or hybrid application can benefit from both: renderer tests cover page behaviour, while external
acceptance tests cover the packaged application and its native boundaries.

## Choose by the outcome you need

| You need to verify… | Start with… | What that establishes |
| --- | --- | --- |
| Website forms, navigation, DOM state and behaviour across browser engines | Playwright Test | Browser-page behaviour through [locators](https://playwright.dev/docs/locators) and assertions, with its [supported browser builds](https://playwright.dev/docs/browsers). |
| Renderer behaviour with controlled API responses | Playwright | [Network mocking](https://playwright.dev/docs/mock) lets a test supply response data and exercise error states. |
| An Electron renderer's controls and application logic | Playwright's experimental Electron API | Access to Electron windows as pages and to the main process; compatibility depends on the application's build. See the [Electron API](https://playwright.dev/docs/api/class-electron). |
| A packaged app's real native dialog, native controls or a transition into an embedded web view | Glass | External observations and input through the platform's accessibility and capture APIs. Coverage depends on what that application publishes. |
| A canvas, custom rendering or a native visual regression | Glass | Captured pixels, native input and baseline comparisons, even when semantic controls are unavailable. |
| A workflow involving two independent applications | Glass sessions for each app | Observed state transferred and verified across the two applications, with separate process and window ownership. |

For a website-only test suite, choose Playwright first. Its renderer access, browser matrix and
network controls directly serve that task. Glass becomes useful when the requirement includes the
actual application shell, native UI or another application. This is a recommendation about test
scope, not a claim that one tool is universally faster or more reliable.

## Similar labels do not imply the same observation

A Playwright role locator resolves against the page. A Glass semantic target resolves against the
accessibility information the application has published to the operating system. A form can be
visible and accessible to a renderer test while its controls are absent from Glass's tree. A DOM
locator pass also does not establish that a screen reader can use the application's native
accessibility publication. See [web content](web-content.md) for publication behaviour by engine
and platform, including the current iOS WKWebView gap.

The action modes matter too. Playwright's ordinary locator click checks that the element receives
pointer events, among other [actionability checks](https://playwright.dev/docs/actionability).
Glass's native accessibility invocation can activate a covered control. Glass pointer mode refuses
known obstructions but can report occlusion evidence as `unproven`; that does not establish the
strict claim that a covered control would refuse input. Successful dispatch alone establishes
neither the intended application result nor strict pointer safety.

Web publication and coordinate behaviour must be checked on each target engine/backend. A working
X11 fixture does not establish Wayland web conformance, and an Android WebView result does not
establish iOS WKWebView support. Treat missing controls, incorrect coordinates and unproven
requirements as limitations to investigate. Do not turn a failed semantic action into an automatic
second attempt with coordinates.

## Keep the test surfaces distinct

Playwright Test and the Playwright library provide APIs for authored tests. [Playwright MCP](https://github.com/microsoft/playwright-mcp)
exposes a configured tool surface to an agent; Glass is also an MCP server. Library capabilities,
MCP tool availability and an agent's decisions are different things. Check the connected server's
actual tool inventory before relying on an API example as an agent capability.

Using both tools normally means separate tests and fresh application instances. They can share a
build identifier, test inputs and expected results. They do not share locator IDs, window IDs,
browser contexts or trace formats. See [test an Electron or hybrid app](../how-to/test-electron-and-hybrid-apps.md)
for a renderer test and a separate packaged-app acceptance recipe.

The [interaction runner](../how-to/measure-interactions.md) can help validate the Glass scope on
your application and host. Inspect exact values, side-effect counts, failures and cleanup before
latency. Scripted MCP measurements do not measure an LLM, and measurements from different engines,
hosts, action modes or application boundaries do not establish a comparative winner.
