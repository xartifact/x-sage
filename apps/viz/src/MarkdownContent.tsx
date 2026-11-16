import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

/** Shared GFM reader. Raw HTML stays text; URLs use react-markdown's safe default. */
export default function MarkdownContent({ content }: { content: string }) {
  return (
    <div className="min-w-0 max-w-full space-y-3 break-words text-sm leading-7 [&_h1]:text-lg [&_h2]:text-base [&_h3]:text-sm [&_h1]:font-semibold [&_h2]:font-semibold [&_h3]:font-semibold [&_h4]:font-medium [&_h5]:font-medium [&_h6]:font-medium [&_ul]:list-disc [&_ul]:pl-5 [&_ol]:list-decimal [&_ol]:pl-5 [&_li>ul]:mt-1 [&_li>ol]:mt-1 [&_.contains-task-list]:list-none [&_.contains-task-list]:pl-0 [&_input]:mr-2 [&_blockquote]:border-l-2 [&_blockquote]:border-primary/40 [&_blockquote]:pl-4 [&_blockquote]:text-muted-foreground [&_code]:rounded [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:font-mono [&_code]:text-xs [&_pre]:max-w-full [&_pre]:overflow-x-auto [&_pre]:rounded-lg [&_pre]:bg-muted [&_pre]:p-3 [&_pre]:leading-6 [&_pre_code]:p-0 [&_img]:max-w-full [&_img]:rounded-lg [&_hr]:border-border">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          table: ({ children }) => (
            <div role="region" aria-label="Markdown 表格（可横向滚动）" tabIndex={0} className="max-w-full overflow-x-auto rounded-lg border focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
              <table className="w-full border-collapse text-left text-xs leading-6 [&_th]:min-w-28 [&_th]:border-b [&_th]:bg-muted [&_th]:px-3 [&_th]:py-2 [&_th]:font-semibold [&_td]:min-w-28 [&_td]:border-b [&_td]:px-3 [&_td]:py-2 [&_td]:align-top [&_tbody_tr:last-child_td]:border-b-0 [&_tbody_tr:hover]:bg-muted/40">{children}</table>
            </div>
          ),
          a: ({ children, href, title }) => (
            <a href={href} title={title} target={href ? "_blank" : undefined} rel="noopener noreferrer" className="text-primary underline underline-offset-4 hover:opacity-80">{children}</a>
          ),
        }}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
}

export function MarkdownPreview({ content }: { content: string }) {
  if (!content.trim()) return null;
  return (
    <details className="min-w-0 rounded-lg border p-3">
      <summary className="cursor-pointer text-sm font-medium">查看 Markdown 渲染预览</summary>
      <div className="mt-3"><MarkdownContent content={content} /></div>
    </details>
  );
}
