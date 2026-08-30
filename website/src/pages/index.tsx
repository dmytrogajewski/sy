import Link from '@docusaurus/Link';
import useBaseUrl from '@docusaurus/useBaseUrl';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import Layout from '@theme/Layout';
import clsx from 'clsx';
import styles from './index.module.css';

const FEATURES = [
  {
    href: '/docs/explanation/what-sy-is',
    name: 'one apply',
    desc: 'The desktop, units, and agent plumbing come from git. A fresh Fedora box can be the same machine.',
  },
  {
    href: '/docs/tutorials/search-your-files',
    name: 'search your files',
    desc: 'Register a folder. Index it. Search from the terminal or from Claude, Cursor, Codex, Gemini.',
  },
  {
    href: '/docs/tutorials/browse-your-files',
    name: 'file manager',
    desc: 'Super+E opens sy file. Markdown preview is a plugin, not Chrome.',
  },
  {
    href: '/docs/tutorials/drive-sy-from-an-agent',
    name: 'same CLI for agents',
    desc: 'JSON, dry-run, stable exit codes. MCP is wired by sy auto, not by hand-edited client configs.',
  },
  {
    href: '/docs/how-to/set-up-npu',
    name: 'optional silicon',
    desc: 'NPU embeddings, a Spark for models, phone-as-key sudo. Skip any of them. Bring-up does not require them.',
  },
];

export default function Home(): JSX.Element {
  const {siteConfig} = useDocusaurusContext();
  return (
    <Layout
      title={`${siteConfig.title} docs`}
      description="sy is an Agentic OS layer for Fedora: one binary, a git tree of configs, a workstation you and an agent can drive the same way.">
      <main className={styles.hero}>
        <div className="container">
          <div className={styles.frame}>
            <div className={styles.frameHeader}>
              <span className={styles.dot} />
              <span className={styles.dot} />
              <span className={styles.dot} />
              <span className={styles.title}>foot — sy@fedora:~</span>
            </div>
            <div className={styles.frameBody}>
              <p className={styles.lede}>
                {siteConfig.title} is an Agentic OS layer for Fedora 43.
                One Rust binary and a repo of configs turn a stock laptop
                into a workstation an agent can drive as well as you can.
              </p>
              <p className={styles.ledeDim}>
                Search your notes. Open a file manager. Approve sudo from
                the phone. Serve a model on a Spark if you have one. Rebuild
                the whole thing with
                cargo build and sy apply when the disk dies.
              </p>
              <div className={styles.line}>
                <span className={styles.prompt}>$</span>
                <span className={styles.cmd}>sy apply && systemctl --user enable --now sy.target</span>
              </div>
              <pre className={styles.output}>
{`# the laptop now matches this git checkout
# same commands work in a terminal and in an agent`}
              </pre>
              <div className={clsx(styles.actions)}>
                <Link className="button button--primary button--lg" to="/docs/intro">
                  the story
                </Link>
                <Link
                  className="button button--secondary button--lg"
                  to="/docs/explanation/what-sy-is">
                  what sy is
                </Link>
                <Link
                  className="button button--secondary button--lg"
                  to="/docs/tutorials/getting-started">
                  bring up a laptop
                </Link>
              </div>
            </div>
          </div>
          <img
            className={styles.schema}
            src={useBaseUrl('img/diagrams/sy-stack.svg')}
            alt="You and an agent both talk to one sy binary. sy sits on Fedora 43. NPU, Spark, and phone are optional."
          />
          <div className={styles.planes}>
            {FEATURES.map((feature) => (
              <Link key={feature.name} className={styles.plane} to={feature.href}>
                <span className={styles.planeName}>{feature.name}</span>
                <span className={styles.planeDesc}>{feature.desc}</span>
              </Link>
            ))}
          </div>
        </div>
      </main>
    </Layout>
  );
}
