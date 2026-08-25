import {themes as prismThemes} from 'prism-react-renderer';
import type {Config} from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';
import repoLinks from './src/remark/repo-links';

const config: Config = {
  title: 'sy',
  tagline: 'Agentic OS layer for Fedora',
  favicon: 'img/favicon.svg',

  url: 'https://dmytrogajewski.github.io',
  baseUrl: '/sy/',

  organizationName: 'dmytrogajewski',
  projectName: 'sy',
  trailingSlash: true,

  onBrokenLinks: 'throw',

  markdown: {
    format: 'md',
    hooks: {
      onBrokenMarkdownLinks: 'warn',
    },
  },

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  presets: [
    [
      'classic',
      {
        docs: {
          path: '../docs',
          routeBasePath: 'docs',
          sidebarPath: './sidebars.ts',
          exclude: ['syauth-setup.md'],
          numberPrefixParser: false,
          editUrl: ({docPath}) =>
            `https://github.com/dmytrogajewski/sy/edit/main/docs/${docPath}`,
          showLastUpdateTime: false,
          showLastUpdateAuthor: false,
          beforeDefaultRemarkPlugins: [repoLinks],
          remarkPlugins: [repoLinks],
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      } satisfies Preset.Options,
    ],
  ],

  themeConfig: {
    colorMode: {
      defaultMode: 'dark',
      disableSwitch: true,
      respectPrefersColorScheme: false,
    },
    docs: {
      sidebar: {
        hideable: false,
        autoCollapseCategories: false,
      },
    },
    navbar: {
      title: 'sy',
      logo: {
        alt: 'sy mascot — a tiny kitten',
        src: 'img/mascot.png',
      },
      items: [
        {
          type: 'docSidebar',
          sidebarId: 'docs',
          position: 'left',
          label: 'docs',
        },
        {
          to: '/docs/intro',
          label: 'story',
          position: 'left',
        },
        {
          to: '/docs/reference/cli',
          label: 'cli',
          position: 'left',
        },
        {
          href: 'https://github.com/dmytrogajewski/sy',
          label: 'github',
          position: 'right',
        },
      ],
    },
    footer: {
      style: 'dark',
      links: [
        {
          title: 'learn',
          items: [
            {label: 'start here', to: '/docs/intro'},
            {label: 'what sy is', to: '/docs/explanation/what-sy-is'},
            {label: 'bring-up', to: '/docs/tutorials/getting-started'},
            {label: 'search files', to: '/docs/tutorials/search-your-files'},
            {label: 'browse files', to: '/docs/tutorials/browse-your-files'},
          ],
        },
        {
          title: 'look up',
          items: [
            {label: 'cli', to: '/docs/reference/cli'},
            {label: 'glossary', to: '/docs/reference/glossary'},
            {label: 'config', to: '/docs/reference/config'},
          ],
        },
        {
          title: 'project',
          items: [
            {label: 'github', href: 'https://github.com/dmytrogajewski/sy'},
            {label: 'contributing', href: 'https://github.com/dmytrogajewski/sy/blob/main/CONTRIBUTING.md'},
            {label: 'security', href: 'https://github.com/dmytrogajewski/sy/blob/main/SECURITY.md'},
          ],
        },
      ],
      copyright: 'MIT · one binary, many planes · no snowflakes',
    },
    prism: {
      theme: prismThemes.vsDark,
      darkTheme: prismThemes.vsDark,
      additionalLanguages: ['bash', 'toml', 'json', 'rust', 'ini'],
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
