// App.jsx — composição enxuta após refactor. State + data hooks ficam aqui;
// UI é dividida em components/. Filtros de Routes são persistidos via
// localStorage (key em Routes.jsx).

import React, { useEffect, useMemo, useState } from "react";
import "./styles.css";
import { Badge } from "./components/primitives.jsx";
import { Overview } from "./components/Overview.jsx";
import { Market } from "./components/Market.jsx";
import { Routes } from "./components/Routes.jsx";
import { Pipeline } from "./components/Pipeline.jsx";
import { Infrastructure } from "./components/Infrastructure.jsx";
import { Safety } from "./components/Safety.jsx";
import { ageLabel, formatDualTime, saneGross, saneNet } from "./lib/format.js";

const emptySnapshot = {
  schema_version: "operator.v1", data_source: "tui_state", sequence: 0, generated_at: null,
  runtime: { mode: "PAPER", dry_run: true, phase: "Aguardando primeiro snapshot", uptime_secs: 0, shutdown_state: "armed" },
  safety: { signer_present: false, broadcaster_present: false, wrapper_enabled: false, simulate_before_execute: true, economics_consistent: null, mainnet_blocked: true },
  chain: { chain_id: 137, network: "Polygon", head_block: null, anchor_block: null, anchor_hash: null, confirmations: null, data_age_ms: null },
  round: { duration_ms: null, quotes: 0, edges: null, cycles_detected: 0, routes_ranked: 0, routes_evaluated: null, gross_positive: 0, economically_positive: 0, stable: null, risk_approved: null, selected: null, timeouts: null, latency_p50_ms: null, latency_p95_ms: null },
  prices: [], routes: [], rpc: [], alerts: [],
};

function normalizeSnapshot(data) {
  return {
    ...emptySnapshot,
    ...data,
    runtime: { ...emptySnapshot.runtime, ...(data.runtime || {}) },
    safety: { ...emptySnapshot.safety, ...(data.safety || {}) },
    chain: { ...emptySnapshot.chain, ...(data.chain || {}) },
    round: { ...emptySnapshot.round, ...(data.round || {}) },
    prices: data.prices || [], routes: data.routes || [], rpc: data.rpc || [], alerts: data.alerts || [],
  };
}

function useOperatorData() {
  const [snapshot, setSnapshot] = useState(emptySnapshot);
  const [connection, setConnection] = useState("idle");

  useEffect(() => {
    let source;
    const refresh = () => fetch("/api/v1/snapshot")
      .then((response) => { if (!response.ok) throw new Error("snapshot unavailable"); return response.json(); })
      .then((data) => { setSnapshot(normalizeSnapshot(data)); setConnection("live"); })
      .catch(() => setConnection("offline"));
    refresh();
    const poll = window.setInterval(refresh, 5000);
    try {
      source = new EventSource("/api/v1/events");
      source.addEventListener("snapshot", (event) => { setSnapshot(normalizeSnapshot(JSON.parse(event.data))); setConnection("live"); });
      source.onopen = () => setConnection("live");
      source.onerror = () => setConnection("reconnecting");
    } catch { setConnection("offline"); }
    return () => { source?.close(); window.clearInterval(poll); };
  }, []);
  return { snapshot, connection };
}

// Histórico persistido em SQLite (rodadas lentas ~5-15min → 30s poll).
function useHistory() {
  const [rounds, setRounds] = useState([]);
  const [stats, setStats] = useState(null);

  useEffect(() => {
    const refresh = () => {
      fetch("/api/v1/rounds?limit=24")
        .then((response) => { if (!response.ok) throw new Error("rounds unavailable"); return response.json(); })
        .then((data) => setRounds(data.rounds || []))
        .catch(() => {});
      fetch("/api/v1/stats")
        .then((response) => { if (!response.ok) throw new Error("stats unavailable"); return response.json(); })
        .then((data) => setStats(data.stats || null))
        .catch(() => {});
    };
    refresh();
    const poll = window.setInterval(refresh, 30000);
    return () => window.clearInterval(poll);
  }, []);
  return { rounds, stats };
}

