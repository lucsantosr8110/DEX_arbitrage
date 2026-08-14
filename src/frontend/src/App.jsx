import React, { useEffect, useMemo, useState } from "react";
import "./styles.css";

const emptySnapshot = {
  schema_version: "operator.v1", data_source: "tui_state", sequence: 0, generated_at: null,
  runtime: { mode: "PAPER", dry_run: true, phase: "Aguardando primeiro snapshot", uptime_secs: 0, shutdown_state: "armed" },
  safety: { signer_present: false, broadcaster_present: false, wrapper_enabled: false, simulate_before_execute: true, economics_consistent: null, mainnet_blocked: true },
  chain: { chain_id: 137, network: "Polygon", head_block: null, anchor_block: null, anchor_hash: null, confirmations: null, data_age_ms: null },
  round: { duration_ms: null, quotes: 0, edges: null, cycles_detected: 0, routes_ranked: 0, routes_evaluated: null, gross_positive: 0, economically_positive: 0, stable: null, risk_approved: null, selected: null, timeouts: null, latency_p50_ms: null, latency_p95_ms: null },
  prices: [], routes: [], rpc: [], alerts: [],
};

const money = (value) => value == null || !Number.isFinite(value) ? "—" : `${value < 0 ? "−" : ""}$${Math.abs(value).toFixed(2)}`;
const pct = (value) => `${value < 0 ? "−" : ""}${Math.abs(value).toFixed(2)}%`;
const ageLabel = (ms) => ms == null ? "—" : (ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`);
const formatPrice = (value) => value == null ? "—" : Number(value).toLocaleString("en-US", { maximumFractionDigits: 8 });
const formatNumber = (value) => value == null ? "—" : Number(value).toLocaleString("pt-BR");
const saneGross = (value) => Number.isFinite(value) && Math.abs(value) <= 50;
const saneNet = (value) => Number.isFinite(value) && Math.abs(value) <= 500;

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

// Histórico persistido em SQLite (rodadas passadas sobrevivem a restarts).
// Rounds são lentos (~5-15min), então poll de 30s é suficiente.
function useHistory() {
  const [rounds, setRounds] = useState([]);
  const [stats, setStats] = useState(null);
  const [historyEnabled, setHistoryEnabled] = useState(false);

  useEffect(() => {
    const refresh = () => {
      fetch("/api/v1/rounds?limit=24")
        .then((response) => { if (!response.ok) throw new Error("rounds unavailable"); return response.json(); })
        .then((data) => setRounds(data.rounds || []))
        .catch(() => {});
      fetch("/api/v1/stats")
        .then((response) => { if (!response.ok) throw new Error("stats unavailable"); return response.json(); })
        .then((data) => { setStats(data.stats || null); setHistoryEnabled(!!data.history_enabled); })
        .catch(() => {});
    };
    refresh();
    const poll = window.setInterval(refresh, 30000);
    return () => window.clearInterval(poll);
  }, []);
  return { rounds, stats, historyEnabled };
}

// Bar-chart CSS do net USD por rodada (sem lib — espelha estilo do console).
function RoundChart({ rounds }) {
  if (!rounds.length) return <EmptyState message="Sem histórico persistido de rodadas." />;
  const ordered = rounds.slice().reverse().slice(-24); // mais antigo → mais recente
  const maxAbs = Math.max(0.01, ...ordered.map((round) => Math.abs(round.net_usd_total || 0)));
  return <div className="chart"><div className="bars">{ordered.map((round) => {
    const net = round.net_usd_total || 0;
    const tone = net > 0 ? "green" : net < 0 ? "red" : "neutral";
    const height = Math.max(2, (Math.abs(net) / maxAbs) * 100);
    return <div className="bar-group" key={round.sequence} title={`#${round.sequence} ${money(net)} (${round.completed_at})`}><i style={{ height: `${height}%`, background: tone === "green" ? "var(--green)" : tone === "red" ? "var(--red)" : "#377582" }} /></div>;
  })}</div><div className="chart-labels"><span>últimas {ordered.length} rodadas · net USD por round</span><span>máx {money(Math.max(0, ...ordered.map((round) => round.net_usd_total || 0)))}</span></div><div className="break-even"><span /><span>Break-even</span></div></div>;
}

function Badge({ children, tone = "neutral" }) { return <span className={`badge badge-${tone}`}>{children}</span>; }
function Section({ title, eyebrow, action, children, className = "" }) { return <section className={`panel ${className}`}><div className="panel-heading"><div><span className="eyebrow">{eyebrow}</span><h2>{title}</h2></div>{action}</div>{children}</section>; }
function Gate({ label, value, good = false }) { return <div className="gate"><span className={`gate-dot ${good ? "good" : "blocked"}`} /><div><strong>{label}</strong><small>{value}</small></div></div>; }

