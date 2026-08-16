// Routes — tabela de rotas com colunas novas (cycle_rate, tui_spread, gas,
// slippage), filtros persistidos em localStorage, e anomaly highlighting
// em rows onde math não fecha (cycle_rate>0 → cycle_net<0).

import { useEffect, useState } from "react";
import { Badge, EmptyState, Section } from "./primitives.jsx";
import { money, pct } from "../lib/format.js";

const FILTERS_KEY = "argus.routes.filters.v1";

function loadFilters() {
  try {
    const raw = localStorage.getItem(FILTERS_KEY);
    return raw ? JSON.parse(raw) : {};
  } catch { return {}; }
}

function saveFilters(value) {
  try { localStorage.setItem(FILTERS_KEY, JSON.stringify(value)); } catch {}
}

// Anomaly usado também em Overview — duplicar aqui para evitar import circular.
function anomalyKind(route) {
  if (route.cycle_rate_pct > 0 && route.cycle_net_usd < 0) return "gas-dominates";
  if (route.tui_spread_pct > 1.5 && route.cycle_rate_pct < 0.1) return "forward-only";
  return null;
}

export function Routes({ routes }) {
  const [filters, setFilters] = useState(() => loadFilters());
  useEffect(() => { saveFilters(filters); }, [filters]);

  const venuePair = filters.venuePair || "all";
  const onlyPositive = !!filters.onlyPositive;
  const onlyExecutable = !!filters.onlyExecutable;

  const venuePairs = Array.from(new Set(routes.map((r) => r.venues))).sort();

  const filtered = routes.filter((route) => {
    if (onlyExecutable && !route.executable) return false;
    if (onlyPositive && !(route.net > 0)) return false;
    if (venuePair !== "all" && route.venues !== venuePair) return false;
    return true;
  });

  return (
    <Section
      title="Rotas avaliadas"
      eyebrow="ROUTE RANKING"
      action={
        <div className="filters">
          <select
            className="filter"
            value={venuePair}
            onChange={(e) => setFilters((f) => ({ ...f, venuePair: e.target.value }))}
            aria-label="Filtrar por par de venues"
          >
            <option value="all">todos venues ({venuePairs.length})</option>
            {venuePairs.map((vp) => <option key={vp} value={vp}>{vp}</option>)}
          </select>
          <button
            className={`filter ${onlyPositive ? "active" : ""}`}
            onClick={() => setFilters((f) => ({ ...f, onlyPositive: !onlyPositive }))}
          >
            só net &gt; 0
          </button>
          <button
            className={`filter ${onlyExecutable ? "active" : ""}`}
            onClick={() => setFilters((f) => ({ ...f, onlyExecutable: !onlyExecutable }))}
          >
            só executáveis
          </button>
        </div>
      }
    >
      {filtered.length ? (
        <div className="table-wrap">
          <table className="routes-table">
            <thead>
              <tr>
                <th>Rota</th>
                <th>Tipo</th>
                <th>TUI spread</th>
                <th>Cycle rate</th>
                <th>Net</th>
                <th>Gas est.</th>
                <th>Slippage</th>
                <th>Distância</th>
                <th>Estado</th>
              </tr>
            </thead>
            <tbody>
              {filtered.map((route) => {
                const a = anomalyKind(route);
                return (
                  <tr key={route.id} className={a ? `row-anomaly-${a}` : ""}>
                    <td>
                      <strong>{route.path}</strong>
                      <small>{route.id} · {route.venues}</small>
                    </td>
                    <td>
                      <Badge tone={route.route_kind === "triangular" ? "cyan" : "outline"}>
                        {route.route_kind === "triangular" ? "3L" : "2L"}
                      </Badge>
                    </td>
                    <td className="mono">{pct(route.tui_spread_pct ?? route.gross)}</td>
                    <td className={`mono ${route.cycle_rate_pct > 0 ? "text-amber" : "muted"}`}>
                      {pct(route.cycle_rate_pct ?? 0)}
                    </td>
                    <td className={`mono ${route.net > 0 ? "text-green" : "text-red"}`}>{money(route.net)}</td>
                    <td className="mono muted">{money(route.gas_estimate_usd)}</td>
                    <td className="mono muted">{route.slippage_budget_bps != null ? `${route.slippage_budget_bps}bps` : "—"}</td>
                    <td className="mono">{money(route.distance)}</td>
                    <td>
                      <Badge tone={route.executable ? "green" : "amber"}>{route.status}</Badge>
                      {route.reason && <small className="reason">{route.reason}</small>}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
          <div className="table-foot">
            <span>{filtered.length} de {routes.length} rotas</span>
            <span>filtros salvos localmente</span>
          </div>
        </div>
      ) : <EmptyState message="Nenhuma rota real observada ainda." />}
    </Section>
  );
}