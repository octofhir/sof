// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// Hosted on GitHub Pages at https://octofhir.github.io/sof/
export default defineConfig({
  site: 'https://octofhir.github.io',
  base: '/sof',
  compressHTML: true,
  integrations: [
    starlight({
      title: 'octofhir-sof',
      description:
        'A pure-Rust SQL on FHIR 3.0 engine: database-free in-memory evaluation, multi-dialect SQL generation, and SQLQuery jobs with terminology.',
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/octofhir/sof' },
      ],
      editLink: {
        baseUrl: 'https://github.com/octofhir/sof/edit/main/docs/',
      },
      sidebar: [
        {
          label: 'Start here',
          items: [
            { label: 'Introduction', slug: 'index' },
            { label: 'Install', slug: 'install' },
            { label: 'Quickstart', slug: 'quickstart' },
          ],
        },
        {
          label: 'Guides',
          items: [{ autogenerate: { directory: 'guides' } }],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI', slug: 'reference/cli' },
            { label: 'Library (crates)', slug: 'reference/library' },
          ],
        },
        {
          label: 'Lint rules',
          items: [{ autogenerate: { directory: 'rules', collapsed: true } }],
        },
      ],
    }),
  ],
});