function App() {
  const { snapshot, connection } = useOperatorData();
  const { rounds, stats } = useHistory();
  const [section, setSection] = useState("overview");
  const age = snapshot.chain?.data_age_ms;
  const routes = useMemo(
    () => (snapshot.routes || []).filter((route) => saneGross(route.gross) && saneNet(route.net)),
    [snapshot.routes]
  );
  const bestRoute = useMemo(
    () => [...(snapshot.routes || [])]
      .filter((route) => saneGross(route.gross) && saneNet(route.net))
      .sort((a, b) => (b.net ?? -Infinity) - (a.net ?? -Infinity))[0],
    [snapshot.routes]
  );
  const nav = [
    { id: "overview", label: "Visão geral" },
    { id: "market", label: "Mercado" },
    { id: "routes", label: "Rotas" },
    { id: "pipeline", label: "Pipeline" },
    { id: "infra", label: "Infraestrutura" },
    { id: "safety", label: "Segurança" },
  ];
  const [utcTs, localTs] = formatDualTime(snapshot.generated_at);

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <span className="brand-mark">⌁</span>
          <div><strong>ARGUS</strong><small>operator console</small></div>
        </div>
        <div className="workspace">
          <span className="eyebrow">WORKSPACE</span>
          <strong>Polygon / dry-run</strong>
          <small>canonical runtime</small>
        </div>
        <nav>
          {nav.map((item) => (
            <button key={item.id} className={section === item.id ? "active" : ""} onClick={() => setSection(item.id)}>
              <span className="nav-marker" />{item.label}
            </button>
          ))}
        </nav>
        <div className="sidebar-footer">
          <span className="pulse" />Read-only mode
          <div>schema {snapshot.schema_version}</div>
        </div>
      </aside>
      <main className="main">
        <header className="topbar">
          <div className="mobile-brand">ARGUS <span>operator console</span></div>
          <div className="topbar-status">
            <Badge tone="cyan">POLYGON</Badge>
            <Badge tone="amber">DRY RUN</Badge>
            <span className="status-item">
              <i className={`status-dot ${connection === "live" ? "live" : "warn"}`} />
              {connection === "live" ? "SSE connected" : connection === "reconnecting" ? "reconnecting" : connection === "offline" ? "API offline" : "aguardando API"}
            </span>
            <span className="status-item">anchor <b>{snapshot.chain?.anchor_block ?? "—"}</b></span>
            <span className="status-item">
              data age <b className={age > 10000 ? "text-amber" : ""}>{ageLabel(age)}</b>
            </span>
          </div>
        </header>
        <div className="content">
          <div className="page-intro">
            <div>
              <span className="eyebrow">OPERATOR / {section.toUpperCase()}</span>
              <h1>{nav.find((item) => item.id === section)?.label}</h1>
              <p>
                Snapshot autoritativo · sequência <span className="mono">#{snapshot.sequence}</span>
                {utcTs && localTs ? ` · ${utcTs} UTC / ${localTs} local` : " · sem evidência"}
              </p>
            </div>
            <div className="intro-actions">
              <Badge tone="outline">READ ONLY</Badge>
              <button className="icon-button" aria-label="Atualizar snapshot" onClick={() => window.location.reload()}>↻</button>
            </div>
          </div>
          {age > 10000 && (
            <div className="stale-banner">
              <span>!</span>
              <div>
                <strong>Dados potencialmente stale</strong>
                <small>O console preserva o último snapshot conhecido. Nenhuma decisão de execução é habilitada.</small>
              </div>
            </div>
          )}
          {section === "overview" && <Overview snapshot={snapshot} bestRoute={bestRoute} rounds={rounds} stats={stats} />}
          {section === "market" && <Market prices={snapshot.prices || []} />}
          {section === "routes" && <Routes routes={routes} />}
          {section === "pipeline" && <Pipeline snapshot={snapshot} stats={stats} rounds={rounds} />}
          {section === "infra" && <Infrastructure snapshot={snapshot} />}
          {section === "safety" && <Safety snapshot={snapshot} />}
        </div>
        <footer className="footer">
          <span>schema {snapshot.schema_version}</span>
          <span>·</span>
          <span>phase {snapshot.runtime.phase}</span>
          <span>·</span>
          <span>uptime {Math.floor((snapshot.runtime.uptime_secs || 0) / 60)}min</span>
        </footer>
      </main>
    </div>
  );
}

class ConsoleErrorBoundary extends React.Component {
  state = { hasError: false };
  static getDerivedStateFromError() { return { hasError: true }; }
  render() {
    if (this.state.hasError) {
      return (
        <div className="fatal-state">
          <strong>ARGUS não conseguiu montar o console</strong>
          <span>Atualize a página para tentar novamente.</span>
          <button onClick={() => window.location.reload()}>Atualizar</button>
        </div>
      );
    }
    return this.props.children;
  }
}

export default function ConsoleRoot() { return <ConsoleErrorBoundary><App /></ConsoleErrorBoundary>; }