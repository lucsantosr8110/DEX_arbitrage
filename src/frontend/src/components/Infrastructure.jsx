// Infrastructure — RPC health + runtime telemetry.

import { Badge, EmptyState, Kpi, Section } from "./primitives.jsx";

export function Infrastructure({ snapshot }) {
  return (
    <div className="infra-grid">
      <Section title="RPC providers" eyebrow="HEALTH">
        {snapshot.rpc.length ? (
          <div className="rpc-list">
            {snapshot.rpc.map((rpc) => (
              <div className="rpc-row" key={rpc.alias}>
                <span className={`status-dot ${rpc.status === "healthy" ? "live" : "warn"}`} />
                <div>
                  <strong>{rpc.alias}</strong>
                  <small>{rpc.failures > 0 ? `${rpc.failures} falhas acumuladas` : "sem falhas"}</small>
                </div>
                <b>{rpc.latency_ms == null ? "—" : `${rpc.latency_ms}ms`}</b>
                <Badge tone={rpc.status === "healthy" ? "green" : rpc.status === "cooldown" ? "red" : "amber"}>
                  {rpc.status}
                </Badge>
              </div>
            ))}
          </div>
        ) : <EmptyState message="A API ainda não expõe telemetria detalhada de RPC." />}
      </Section>
      <Section title="Runtime telemetry" eyebrow="PROCESS">
        <div className="telemetry">
          <Kpi label="Worker" value={snapshot.sequence > 0 ? "running" : "starting"} note={snapshot.runtime.phase} tone={snapshot.sequence > 0 ? "green" : "amber"} />
          <Kpi label="Uptime" value={`${snapshot.runtime.uptime_secs || 0}s`} note="desde startup" />
          <Kpi label="Prometheus" value="ver endpoint" note="não medido pela API" tone="cyan" />
          <Kpi label="Queue" value="—" note="não exposto pela API" />
        </div>
      </Section>
    </div>
  );
}