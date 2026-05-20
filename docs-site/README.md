The giant.build site - landing page + docs.

Built with [Astro Starlight](https://starlight.astro.build). Deployed
to GitHub Pages by `.github/workflows/deploy-docs.yml` on every push to
`main` that touches `docs-site/`.

## Local dev

```bash
# Inside the devenv shell (Node is enabled in devenv.nix):
cd docs-site
npm install     # one time
npx astro dev   # http://localhost:4321
```

## Build

```bash
npx astro build         # → dist/
npx astro preview       # serve the built site
```

## Structure

```
docs-site/
├── astro.config.mjs        # site config + sidebar
├── public/                 # static assets served as-is
│   └── install.sh          # served at https://giant.build/install.sh
├── src/
│   ├── assets/             # images optimized at build time
│   ├── styles/landing.css  # landing-page styles (scoped via .landing)
│   └── content/docs/       # all the pages
│       ├── index.mdx       # landing page (template: splash)
│       ├── start/
│       ├── concepts/
│       ├── guides/
│       ├── reference/
│       └── extending/
```

## Editing

- **Landing page**: `src/content/docs/index.mdx` plus
  `src/styles/landing.css`. Uses `template: splash` to hide the sidebar.
- **Docs pages**: plain Markdown in `src/content/docs/<section>/`.
  Frontmatter `title:` and `description:` are required.
- **Sidebar**: edit `astro.config.mjs` to reorder or add sections.
- **Search**: Pagefind builds automatically at site build time.

## Conventions

- Page titles are sentence case ("Cache layout", not "Cache Layout").
- Code blocks always specify a language. Prefer `console` for shell
  examples (gets prompt highlighting).
- Internal links use the `/<section>/<slug>/` form (with leading
  slash, no `.md`).
- Don't add a top-level H1 inside the body - the frontmatter `title:`
  provides it.
