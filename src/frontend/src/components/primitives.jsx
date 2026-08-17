// Primitives reutilizáveis: Badge, Section, Gate, Kpi, EmptyState.

export function Badge({ children, tone = "neutral" }) {
  return <span className={`badge badge-${tone}`}>{children}</span>;
}

export function Section({ title, eyebrow, action, children, className = "" }) {
  return (
    <section className={`panel ${className}`}>
      <div className="panel-heading">
        <div>
          <span className="eyebrow">{eyebrow}</span>
          <h2>{title}</h2>
        </div>
        {action}
      </div>
      {children}
    </section>
  );
}

export function Gate({ label, value, good = false }) {
  return (
    <div className="gate">
      <span className={`gate-dot ${good ? "good" : "blocked"}`} />
      <div>
        <strong>{label}</strong>
        <small>{value}</small>
      </div>
    </div>
  );
}

export function Kpi({ label, value, note, tone = "" }) {
  return (
    <div className="kpi">
      <span>{label}</span>
      <strong className={tone ? `text-${tone}` : ""}>{value}</strong>
      <small>{note}</small>
    </div>
  );
}

export function EmptyState({ message }) {
  return <div className="empty-state">{message}</div>;
}