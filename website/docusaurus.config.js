export default {
  title: 'CenDB',
  tagline: 'Multi-model embedded database engine built in safe Rust',
  url: 'https://DavidAtanoff.github.io',
  baseUrl: '/CenDB/',
  onBrokenLinks: 'throw',
  onBrokenMarkdownLinks: 'warn',
  favicon: 'img/favicon.ico',
  organizationName: 'DavidAtanoff',
  projectName: 'CenDB',
  presets: [
    [
      'classic',
      {
        docs: {
          routeBasePath: '/',
          sidebarPath: './sidebars.js',
          editUrl: 'https://github.com/DavidAtanoff/CenDB/edit/main/website/docs',
        },
        theme: {
          customCss: './src/css/custom.css',
        },
      },
    ],
  ],
  themeConfig: {
    colorMode: {
      defaultMode: 'dark',
      disableSwitch: false,
      respectPrefersColorScheme: true,
    },
    navbar: {
      title: 'CenDB',
      logo: {
        alt: 'CenDB Logo',
        src: 'img/logo.svg',
      },
      items: [
        {
          type: 'doc',
          docId: 'introduction',
          position: 'left',
          label: 'Docs',
        },
        {
          href: 'https://github.com/DavidAtanoff/CenDB',
          label: 'GitHub',
          position: 'right',
        },
      ],
    },
    footer: {
      style: 'dark',
      links: [
        {
          title: 'Docs',
          items: [
            { label: 'Introduction', to: '/' },
            { label: 'Architecture', to: '/architecture' },
            { label: 'Benchmarks', to: '/benchmarks' },
            { label: 'Security', to: '/security' },
          ],
        },
        {
          title: 'Community',
          items: [
            { label: 'GitHub', href: 'https://github.com/DavidAtanoff/CenDB' },
            { label: 'Issues', href: 'https://github.com/DavidAtanoff/CenDB/issues' },
          ],
        },
      ],
      copyright: `Copyright © 2024-2026 David Atanoff. Built with Docusaurus.`,
    },
    prism: {
      theme: require('prism-react-renderer').themes.github,
      darkTheme: require('prism-react-renderer').themes.dracula,
      additionalLanguages: ['rust', 'toml', 'bash'],
    },
  },
};