function App() {
  const { snapshot, connection } = useOperatorData();
  const { rounds, stats } = useHistory();
  const [section, setSection] = useState("overview");
  const [onlyAuthoritative, setOnlyAuthoritative] = useState(false);
  const age = snapshot.chain?.data_age_ms;
  const routes = useMemo(() => (snapshot.routes || []).filter((route) => saneGross(route.gross) && saneNet(route.net) && (!onlyAuthoritative || route.authoritative)), [snapshot.routes, onlyAuthoritative]);
  const bestRoute = [...(snapshot.routes || [])].filter((route) => saneGross(route.gross) && saneNet(route.net)).sort((a, b) => (b.net ?? -Infinity) - (a.net ?? -Infinity))[0];
  const nav = [{ id: "overview", label: "Visão geral" }, { id: "market", label: "Mercado" }, { id: "routes", label: "Rotas" }, { id: "pipeline", label: "Pipeline" }, { id: "infra", label: "Infraestrutura" }, { id: "safety", label: "Segurança" }];

  return <div className="app-shell">
    <aside className="sidebar"><div className="brand"><span className="brand-mark">⌁</span><div><strong>ARGUS</strong><small>operator console</small></div></div><div className="workspace"><span className="eyebrow">WORKSPACE</span><strong>Polygon / dry-run</strong><small>canonical runtime</small></div><nav>{nav.map((item) => <button key={item.id} className={section === item.id ? "active" : ""} onClick={() => setSection(item.id)}><span className="nav-marker" />{item.label}</button>)}</nav><div className="sidebar-footer"><span className="pulse" />Read-only mode<div>schema {snapshot.schema_version}</div></div></aside>
    <main className="main"><header className="topbar"><div className="mobile-brand">ARGUS <span>operator console</span></div><div className="topbar-status"><Badge tone="cyan">POLYGON</Badge><Badge tone="amber">DRY RUN</Badge><span className="status-item"><i className={`status-dot ${connection === "live" ? "live" : "warn"}`} />{connection === "live" ? "SSE connected" : connection === "reconnecting" ? "reconnecting" : connection === "offline" ? "API offline" : "aguardando API"}</span><span className="status-item">anchor <b>{snapshot.chain?.anchor_block ?? "—"}</b></span><span className="status-item">data age <b className={age > 10000 ? "text-amber" : ""}>{ageLabel(age)}</b></span></div></header>
      <div className="content"><div className="page-intro"><div><span className="eyebrow">OPERATOR / {section.toUpperCase()}</span><h1>{nav.find((item) => item.id === section)?.label}</h1><p>Snapshot autoritativo · sequência <span className="mono">#{snapshot.sequence}</span> · {snapshot.generated_at ? new Date(snapshot.generated_at).toLocaleTimeString("pt-BR") : "sem evidência"}</p></div><div className="intro-actions"><Badge tone="outline">READ ONLY</Badge><button className="icon-button" aria-label="Atualizar snapshot" onClick={() => window.location.reload()}>↻</button></div></div>
      {age > 10000 && <div className="stale-banner"><span>!</span><div><strong>Dados potencialmente stale</strong><small>O console preserva o último snapshot conhecido. Nenhuma decisão de execução é habilitada.</small></div></div>}
      {section === "overview" && <Overview snapshot={snapshot} bestRoute={bestRoute} rounds={rounds} stats={stats} />}
      {section === "market" && <Market prices={snapshot.prices || []} />}
      {section === "routes" && <Routes routes={routes} onlyAuthoritative={onlyAuthoritative} setOnlyAuthoritative={setOnlyAuthoritative} />}
      {section === "pipeline" && <Pipeline snapshot={snapshot} />}
      {section === "infra" && <Infrastructure snapshot={snapshot} />}
      {section === "safety" && <Safety snapshot={snapshot} />}
      </div></main>
  </div>;
}

class ConsoleErrorBoundary extends React.Component {
  state = { hasError: false };

  static getDerivedStateFromError() { return { hasError: true }; }

  render() {
    if (this.state.hasError) {
      return <div className="fatal-state"><strong>ARGUS não conseguiu montar o console</strong><span>Atualize a página para tentar novamente.</span><button onClick={() => window.location.reload()}>Atualizar</button></div>;
    }
    return this.props.children;
  }
}

