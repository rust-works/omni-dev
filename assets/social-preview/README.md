# Social Preview Image

Source and rendered PNG for the GitHub repository's [social preview
card](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/customizing-your-repositorys-social-media-preview)
(the Open Graph image shown when the repo is linked from HN, X, Slack,
LinkedIn, Discord, Reddit, etc.).

## Files

- `social-preview.svg` — editable source.
- `social-preview.png` — 1280×640 PNG rendered from the SVG. This is the
  file uploaded to GitHub.

## Re-rendering

```bash
rsvg-convert -w 1280 -h 640 \
  assets/social-preview/social-preview.svg \
  -o assets/social-preview/social-preview.png
```

`rsvg-convert` ships with `librsvg` (`brew install librsvg` on macOS).

## Uploading to GitHub

GitHub does not accept social previews via the API; this is a one-time
manual upload by a repo admin:

1. Open <https://github.com/rust-works/omni-dev/settings>.
2. Scroll to **Social preview**.
3. Click **Edit** → **Upload an image…** and select
   `assets/social-preview/social-preview.png`.

Constraints (per GitHub): PNG / JPG / GIF, ≤ 1 MiB, recommended
1280×640. The committed PNG satisfies all three.

The upload survives renames and persists across the repo's lifetime;
re-upload only when the design changes.
