// Overview — KPIs hierárquicos + waterfall honesto + chart + alertas.
// Waterfall tem 3 linhas de % (tui_spread, cycle_rate, cycle_net) + o
// economic waterfall em USD (gross/gas/flashloan/net) quando disponível.
// Anomaly highlighting marca edge real onde cycle_rate > 0 mas cycle_net < 0.

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

const NEGATIVE_CAUSE_LABEL = {
  NO_GROSS_SPREAD: "sem spread bruto",
  GAS_DOMINATES: "gas domina",
  FLASHLOAN_FEE_DOMINATES: "fee do flashloan domina",
  NET_NON_POSITIVE_OTHER: "net negativo, causa não isolada",
};

// Economic waterfall em USD: gross (já pós-fee do AMM, não deduz fee de
// novo) → gas → flashloan → net. Componente ausente = "—", nunca $0.00
// fabricado (COMPONENT_UNAVAILABLE).
function EconomicWaterfallUsd({ route }) {
  const hasAny = route.gross_pnl_usd != null || route.gas_cost_usd != null || route.flashloan_cost_usd != null;
  if (!hasAny) return null;
  const rows = [
    ["Gross (pós-fee AMM)", route.gross_pnl_usd],
    ["Gas", route.gas_cost_usd != null ? -route.gas_cost_usd : null],
    ["Flashloan", route.flashloan_cost_usd != null ? -route.flashloan_cost_usd : null],
  ];
  const maxAbs = Math.max(0.01, ...rows.map(([, v]) => Math.abs(v ?? 0)), Math.abs(route.cycle_net_usd ?? 0));
  return (
    <div className="econ-waterfall">
      <div className="econ-waterfall-head">Economic waterfall (USD)</div>
      {rows.map(([label, value]) => {
        const widthPct = value == null ? 0 : Math.min(100, Math.max(4, (Math.abs(value) / maxAbs) * 100));
        return (
          <div className="econ-waterfall-row" key={label}>
            <span>{label}</span>
            <div className="econ-waterfall-track"><i className={value != null && value < 0 ? "neg" : ""} style={{ width: `${widthPct}%` }} /></div>
            <b title={value == null ? "Component unavailable" : undefined}>{value == null ? "—" : money(value)}</b>
          </div>
        );
      })}
      <div className="econ-waterfall-row net">
        <span>Net</span>
        <div className="econ-waterfall-track"><i style={{ width: `${Math.min(100, Math.max(4, Math.abs(route.cycle_net_usd ?? 0) / maxAbs * 100))}%` }} /></div>
        <b>{money(route.cycle_net_usd)}</b>
      </div>
      {route.negative_cause && route.negative_cause !== "POSITIVE" && (
        <div className="econ-waterfall-cause">
          PRIMARY_NEGATIVE_CAUSE: <b>{NEGATIVE_CAUSE_LABEL[route.negative_cause] || route.negative_cause}</b>
        </div>
      )}
    </div>
  );
}

export function Overview({ snapshot, bestRoute, rounds, stats }) {
  const r = snapshot.round;
  const bestNet = stats?.best_net_usd ?? bestRoute?.net ?? null;
  const histRoute = stats?.best_net_route_path ? {
    path: stats.best_net_route_path,
    venues: stats.best_net_route_venues || "—",
    // `best_net_route_gross` é o mesmo valor que `tui_spread_pct` (ver
    // persist_round: best_route_gross = route.tui_spread_pct); o campo
    // `best_net_route_tui_spread_pct` nunca existiu na API — pedir por
    // esse nome sempre voltava `undefined`, caindo no fallback `value || 0`
    // do WaterfallRow e mostrando "0.00%" mesmo com edge real (achado
    // 2026-08-17).
    tui_spread_pct: stats.best_net_route_gross,
    cycle_rate_pct: stats.best_net_route_cycle_rate_pct,
    cycle_net_usd: stats.best_net_route_net,
    gas_estimate_usd: stats.best_net_route_gas_estimate_usd,
    gross_pnl_usd: stats.best_net_route_gross_pnl_usd,
    gas_cost_usd: stats.best_net_route_gas_cost_usd,
    flashloan_cost_usd: stats.best_net_route_flashloan_cost_usd,
    negative_cause: stats.best_net_route_negative_cause,
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
        <div className="kpi-grid kpi-grid-5">
          <Kpi label="Rodadas totais" value={formatNumber(totalRounds)} note={stats ? "persistidas em SQLite" : "última sequência"} />
          <Kpi label="Net acumulado" value={money(stats?.sum_net_usd)} note={stats ? `soma de ${formatNumber(totalRounds)} rounds` : "sem histórico"} tone={stats?.sum_net_usd > 0 ? "green" : stats?.sum_net_usd < 0 ? "red" : "amber"} />
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
              <EconomicWaterfallUsd route={waterfall} />
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
            <Gate
              label="Economics consistent"
              value={snapshot.safety.economics_consistent == null
                ? "INCOMPLETE — sem round processado ainda"
                : snapshot.safety.economics_consistent
                  ? "PASS — discovery e projeção canônica batem"
                  : "FAIL — discovery_net_positive ≠ canonical_projection_net_positive"}
              good={snapshot.safety.economics_consistent === true}
            />
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