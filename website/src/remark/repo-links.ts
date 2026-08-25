import {visit} from 'unist-util-visit';
import type {Plugin} from 'unified';

const GITHUB = 'https://github.com/dmytrogajewski/sy';

/**
 * Rewrite Markdown links that escape the docs/ tree so the Docusaurus
 * build can resolve them. Source files keep their repo-relative paths
 * (those still work on GitHub); only the site graph is rewritten.
 */
const repoLinks: Plugin = () => {
  return (tree) => {
    visit(tree, (node: {url?: string}) => {
      if (typeof node.url !== 'string') {
        return;
      }
      rewriteUrl(node);
    });
  };
};

function rewriteUrl(node: {url?: string}): void {
  const url = node.url ?? '';
  if (url === '../tutorials/' || url === '../tutorials') {
    node.url = '/docs/tutorials/getting-started';
    return;
  }
  if (url === '../how-to/' || url === '../how-to') {
    node.url = '/docs/how-to/add-a-knowledge-source';
    return;
  }
  if (!url.startsWith('../../')) {
    return;
  }
  const rest = url.slice('../../'.length);
  if (rest.startsWith('docs/')) {
    return;
  }
  const hashIndex = rest.indexOf('#');
  const path = hashIndex >= 0 ? rest.slice(0, hashIndex) : rest;
  const hash = hashIndex >= 0 ? rest.slice(hashIndex) : '';
  const isDir = path.endsWith('/');
  const kind = isDir ? 'tree' : 'blob';
  const clean = path.replace(/\/$/, '');
  node.url = `${GITHUB}/${kind}/main/${clean}${hash}`;
}

export default repoLinks;
