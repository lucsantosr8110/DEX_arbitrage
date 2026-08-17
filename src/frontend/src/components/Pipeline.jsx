// Pipeline — funil canônico + latency waterfall real por estágio + rejeições.
// Rejeições ainda sem agregação da API (EmptyState honesto).

import { EmptyState, Section } from "./primitives.jsx";
import { formatNumber } from "../lib/format.js";

// Estágios reais medidos em CanonicalRoundTiming (core::canonical_discovery).
// Ordem = ordem de execução no pipeline. "unattributed" não é um estágio de
// código — é total_ms - soma(estágios), sempre exibido por último.
const STAGES = [
  ["anchor_resolution", "Anchor resolution"],
  ["metadata", "Metadata"],
  ["quote", "Initial quotes"],
  ["ranking", "Route search + ranking"],
  ["requote", "Sequential requote"],
  ["context_build", "Context build"],
  ["materialization_economics", "Materialization + economics"],
  ["unattributed", "Unattributed"],
];

function LatencyWaterfall({ stats, lastRound }) {
  const l = stats?.latency;
  const hasLast = STAGES.some(([key]) => lastRound?.[`${key}_ms`] != null);
  if (!l?.sample_count && !hasLast) {
    return <EmptyState message="Sem amostras de latência ainda — aguardando rodadas persistidas." />;
  }
  const rows = STAGES.map(([key, label]) => ({
    key,
    label,
    last: lastRound?.[`${key}_ms`] ?? null,
    p50: l?.[`${key}_p50_ms`] ?? null,
    p95: l?.[`${key}_p95_ms`] ?? null,
  }));
  // Denominador honesto: duração real do round (medida fora do discovery,
  // cobre discovery+shadow+persist), não a soma dos estágios — evita que a
  // barra de "% do total" ultrapasse 100% quando shadow/persist dominam.
  const totalMs = lastRound?.duration_ms
    ?? rows.reduce((sum, row) => sum + (row.last ?? 0), 0)
    ?? 0;
  // Bottleneck só entre estágios reais de código — "unattributed" aponta
  // gap de instrumentação, não um estágio pra otimizar.
  const namedRows = rows.filter((row) => row.key !== "unattributed" && row.last != null);
  const bottleneck = namedRows.length
    ? namedRows.reduce((max, row) => (row.last > (max?.last ?? -1) ? row : max), null)
    : null;
  return (
    <>
      {bottleneck && totalMs > 0 && (
        <div className="latency-bottleneck">
          <strong>BOTTLENECK_STAGE={bottleneck.label}</strong>
          <span>{Math.round((bottleneck.last / totalMs) * 100)}% do round</span>
        </div>
      )}
      <div className="latency-waterfall">
        <div className="latency-waterfall-head">
          <span>Estágio</span><span>Último round</span><span>p50</span><span>p95</span><span>% do total</span>
        </div>
        {rows.map((row) => {
          const pct = totalMs > 0 && row.last != null ? Math.min(100, (row.last / totalMs) * 100) : 0;
          const isBottleneck = bottleneck?.key === row.key;
          return (
            <div className={`latency-waterfall-row ${isBottleneck ? "bottleneck" : ""}`} key={row.key}>
              <span>{row.label}</span>
              <span>{row.last == null ? "—" : `${formatNumber(row.last)}ms`}</span>
              <span>{row.p50 == null ? "—" : `${formatNumber(row.p50)}ms`}</span>
              <span>{row.p95 == null ? "—" : `${formatNumber(row.p95)}ms`}</span>
              <div className="latency-waterfall-bar"><i style={{ width: `${Math.max(row.last != null ? 2 : 0, pct)}%` }} /></div>
            </div>
          );
        })}
        <div className="latency-note">
          {l?.sample_count ? `percentis sobre ${l.sample_count} rodadas persistidas` : "sem percentis — aguardando amostras"}
          {" · "}
          duração total (denominador) = {totalMs > 0 ? `${formatNumber(totalMs)}ms` : "—"}
        </div>
      </div>
    </>
  );
}

export function Pipeline({ snapshot, stats, rounds }) {
  const lastRound = rounds?.[0];
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
      <Section title="Round latency waterfall" eyebrow="LATENCY · POR ESTÁGIO">
        <LatencyWaterfall stats={stats} lastRound={lastRound} />
      </Section>
      <Section title="Rejeições" eyebrow="WHY NOT SELECTED">
        <EmptyState message="A API ainda não expõe motivos agregados de rejeição." />
      </Section>
    </div>
  );
}
