# OAuth Consent Screen Logo

Source and rendered logo for the Google Cloud OAuth consent screen (Google
Auth Platform → Branding → App logo), used by omni-dev's Drive/Gmail OAuth
integration. The design matches the existing VS Code extension icon
([`editors/vscode/media/icon.svg`](../../editors/vscode/media/icon.svg)):
a blue-to-indigo gradient rounded square with a white node-graph glyph.

## Files

- `logo.svg` — editable source, 120×120.
- `logo.png` — 120×120 PNG rendered from the SVG, transparent background.
  This is the file to upload to the consent screen (Google accepts JPG,
  PNG, or BMP, ≤ 1 MiB, square, 120×120 recommended).
- `logo.jpg` / `logo.bmp` — same render flattened onto a white background,
  provided as alternates since JPG/BMP don't support transparency.

## Re-rendering

```bash
rsvg-convert -w 120 -h 120 \
  assets/oauth-logo/logo.svg -o assets/oauth-logo/logo.png

rsvg-convert -w 120 -h 120 --background-color white \
  assets/oauth-logo/logo.svg -o /tmp/logo-flat.png
sips -s format jpeg /tmp/logo-flat.png --out assets/oauth-logo/logo.jpg
sips -s format bmp /tmp/logo-flat.png --out assets/oauth-logo/logo.bmp
rm /tmp/logo-flat.png
```

`rsvg-convert` ships with `librsvg` (`brew install librsvg` on macOS);
`sips` is built into macOS.

## Uploading to Google Cloud

1. Open the [Google Auth Platform → Branding](https://console.cloud.google.com/auth/branding)
   page for the project.
2. Under **App logo**, click **Browse** and select `assets/oauth-logo/logo.png`.
3. Save. Uploading a logo requires the app to go through Google's
   verification process unless it's configured for internal use only or has
   a publishing status of "Testing".
