import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

const doc = (id: string, label: string) => ({type: 'doc' as const, id, label});

const sidebars: SidebarsConfig = {
  docs: [
    doc('intro', 'Start here'),
    {
      type: 'category',
      label: 'Tutorials',
      collapsed: false,
      items: [
        doc('tutorials/getting-started', 'Bring up a laptop'),
        doc('tutorials/search-your-files', 'Search your files'),
        doc('tutorials/browse-your-files', 'Browse files'),
        doc('tutorials/drive-sy-from-an-agent', 'Drive sy from an agent'),
        doc('tutorials/syauth-setup', 'Unlock sudo with your phone'),
      ],
    },
    {
      type: 'category',
      label: 'How-to',
      collapsed: false,
      items: [
        doc('how-to/set-up-npu', 'Set up the NPU'),
        doc('how-to/add-a-knowledge-source', 'Add a knowledge source'),
        doc('how-to/wire-mcp-into-agents', 'Wire MCP into agents'),
        doc('how-to/apply-a-theme', 'Apply a theme'),
        doc('how-to/run-doctor', 'Run sy doctor'),
        doc('how-to/install-spark', 'Install the Spark agent'),
        doc('how-to/serve-a-model-on-spark', 'Serve a model on Spark'),
        doc('how-to/run-sy-file', 'Run sy file from a shell'),
        doc('how-to/troubleshoot-sy-file', 'Troubleshoot sy file'),
        doc('how-to/write-a-sy-plugin', 'Write a sy plugin'),
        doc('how-to/troubleshoot-sy-plugin', 'Troubleshoot a sy plugin'),
        doc('how-to/troubleshoot-syauth', 'Troubleshoot syauth'),
      ],
    },
    {
      type: 'category',
      label: 'Reference',
      collapsed: false,
      items: [
        doc('reference/cli', 'CLI'),
        doc('reference/config', 'Configuration'),
        doc('reference/spark', 'Spark'),
        doc('reference/glossary', 'Glossary'),
        doc('reference/syauth-pam-module', 'syauth PAM module'),
        doc('reference/sy-file-doctor', 'sy file doctor'),
        doc('reference/sy-file-mcp', 'sy file MCP'),
      ],
    },
    {
      type: 'category',
      label: 'Explanation',
      collapsed: false,
      items: [
        doc('explanation/what-sy-is', 'What sy is'),
        doc('explanation/architecture', 'How the planes fit together'),
        doc('explanation/no-snowflakes', 'Why there are no snowflakes'),
        doc('explanation/agent-first-cli', 'Why the CLI is agent-first'),
        doc('explanation/why-npu-not-gpu', 'Why NPU, not GPU'),
      ],
    },
    {
      type: 'category',
      label: 'Decisions',
      items: [
        doc('adr/0001-use-adrs', 'ADR 0001 — Use ADRs'),
        doc(
          'adr/0002-virtual-workspace-with-sy-core-vocabulary',
          'ADR 0002 — Workspace split',
        ),
        doc(
          'adr/0003-vitisai-ep-not-cuda-for-on-device-embedding',
          'ADR 0003 — VitisAI, not CUDA',
        ),
        doc('adr/0004-publish-policy', 'ADR 0004 — publish = false'),
      ],
    },
    {
      type: 'category',
      label: 'Agents and admin',
      items: [
        doc('agents/mon-schema', 'mon snapshot schema'),
        doc('admin/mon-remote', 'Remote scrape of mon'),
        doc('compliance/openssf-best-practices', 'OpenSSF best practices'),
      ],
    },
  ],
};

export default sidebars;