function Overview({ snapshot, bestRoute, rounds, stats }) {
  const r = snapshot.round;
  const bestNet = stats?.best_net_usd ?? bestRoute?.net ?? null;
  const histRoute = stats?.best_net_route_path ? {
    path: stats.best_net_route_path, venues: stats.best_net_route_venues || "—",
    gross: stats.best_net_route_gross, net: stats.best_net_route_net, authoritative: false,
  } : null;
  const waterfall = histRoute ?? bestRoute ?? null;
  const totalRounds = stats?.total_rounds ?? snapshot.sequence;
  return <><div className="kpi-grid"><Kpi label="Rodadas" value={formatNumber(totalRounds)} note={stats ? "persistidas em SQLite" : "última sequência"} /><Kpi label="Melhor net" value={bestNet == null ? "—" : money(bestNet)} note={histRoute ? `melhor histórico · #${stats.best_net_sequence}` : "sem rota observada"} tone={bestNet > 0 ? "green" : "amber"} /><Kpi label="Gross positivas" value={formatNumber(r.gross_positive)} note={`${formatNumber(r.economically_positive)} economicamente positivas`} /><Kpi label="Latência p95" value={r.latency_p95_ms == null ? "—" : `${r.latency_p95_ms}ms`} note={r.latency_p50_ms == null ? "sem medição" : `p50 ${r.latency_p50_ms}ms`} tone="cyan" /></div><div className="overview-grid"><Section title="Economia da melhor rota" eyebrow="WATERFALL" action={histRoute ? <Badge tone="green">BEST EVER</Badge> : bestRoute ? <Badge tone="amber">OBSERVED</Badge> : null}>{waterfall ? <><div className="route-title"><strong>{waterfall.path}</strong><span>{waterfall.venues}</span></div><div className="waterfall"><div className="water-row cyan"><span>Gross spread</span><div className="water-track"><i style={{ width: `${Math.min(100, Math.max(8, Math.abs(waterfall.gross || 0) * 90))}%` }} /></div><b>{pct(waterfall.gross || 0)}</b></div><div className="water-row green"><span>Net projetado</span><div className="water-track"><i style={{ width: `${Math.min(100, Math.max(8, Math.abs(waterfall.net || 0) * 90))}%` }} /></div><b>{money(waterfall.net)}</b></div></div></> : <EmptyState message="Nenhuma rota real observada ainda." />}</Section><Section title="Estado dos gates" eyebrow="SAFETY GATES"><div className="gates"><Gate label="Economics consistent" value={snapshot.safety.economics_consistent == null ? "sem evidência" : snapshot.safety.economics_consistent ? "confirmado" : "inconsistente"} good={snapshot.safety.economics_consistent === true} /><Gate label="Simulate before execute" value="obrigatório" good /><Gate label="Signer" value="ausente · bloqueado" /><Gate label="Broadcaster" value="ausente · bloqueado" /><Gate label="Mainnet execution" value="bloqueado por política" /></div></Section></div><div className="bottom-grid"><Section title="Atividade de 1 hora" eyebrow="ROUND PERFORMANCE"><RoundChart rounds={rounds} /></Section><Section title="Alertas recentes" eyebrow="ACTIONABLE"><div className="alerts">{snapshot.alerts.length ? snapshot.alerts.map((alert) => <div className="alert" key={alert.title}><span className={`alert-icon ${alert.severity}`}>{alert.severity === "warning" ? "!" : "i"}</span><div><strong>{alert.title}</strong><small>{alert.detail}</small></div></div>) : <EmptyState message="Nenhum alerta emitido pela API." />}</div></Section></div></>; }
