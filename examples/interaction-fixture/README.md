# Interaction fixture

A deterministic local application fixture for the [interaction benchmark](../../tools/interaction-bench/README.md).
The fixture uses ordinary HTML controls and JavaScript. There is no automation or state-reading API.
Drivers observe values and counters through their public accessibility tools.

The runner serves `index.html`, `fixture.js` and `frame.html` on two owned loopback ports. The second
port supplies the cross-origin frame. Builds and browser installation happen before a run.

Select a case with `?case=large-form` (see the runner's `list` command). Each attempt loads a fresh
document. Delay and motion default to 3000 ms and accept `delay_ms` / `motion_ms` query parameters;
the runner records these values and uses the same settings for every driver.

- The form exposes the exact saved value and submission count, independently of the editable field.
- Delayed/moving controls have visible triggers and activation counters.
- Duplicate controls belong to Billing and Shipping groups with separate counters.
- Replacement retains a renamed old target and gives the new target its own counter.
- Frame controls share an action name with an outer control and expose separate counters.
- `occluded` gives the target and cover identical bounds; `occluded-distinct` places a narrower cover
  across the target's center. Both count activations of the target and the cover.

The existing `web-role-fixture` remains unchanged for accessibility role probes.
