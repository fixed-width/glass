# Capture a smaller image

Use `glass_screenshot` with `max_width` or `max_height` when a large window produces more image
pixels than you need. For example:

```json
{"max_width":1280,"max_height":720}
```

Glass shrinks the image to fit while preserving its aspect ratio. It does not resize the app window.
Omit the bounds for a native image, or crop first to keep the area you need:

```json
{"region":{"x":120,"y":80,"width":1200,"height":800},"max_width":600}
```

That crop returns a 600×400 image. The result's `image.source` is the original
`{x:120,y:80,width:1200,height:800}` rectangle, and both scale factors are 0.5. To click a control
at returned pixel `(100,50)`, map its center back to native coordinates: x is
`120 + floor((100+0.5)*1200/600) = 321`; y is `80 + floor((50+0.5)*800/400) = 181`.
Use `glass_click` with `{ "x":321, "y":181 }`, or prefer a semantic target when available.

The same limits work on `glass_do` terminal observations:

```json
{"actions":[{"action":"key","chord":"Return"}],"then":{"screenshot":{"max_width":1280}}}
```

For visual verification, save a native baseline with `glass_baseline_save`:

```json
{"name":"before"}
```

After the app changes, compare with `glass_diff`:

```json
{"name":"before","mode":"exact","include_image":true,"max_width":600}
```

The change statistics and bounding box use native pixels. Only the optional image of the changed
area is reduced. A one-pixel change may disappear in a preview and still count in `changed_pixels`.
Lossless WebP preserves the preview pixels; `pixel_exact:false` means native detail was discarded.
Do not substitute a resized preview for a baseline.

`glass_wait_stable` and `glass_wait_for_region` also accept these limits for returned images.
Image inclusion rules stay the same, and optional session traces retain exactly the returned bytes.
See the [tool reference](../reference/tools.md#image-size-controls) for metadata and edge cases.
