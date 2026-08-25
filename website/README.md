# sy documentation site

Docusaurus 3 build for the user-facing tree in `../docs/`.
Markdown stays in the repository root `docs/` directory so GitHub,
`llms.txt`, and this site share one source. The homepage and
`/docs/intro` tell the product story first; `/docs/explanation/what-sy-is`
is the longer why. Start there once the dev server is up.

Figures live in `../docs/img/` (SVG schematics). `static/img/diagrams`
is a symlink to that folder so the homepage can show the same stack
diagram.

## Preview

From the repository root:

```bash
cd website
npm install
npm start
```

The dev server listens on `http://localhost:3000/sy/`.

## Production build

```bash
make docs-site
```

or:

```bash
cd website && npm ci && npm run build
```

Output is `website/build/`. GitHub Pages is published from that
directory by `.github/workflows/docs-site.yml` on pushes to
`main`.

The workflow uploads a Pages artifact and deploys it. GitHub
does not enable Pages from the workflow file alone. Once per
repository, open **Settings → Pages → Build and deployment →
Source** and choose **GitHub Actions**. After that, the next
push to `main` that touches `docs/` or `website/` publishes
`https://<owner>.github.io/sy/`. Pull requests only build; they
do not deploy.

## Theme

Dark terminal palette (near-black, JetBrains Mono / IBM Plex Mono,
Gruvbox Material aqua accent) lives in `src/css/custom.css`. Colour
mode is locked to dark.

Do not add a `docs/` folder here. The docs plugin reads `../docs`.
