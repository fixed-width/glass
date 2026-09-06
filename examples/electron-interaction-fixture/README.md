# Packaged interaction fixture

This Linux Electron application packages the shared account form from `../interaction-fixture`.
Saving copies the entered account name and increments the submission counter. **Review saved value**
opens an app-owned native confirmation dialog. Confirming copies the saved value into **Confirmed
value** and increments **Confirmation count**; cancelling leaves both unchanged.

Build before running a benchmark:

```bash
npm ci
npm run package
```

`package-lock.json` pins Electron 44.2.0. The build copies its distribution into
`dist/interaction-fixture-linux`, installs the application under `resources/app`, and produces a
`fixture-build.json` with the runtime version and every packaged file's digest. Launch
`dist/interaction-fixture-linux/interaction-fixture`. The distribution includes Electron's license
and third-party notices. Packaging currently supports Linux only.

There is no development server or debugging endpoint. The preload IPC is ordinary application logic
for opening the native dialog. Automation uses the external UI through Glass's public MCP tools.

The benchmark declares native semantic actions for the form. For the dialog it observes the native
window, selects and focuses it, presses Return for the default **Confirm value** action, then observes
closure and the exact confirmed value/count in the main window. This checks completion across the
window boundary; it does not establish semantic accessibility of the native dialog's buttons.
