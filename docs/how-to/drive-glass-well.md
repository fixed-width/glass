# Drive glass well — the `glass-drive` skill

glass works with any MCP agent as-is, with no app integration and no skill required. But an agent
drives it *more reliably* with a little guidance — and without that guidance an agent spends its first
several turns rediscovering the loop by trial and error. The habits that matter:

- **Verify with cheap text before spending a screenshot** — save a baseline, act, then check
  `glass_diff` or a `glass_wait_for_*` call (all text-only) before decoding a new image.
- **Fall back from the accessibility tree to pixels** — try `glass_a11y_snapshot` first for
  deterministic element addressing, and drop to screenshots on a canvas/black-box app.
- **Pace drags** so a frame-based GUI samples the motion, and **reach for multi-touch** where the app
  needs it.

These are packaged as [**glass-drive**](https://github.com/fixed-width/skills), an open
[Agent Skill](https://agentskills.io) that works across agents (Claude Code, Codex, Cursor,
OpenCode, …):

```bash
npx skills add fixed-width/skills -s glass-drive
```

Install it once for your agent and it arrives already knowing the driving loop, instead of learning it
on your time. It stays optional — glass needs neither the skill nor any app cooperation to run — but
it is the single highest-leverage thing you can add when pointing an agent at glass.

For the reasoning behind the cheap-verify loop the skill encodes, see
[the build → see → interact → debug loop](../explanation/the-loop.md); for the tools it reaches for,
see the [tool reference](../reference/tools.md).
