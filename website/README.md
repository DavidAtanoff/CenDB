# CenDB Documentation Website

Built with [Docusaurus 3](https://docusaurus.io/). Deployed to GitHub Pages via GitHub Actions.

## Local development

```bash
cd website
npm install
npm start
```

Open http://localhost:3000/CenDB/ in your browser.

## Build

```bash
cd website
npm run build
```

Output goes to `website/build/`. Serve locally with `npm run serve`.

## Deploy

### GitHub Pages (automatic)

Push to `main` — the `.github/workflows/deploy-docs.yml` workflow builds and deploys automatically.

**Settings → Pages → Source:** GitHub Actions.

### Cloudflare Pages

```bash
# Build command: cd website && npm install && npm run build
# Build output directory: website/build
```

Or via Wrangler:

```bash
npx wrangler pages deploy website/build --project-name=cendb-docs
```

## Structure

```
website/
├── docusaurus.config.js   # Site config (theme, navbar, footer)
├── sidebars.js            # Sidebar navigation
├── package.json           # Dependencies
├── docs/                  # Markdown content
│   ├── introduction.md
│   ├── architecture.md
│   ├── benchmarks.md
│   ├── real-world-benchmarks.md
│   ├── security.md
│   ├── replication.md
│   ├── known-limitations.md
│   └── ...
├── src/
│   └── css/
│       └── custom.css     # Theme overrides (mint accent, dark mode)
└── static/
    └── img/               # Logos and static assets
```

## Theme

- **Dark mode default** with mint green accent (`#6ee7b7`).
- **Inter** for body text, **JetBrains Mono** for code.
- Prism syntax highlighting with Dracula theme for dark mode.
