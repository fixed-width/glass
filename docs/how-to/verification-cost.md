# Measure what the verification loop costs

glass verifies work on the CPU: an accessibility snapshot, a text-only diff (`changed_pct`
+ bbox, no image unless you ask), and `wait_for_*`. The alternative — a screenshot after
every step — puts an image into the agent's context each time. This page measures the
difference on one fixed task so you can put a number on it for your own model.

## The task

Against `glass-fixture-egui` (a 400×300 window with a **Text** field, a **Value** slider,
and an **Apply** button), driven two ways to the same end state (the app logs
`[fixture] apply`):

- **Semantic (text-only):** `glass_a11y_snapshot` → `glass_click_element` (focus field) →
  `glass_type` → `glass_set_value` (slider) → `glass_click_element` (Apply) →
  `glass_wait_for_log`. No images.
- **Screenshot-every-step:** `glass_screenshot` → click/type → `glass_screenshot` → drag →
  `glass_screenshot` → click → `glass_screenshot`. See-act-see.

## Run it

```bash
./scripts/verification-cost.sh
cat target/verification-cost.json
```

On-demand contributor tooling (needs Xvfb + an AT-SPI bus); it is `#[ignore]`d and not part
of the per-PR test gate. The structural numbers (round-trips, image count, image dimensions)
are stable across repeated runs; the approximate text-token counts can drift by a byte or two
because `glass_wait_for_log`'s response embeds a wall-clock `elapsed_ms`.

## What it records

Round-trips, request/response **bytes**, an approximate text-token count
(`text_bytes ÷ 4`), and for each screenshot its **width × height**. It does **not** convert
images to tokens — glass is model-agnostic, and a tokenizer is a dependency it declines to
carry. You convert, below.

## Results

| Arm | Round-trips | Text tokens (approx) | Images | Image size |
|-----|-------------|-----------------------|--------|------------|
| Semantic (text-only) | 6 | 349 | 0 | — |
| Screenshot-every-step | 8 | 301 | 4 | 400×300 |

## Turning images into tokens (your model)

Apply your model's published vision-token formula to each recorded image. Two common ones,
for a 400×300 image:

- **Anthropic (Claude):** `tokens ≈ (width × height) / 750` → (400×300)/750 = **160**
  tokens/image → screenshot arm ≈ 160 × 4 = **640** image tokens, plus its 301 text tokens
  ≈ **941** tokens total. (See Anthropic's vision docs for the current formula.)
- **OpenAI (GPT-class, high detail):** `85 + 170 × tiles`. High detail first scales the
  image so its **shortest side is 768px**, then counts 512-px tiles — so a 400×300
  screenshot scales up to 1024×768 = 2×2 = **4 tiles** → 85 + 170×4 = **765** tokens/image →
  765 × 4 = **3060** image tokens, plus 301 text tokens ≈ **3361** tokens total. (Newer
  OpenAI models tokenize images with a patch-based scheme rather than tiles, so check your
  model's current vision docs.)

On this task the text-only arm (349 tokens) costs roughly **2.7×** less than the screenshot
arm under Anthropic's formula (941 tokens), and roughly **9.6×** less under OpenAI's
high-detail formula (3361 tokens). The gap is structural — images versus text — not an artifact of task length: a
longer task adds a screenshot per verification point to the screenshot arm and only a few
hundred more text bytes to the semantic arm.

## Scaling to your window size

400×300 is a small fixture window, kept small so the harness runs fast and deterministically
under Xvfb. That smallness makes the ratios above conservative: image-token cost scales with
pixel count, so a normal application window costs proportionally more per screenshot — while
the semantic arm's text cost barely moves, since it's driven by the number of elements and
log lines involved, not by how many pixels the window has.

Recompute for your own window by taking its width × height and applying the same formula.
For example, a 1280×800 app window under Claude's formula:

```
(1280 × 800) / 750 ≈ 1365 tokens/image
× 4 screenshots      ≈ 5460 image tokens
+ 301 text tokens     ≈ 5761 tokens for the screenshot arm
```

against 349 for the semantic arm — roughly **16.5×** less, not 2.7×. The 400×300 fixture
number is a floor, not a representative figure: we report primitives (round-trips, bytes,
image dimensions) rather than a single canned ratio so you can plug in your own window size
and model and get a number that actually applies to your app.

> The approximate text-token count uses bytes ÷ 4; the image formulas are the published,
> per-model ones. We report primitives (round-trips, bytes, image dimensions) so the number
> survives a change of tokenizer.
