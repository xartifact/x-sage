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

test('symmetric connections appear once and expose contextual actions', () => {
  const node: GraphNode = {
    id: 'first', atom: 'summary', layer: 2, domain: ['engineering'],
    confidence: 'hypothetical', source: 'personal', triggers: ['test'], filePath: '',
    molecule: '# First concept',
  };
  const other: GraphNode = {...node,id:'second',molecule:'# Second concept'};
  const html = renderToStaticMarkup(<DetailPanel node={node} allNodes={[node,other]} edges={[
    {from:'first',target:'second',type:'analogous-to',confidence:'hypothetical'},
    {from:'second',target:'first',type:'analogous-to',confidence:'hypothetical'},
  ]} onClose={()=>{}} onSelect={()=>{}} onEdit={()=>{}}/>);
  expect(html.match(/Second concept/g)?.length).toBe(1);
  expect(html).toContain('修订概念');
  expect(html).toContain('关联概念 · 1');
});
