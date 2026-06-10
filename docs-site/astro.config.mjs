// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// https://astro.build/config
export default defineConfig({
  site: 'https://giant.build',
  vite: {
    // Vite 5+ requires every Host header to be allowlisted. With `--host`
    // we bind on all interfaces, so requests can arrive as `neptune`,
    // `*.ts.net`, raw IPs, etc. `true` allows everything - fine for a
    // local docs preview, not what you'd want on a public deployment.
    preview: { allowedHosts: true },
    server: { allowedHosts: true },
  },
  integrations: [
    starlight({
      title: 'Giant',
      description: 'A build orchestration tool with content-addressed caching for monorepos.',
      logo: {
        src: './src/assets/giant-mark.svg',
        replacesTitle: false,
      },
      customCss: ['./src/styles/landing.css'],
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/giantdotbuild/giant' },
      ],
      editLink: {
        baseUrl: 'https://github.com/giantdotbuild/giant/edit/main/docs-site/',
      },
      lastUpdated: true,
      sidebar: [
        {
          label: 'Get started',
          items: [
            { label: 'Quickstart', slug: 'start/quickstart' },
            { label: 'Install', slug: 'start/install' },
            { label: 'Your first build', slug: 'start/first-build' },
            { label: 'How Giant compares', slug: 'compare' },
          ],
        },
        {
          label: 'Concepts',
          items: [
            { label: 'Targets and inputs', slug: 'concepts/targets' },
            { label: 'Packages and labels', slug: 'concepts/packages' },
            { label: 'The cache key', slug: 'concepts/cache-key' },
            { label: 'Selection language', slug: 'concepts/selection' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Controlling Giant (NDJSON)', slug: 'guides/controlling-giant' },
            { label: 'Generating config', slug: 'guides/generating-config' },
            { label: 'Docker images', slug: 'guides/docker' },
            { label: 'Tests with giant test', slug: 'guides/tests' },
            { label: 'Pinning toolchains', slug: 'guides/toolchains' },
            { label: 'Watch mode', slug: 'guides/watch' },
            { label: 'CI integration', slug: 'guides/ci' },
            { label: 'Remote cache', slug: 'guides/remote-cache' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI', slug: 'reference/cli' },
            { label: 'giant.yaml', slug: 'reference/config' },
            { label: 'Starlark host', slug: 'reference/starlark' },
            { label: 'Event protocol (NDJSON)', slug: 'reference/events' },
            { label: 'Cache layout', slug: 'reference/cache-layout' },
          ],
        },
        {
          label: 'Extending Giant',
          items: [
            { label: 'giant-task (task runner)', slug: 'extending/giant-task' },
            { label: 'giant-tui (interactive browser)', slug: 'extending/giant-tui' },
            { label: 'Porcelains', slug: 'extending/porcelains' },
            { label: 'Architecture', slug: 'extending/architecture' },
          ],
        },
      ],
    }),
  ],
});
