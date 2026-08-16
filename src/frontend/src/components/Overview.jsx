// Overview — KPIs hierárquicos + waterfall honesto + chart + alertas.
// Waterfall tem 3 colunas: tui_spread (forward dispersion), cycle_rate
// (cycle fecha), cycle_net (projetado em $). Anomaly highlighting marca
// edge real onde cycle_rate > 0 mas cycle_net < 0.

import { Badge, EmptyState, Gate, Kpi, Section } from "./primitives.jsx";
import { formatDualTime, formatNumber, money, pct, timeAgo } from "../lib/format.js";

export function RoundChart({ rounds }) {
  if (!rounds.length) return <EmptyState message="Sem histórico persistido de rodadas." />;
  const ordered = rounds.slice().reverse().slice(-24);
  const maxAbs = Math.max(0.01, ...ordered.map((round) => Math.abs(round.net_usd_total || 0)));
  return (
    <div className="chart">
      <div className="bars">
        {ordered.map((round) => {
          const net = round.net_usd_total || 0;
          const tone = net > 0 ? "green" : net < 0 ? "red" : "neutral";
          const height = Math.max(2, (Math.abs(net) / maxAbs) * 100);
          const [, localTs] = formatDualTime(round.completed_at);
          return (
            <div
              className="bar-group"
              key={round.sequence}
              title={`#${round.sequence} · ${money(net)} · ${localTs || round.completed_at}`}
            >
              <i style={{ height: `${height}%`, background: tone === "green" ? "var(--green)" : tone === "red" ? "var(--red)" : "#377582" }} />
            </div>
          );
        })}
      </div>
      <div className="chart-labels">
        <span>últimas {ordered.length} rodadas · net USD por round</span>
        <span>máx {money(Math.max(0, ...ordered.map((round) => round.net_usd_total || 0)))}</span>
      </div>
      <div className="break-even"><span /><span>Break-even</span></div>
    </div>
  );
}

// Anomaly: edge real mas gas domina (cycle_rate>0 mas cycle_net<0),
// ou spread forward-only (tui_spread >> cycle_rate).
function routeAnomaly(route) {
  if (route.cycle_rate_pct > 0 && route.cycle_net_usd < 0) return "gas-dominates";
  if (route.tui_spread_pct > 1.5 && route.cycle_rate_pct < 0.1) return "forward-only";
  return null;
}

function WaterfallRow({ label, value, kind, anomaly }) {
  const widthPct = Math.min(100, Math.max(8, Math.abs(value || 0) * 90));
  return (
    <div className={`water-row ${kind} ${anomaly ? `anomaly-${anomaly}` : ""}`}>
      <span>{label}</span>
      <div className="water-track"><i style={{ width: `${widthPct}%` }} /></div>
      <b>{kind === "green" ? money(value) : pct(value || 0)}</b>
    </div>
  );
}

