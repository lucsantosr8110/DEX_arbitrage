// Pipeline — funil canônico + latência por estágio + rejeições.
// Rejeições ainda sem agregação da API (EmptyState honesto).

import { EmptyState, Section } from "./primitives.jsx";
import { formatNumber } from "../lib/format.js";

function LatencyStrip({ stats }) {
  const l = stats?.latency;
  if (!l || !l.sample_count) return <EmptyState message="Sem amostras de latência ainda — aguardando rodadas persistidas." />;
  const rows = [
    ["Duração do round", l.duration_p50_ms, l.duration_p95_ms],
    ["Discovery (quotes + ciclos)", l.discovery_p50_ms, l.discovery_p95_ms],
    ["Shadow (re-quote + simulação)", l.shadow_p50_ms, l.shadow_p95_ms],
  ];
  return (
    <div className="latency-grid">
      {rows.map(([label, p50, p95]) => (
        <div key={label}>
          <small>{label}</small>
          <strong>{p50 == null ? "—" : `${p50}ms`}<span>p50</span></strong>
          <strong>{p95 == null ? "—" : `${p95}ms`}<span>p95</span></strong>
        </div>
      ))}
      <div className="latency-note">percentis sobre {l.sample_count} rodadas persistidas</div>
    </div>
  );
}

export function Pipeline({ snapshot, stats }) {
  const steps = [
    ["Quotes", snapshot.round.quotes],
    ["Edges", snapshot.round.edges],
    ["Ciclos detectados", snapshot.round.cycles_detected],
    ["Top ranked", snapshot.round.routes_ranked],
    ["Re-quote / evaluated", snapshot.round.routes_evaluated],
    ["Econ. / stable", snapshot.round.economically_positive],
    ["Risk approved", snapshot.round.risk_approved],
  ];
  return (
    <div className="pipeline-layout">
      <Section title="Funil canônico" eyebrow={`ROUND #${snapshot.sequence}`}>
        <div className="funnel">
          {steps.map(([label, value], i) => (
            <div className="funnel-row" key={label}>
              <span className="funnel-index">0{i + 1}</span>
              <div className="funnel-name">
                <strong>{label}</strong>
                <small>{value == null ? "sem medição" : "snapshot real"}</small>
              </div>
              <b>{formatNumber(value)}</b>
              <div className="funnel-bar"><i style={{ width: value == null ? "5%" : `${Math.max(5, 100 - i * 12)}%` }} /></div>
            </div>
          ))}
        </div>
      </Section>
      <Section title="Latência por estágio" eyebrow="LATENCY">
        <LatencyStrip stats={stats} />
      </Section>
      <Section title="Rejeições" eyebrow="WHY NOT SELECTED">
        <EmptyState message="A API ainda não expõe motivos agregados de rejeição." />
      </Section>
    </div>
  );
}