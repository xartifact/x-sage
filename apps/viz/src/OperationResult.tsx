export default function OperationResult({ value }: { value: unknown }) {
  if (!value || typeof value !== "object") return <p>{String(value)}</p>;
  const result = value as Record<string, unknown>;
  if (typeof result.diff === "string")
    return (
      <section>
        <h3>提案差异</h3>
        {result.applies === false && (
          <p role="alert">基线已变化，请重新读取并提交。</p>
        )}
        <pre className="operation-result">{result.diff || "正文没有变化"}</pre>
      </section>
    );
  if (typeof result.markdown === "string")
    return <pre className="operation-result">{result.markdown}</pre>;
  if (
    Array.isArray(value) &&
    value.every((row) => row && typeof row === "object" && "concept" in row)
  )
    return (
      <section>
        <h3>未决问题 · {value.length}</h3>
        {value.map((row, index) => (
          <article key={index}>
            <strong>{row.concept}</strong>
            <p>{row.reason}</p>
            <small>
              {row.source} · {row.priority}
            </small>
          </article>
        ))}
      </section>
    );
  if ("id" in result)
    return (
      <section role="status">
        <p>已创建概念：{String(result.id)}</p>
        {Boolean(result.warning) && <p>{String(result.warning)}</p>}
      </section>
    );
  if ("file" in result && !("proposal" in result))
    return (
      <p role="status">
        已保存待审提案：{String(result.file)}。请在“审阅提案”中查看并合并。
      </p>
    );
  if (result.ok === true) return <p role="status">保存成功</p>;
  if (typeof result.valid === "boolean")
    return (
      <section>
        <h3>{result.valid ? "校验通过" : "发现需要处理的问题"}</h3>
        <pre className="operation-result">{JSON.stringify(value, null, 2)}</pre>
      </section>
    );
  return (
    <pre className="operation-result">{JSON.stringify(value, null, 2)}</pre>
  );
}
