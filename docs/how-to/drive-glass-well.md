# Drive Glass well — the `glass-drive` skill

Glass works with any MCP agent as-is, with no app integration and no skill required. But an agent
drives it *more reliably* with a little guidance — and without that guidance an agent spends its first
several turns rediscovering the loop by trial and error. The habits that matter:

- **Verify with cheap text before spending a screenshot** — save a baseline, act, then check
  `glass_diff`, a `glass_do` element wait, or `glass_logs` with `wait` before decoding a new image.
- **Use accessibility before pixels** — use a selector action when one intended target is unique;
  use `glass_find_elements` when several candidates need inspection; use `glass_a11y_snapshot` only
  for broad structural diagnosis. Fall back to pixels when the app publishes no usable semantic tree.
- **Pace drags** so a frame-based GUI samples the motion, and **reach for multi-touch** where the app
  needs it.

These are packaged as [**glass-drive**](https://github.com/fixed-width/skills), an open
[Agent Skill](https://agentskills.io) that works across agents (Claude Code, Codex, Cursor,
OpenCode, …):

```bash
npx skills add fixed-width/skills -s glass-drive
```

Install it for your agent to make these habits available in later tasks. The skill stays optional;
Glass needs no app cooperation to run. New configurations use the lean profile, where actions go
through `glass_do`; see [tool profiles](../reference/tools.md#tool-profiles) when adapting older
examples that use standalone tools.

For the reasoning behind the cheap-verify loop the skill encodes, see
[the build → see → interact → debug loop](../explanation/the-loop.md); for the tools it reaches for,
see the [tool reference](../reference/tools.md).