function EmptyState({ message }) { return <div className="empty-state">{message}</div>; }
function Kpi({ label, value, note, tone = "" }) { return <div className="kpi"><span>{label}</span><strong className={tone ? `text-${tone}` : ""}>{value}</strong><small>{note}</small></div>; }
function Market({ prices }) { return <Section title="Matriz de preços" eyebrow="MARKET / FRESHNESS">{prices.length ? <div className="table-wrap"><table><thead><tr><th>Token / par</th><th>QuickSwap</th><th>SushiSwap</th><th>Curve</th><th>Uniswap V3</th><th>Net projetado</th></tr></thead><tbody>{prices.map((price) => <tr key={price.pair}><td><strong>{price.pair}</strong></td>{["quickswap", "sushiswap", "curve", "uniswap_v3"].map((dex) => <td className={price[dex] != null ? "mono" : "muted"} key={dex}>{formatPrice(price[dex])}</td>)}<td className="mono">{money(price.net_usd)}</td></tr>)}</tbody></table></div> : <EmptyState message="Nenhum preço real recebido do radar ainda." />}</Section>; }
function Routes({ routes, onlyAuthoritative, setOnlyAuthoritative }) { return <Section title="Rotas avaliadas" eyebrow="ROUTE RANKING" action={<label className="toggle"><input type="checkbox" checked={onlyAuthoritative} onChange={(e) => setOnlyAuthoritative(e.target.checked)} /><span /> somente executáveis</label>}>{routes.length ? <div className="table-wrap"><table className="routes-table"><thead><tr><th>Rota</th><th>Tipo</th><th>Gross</th><th>Net</th><th>Distância</th><th>Estado</th></tr></thead><tbody>{routes.map((route) => <tr key={route.id}><td><strong>{route.path}</strong><small>{route.id} · {route.venues}</small></td><td><Badge tone={route.route_kind === "triangular" ? "cyan" : "outline"}>{route.route_kind === "triangular" ? "3L" : "2L"}</Badge></td><td className="mono">{pct(route.gross)}</td><td className={`mono ${route.net > 0 ? "text-green" : "text-red"}`}>{money(route.net)}</td><td className="mono">{money(route.distance)}</td><td><Badge tone={route.executable ? "green" : "amber"}>{route.status}</Badge>{route.reason && <small className="reason">{route.reason}</small>}</td></tr>)}</tbody></table></div> : <EmptyState message="Nenhuma rota real observada ainda." />}</Section>; }
function Pipeline({ snapshot }) { const steps = [["Quotes", snapshot.round.quotes], ["Edges", snapshot.round.edges], ["Ciclos detectados", snapshot.round.cycles_detected], ["Top ranked", snapshot.round.routes_ranked], ["Re-quote / evaluated", snapshot.round.routes_evaluated], ["Econ. / stable", snapshot.round.economically_positive], ["Risk approved", snapshot.round.risk_approved]]; return <div className="pipeline-layout"><Section title="Funil canônico" eyebrow={`ROUND #${snapshot.sequence}`}><div className="funnel">{steps.map(([label, value], i) => <div className="funnel-row" key={label}><span className="funnel-index">0{i + 1}</span><div className="funnel-name"><strong>{label}</strong><small>{value == null ? "sem medição" : "snapshot real"}</small></div><b>{formatNumber(value)}</b><div className="funnel-bar"><i style={{ width: value == null ? "5%" : `${Math.max(5, 100 - i * 12)}%` }} /></div></div>)}</div></Section><Section title="Rejeições" eyebrow="WHY NOT SELECTED"><EmptyState message="A API ainda não expõe motivos agregados de rejeição." /></Section></div>; }
function Infrastructure({ snapshot }) { return <div className="infra-grid"><Section title="RPC providers" eyebrow="HEALTH">{snapshot.rpc.length ? <div className="rpc-list">{snapshot.rpc.map((rpc) => <div className="rpc-row" key={rpc.alias}><span className={`status-dot ${rpc.status === "healthy" ? "live" : "warn"}`} /><div><strong>{rpc.alias}</strong></div><b>{rpc.latency_ms == null ? "—" : `${rpc.latency_ms}ms`}</b><Badge tone={rpc.status === "healthy" ? "green" : "amber"}>{rpc.status}</Badge></div>)}</div> : <EmptyState message="A API ainda não expõe telemetria detalhada de RPC." />}</Section><Section title="Runtime telemetry" eyebrow="PROCESS"><div className="telemetry"><Kpi label="Worker" value={snapshot.sequence > 0 ? "running" : "starting"} note={snapshot.runtime.phase} tone={snapshot.sequence > 0 ? "green" : "amber"} /><Kpi label="Uptime" value={`${snapshot.runtime.uptime_secs || 0}s`} note="desde startup" /><Kpi label="Prometheus" value="ver endpoint" note="não medido pela API" tone="cyan" /><Kpi label="Queue" value="—" note="não exposto pela API" /></div></Section></div>; }
function Safety({ snapshot }) { return <div className="safety-layout"><Section title="Checklist permanente" eyebrow="FAIL-CLOSED"><div className="safety-list"><Gate label="Dry run" value={snapshot.runtime.dry_run ? "ativo" : "inativo"} good={snapshot.runtime.dry_run} /><Gate label="Signer" value="não exposto pela API" /><Gate label="Broadcaster" value="não exposto pela API" /><Gate label="Wrapper" value="não exposto pela API" /><Gate label="Simulação pré-execução" value={snapshot.safety.simulate_before_execute ? "obrigatória" : "desligada"} good={snapshot.safety.simulate_before_execute} /><Gate label="Chain ID" value={`${snapshot.chain.network} · ${snapshot.chain.chain_id}`} good /></div></Section><Section title="Dados públicos" eyebrow="READ ONLY"><div className="config-grid">{[["Fonte", snapshot.data_source], ["Runtime mode", snapshot.runtime.mode], ["Startup phase", snapshot.runtime.phase], ["Schema", snapshot.schema_version], ["Head block", snapshot.chain.head_block ?? "não exposto"], ["Anchor", snapshot.chain.anchor_block ?? "não exposto"]].map(([label, value]) => <div key={label}><small>{label}</small><strong>{value}</strong></div>)}</div><div className="security-note">Segredos, RPCs, chaves e tokens não são serializados neste console.</div></Section></div>; }

export default function ConsoleRoot() { return <ConsoleErrorBoundary><App /></ConsoleErrorBoundary>; }
