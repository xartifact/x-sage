import { expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import DetailPanel from './DetailPanel';
import type { GraphNode } from './types';

test('concept prose renders markdown while keeping embedded HTML inert', () => {
  const node: GraphNode = {
    id: 'untrusted-prose', atom: 'summary', layer: 2, domain: ['engineering'],
    confidence: 'hypothetical', source: 'personal', triggers: ['html'], filePath: '',
    molecule: '# Example\n\n**Bold**\n<script>alert(1)</script>\n<img src=x onerror=alert(1)>',
  };
  const html = renderToStaticMarkup(<DetailPanel node={node} edges={[]} allNodes={[node]} onClose={()=>{}}/>);
  expect(html).toContain('<strong>Bold</strong>');
  expect(html).not.toContain('<script>');
  expect(html).not.toContain('<img src=x');
  expect(html).toContain('&lt;script&gt;');
});
