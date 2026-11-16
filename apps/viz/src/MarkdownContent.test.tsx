import { expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import MarkdownContent from "./MarkdownContent";
import OperationResult from "./OperationResult";

const table = '| 项目 | 数量 |\n| :--- | ---: |\n| **概念** | 12 |\n| a\\|b | 3 |';
test('GFM tables preserve alignment, inline formatting and escaped pipes inside a scroll region', () => {
  const html = renderToStaticMarkup(<MarkdownContent content={table} />);
  expect(html).toContain('<table');
  expect(html).toContain('<thead>');
  expect(html).toContain('<tbody>');
  expect(html).toContain('text-align:right');
  expect(html).toContain('<strong>概念</strong>');
  expect(html).toContain('a|b');
  expect(html).toContain('overflow-x-auto');
  expect(html).toContain('tabindex="0"');
});
test('GFM lists, quotes and fenced code retain their semantics', () => {
  const html = renderToStaticMarkup(<MarkdownContent content={'- [x] 完成\n- [ ] 待办\n\n1. 第一步\n\n> 引用\n\n~~删除~~\n\n```html\n<script>**literal**</script>\n```'} />);
  expect(html).toContain('type="checkbox"');
  expect(html).toContain('disabled=""');
  expect(html).toContain('<ol>');
  expect(html).toContain('<blockquote>');
  expect(html).toContain('<del>删除</del>');
  expect(html).toContain('&lt;script&gt;**literal**&lt;/script&gt;');
});
test('raw HTML and unsafe URLs remain inert while safe links work', () => {
  const html = renderToStaticMarkup(<MarkdownContent content={'<script>alert(1)</script>\n\n[unsafe](javascript:alert%281%29)\n\n[safe](https://example.com)'} />);
  expect(html).not.toContain('<script>');
  expect(html).toContain('&lt;script&gt;');
  expect(html).not.toContain('href="javascript:');
  expect(html).toContain('href="https://example.com"');
  expect(html).toContain('rel="noopener noreferrer"');
});
test('manifest results use the same table renderer', () => {
  const html = renderToStaticMarkup(<OperationResult value={{markdown: table}} />);
  expect(html).toContain('<table');
  expect(html).not.toContain('<pre');
});