export function Overview({ snapshot, bestRoute, rounds, stats }) {
  const r = snapshot.round;
  const bestNet = stats?.best_net_usd ?? bestRoute?.net ?? null;
  const histRoute = stats?.best_net_route_path ? {
    path: stats.best_net_route_path,
    venues: stats.best_net_route_venues || "—",
    tui_spread_pct: stats.best_net_route_tui_spread_pct,
    cycle_rate_pct: stats.best_net_route_cycle_rate_pct,
    cycle_net_usd: stats.best_net_route_net,
    gas_estimate_usd: stats.best_net_route_gas_estimate_usd,
  } : null;
  const waterfall = histRoute ?? bestRoute ?? null;
  const anomaly = waterfall ? routeAnomaly(waterfall) : null;
  const totalRounds = stats?.total_rounds ?? snapshot.sequence;
  const lastRound = rounds[0];
  const lastRoundAge = lastRound ? timeAgo(lastRound.completed_at) : null;

  // KPI linha 1 — Agora
  const lastNet = lastRound?.net_usd_total ?? null;
  const lastSequence = lastRound?.sequence ?? snapshot.sequence;

  return (
    <>
      <div className="kpi-row">
        <div className="kpi-label">AGORA</div>
        <div className="kpi-grid">
          <Kpi label="Última round" value={`#${formatNumber(lastSequence)}`} note={lastRoundAge ? `concluída ${lastRoundAge}` : "snapshot inicial"} tone="cyan" />
          <Kpi label="Net último round" value={money(lastNet)} note={lastRound?.started_at ? `iniciada ${timeAgo(lastRound.started_at)}` : "—"} tone={lastNet > 0 ? "green" : lastNet < 0 ? "red" : "amber"} />
          <Kpi label="Rotas rankeadas" value={formatNumber(r.routes_ranked)} note={`${formatNumber(r.cycles_detected)} ciclos detectados`} />
          <Kpi label="Edge ativo" value={r.economically_positive != null ? `${formatNumber(r.economically_positive)} / ${formatNumber(r.gross_positive)}` : "—"} note="econ / gross positivos" tone="green" />
        </div>
      </div>

      <div className="kpi-row">
        <div className="kpi-label">PERFORMANCE</div>
        <div className="kpi-grid">
          <Kpi label="Rodadas totais" value={formatNumber(totalRounds)} note={stats ? "persistidas em SQLite" : "última sequência"} />
          <Kpi label="Melhor net" value={money(bestNet)} note={histRoute ? `melhor histórico · #${stats.best_net_sequence}` : "sem rota observada"} tone={bestNet > 0 ? "green" : "amber"} />
          <Kpi label="Latência p95" value={r.latency_p95_ms == null ? "—" : `${r.latency_p95_ms}ms`} note={r.latency_p50_ms == null ? "sem medição" : `p50 ${r.latency_p50_ms}ms · duração do round`} tone="cyan" />
          <Kpi label="Uptime" value={`${formatNumber(Math.floor((snapshot.runtime.uptime_secs || 0) / 60))}min`} note="desde startup" />
        </div>
      </div>

      <div className="overview-grid">
        <Section
          title="Economia da melhor rota"
          eyebrow="WATERFALL · 3 MÉTRICAS"
          action={histRoute ? <Badge tone="green">BEST EVER</Badge> : bestRoute ? <Badge tone="amber">OBSERVED</Badge> : null}
        >
          {waterfall ? (
            <>
              <div className="route-title">
                <strong>{waterfall.path}</strong>
                <span>{waterfall.venues}</span>
              </div>
              <div className="waterfall-legend">
                <span><i className="legend-dot cyan" />TUI spread (forward dispersion)</span>
                <span><i className="legend-dot amber" />Cycle rate (cycle fecha)</span>
                <span><i className="legend-dot green" />Cycle net (projetado)</span>
              </div>
              <div className="waterfall">
                <WaterfallRow label="TUI spread" value={waterfall.tui_spread_pct} kind="cyan" anomaly={anomaly === "forward-only" ? anomaly : null} />
                <WaterfallRow label="Cycle rate" value={waterfall.cycle_rate_pct} kind="amber" anomaly={null} />
                <WaterfallRow label="Cycle net" value={waterfall.cycle_net_usd} kind="green" anomaly={anomaly === "gas-dominates" ? anomaly : null} />
              </div>
              {anomaly === "gas-dominates" && (
                <div className="anomaly-note amber">
                  <strong>edge real, gas domina</strong>
                  <small>cycle_rate &gt; 0 mas cycle_net &lt; 0 — slippage/flashloan come o lucro</small>
                </div>
              )}
              {anomaly === "forward-only" && (
                <div className="anomaly-note cyan">
                  <strong>spread forward-only</strong>
                  <small>TUI dispersão alta, mas cycle não fecha — não é arbitragem</small>
                </div>
              )}
            </>
          ) : <EmptyState message="Nenhuma rota real observada ainda." />}
        </Section>

        <Section title="Estado dos gates" eyebrow="SAFETY GATES">
          <div className="gates">
            <Gate label="Economics consistent" value={snapshot.safety.economics_consistent == null ? "sem evidência" : snapshot.safety.economics_consistent ? "confirmado" : "inconsistente"} good={snapshot.safety.economics_consistent === true} />
            <Gate label="Simulate before execute" value="obrigatório" good />
            <Gate label="Signer" value="ausente · bloqueado" />
            <Gate label="Broadcaster" value="ausente · bloqueado" />
            <Gate label="Mainnet execution" value="bloqueado por política" />
          </div>
        </Section>
      </div>

      <div className="bottom-grid">
        <Section title="Atividade de 1 hora" eyebrow="ROUND PERFORMANCE">
          <RoundChart rounds={rounds} />
        </Section>
        <Section title="Alertas recentes" eyebrow="ACTIONABLE">
          <div className="alerts">
            {snapshot.alerts.length ? snapshot.alerts.map((alert) => (
              <div className="alert" key={alert.title}>
                <span className={`alert-icon ${alert.severity}`}>{alert.severity === "warning" ? "!" : "i"}</span>
                <div>
                  <strong>{alert.title}</strong>
                  <small>{alert.detail}</small>
                </div>
              </div>
            )) : <EmptyState message="Nenhum alerta emitido pela API." />}
          </div>
        </Section>
      </div>
    </>
  );
}