# omni-dev website

Source for [omni-dev.john-ky.io](https://omni-dev.john-ky.io), built with
[Zola](https://www.getzola.org/) and deployed to GitHub Pages by
`.github/workflows/website.yml`.

## Local preview

```bash
brew install zola      # or: nix shell nixpkgs#zola
cd website
zola serve
```

Open the URL it prints (default `http://127.0.0.1:1111`).

## Structure

- `config.toml` — site config (base URL, theme, extra links)
- `content/` — markdown pages
- `templates/` — Tera HTML templates
- `sass/` — styles compiled to `style.css`
- `static/` — copied verbatim into the build, including `CNAME`
