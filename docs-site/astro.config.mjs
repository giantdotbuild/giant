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
        { icon: 'github', label: 'GitHub', href: 'https://github.com/johnae/giant' },
      ],
      editLink: {
        baseUrl: 'https://github.com/johnae/giant/edit/main/docs-site/',
      },
      lastUpdated: true,
      sidebar: [
        {
          label: 'Get started',
          items: [
            { label: 'Quickstart', slug: 'start/quickstart' },
            { label: 'Install', slug: 'start/install' },
            { label: 'Your first build', slug: 'start/first-build' },
          ],
        },
        {
          label: 'Concepts',
          items: [
            { label: 'Targets and inputs', slug: 'concepts/targets' },
            { label: 'The cache key', slug: 'concepts/cache-key' },
            { label: 'Discovery', slug: 'concepts/discovery' },
            { label: 'Structural inputs', slug: 'concepts/structural-inputs' },
            { label: 'Selection language', slug: 'concepts/selection' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Go monorepo', slug: 'guides/go-monorepo' },
            { label: 'Docker images', slug: 'guides/docker' },
            { label: 'Tests with giant test', slug: 'guides/tests' },
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
