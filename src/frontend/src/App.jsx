import React, { useEffect, useMemo, useState } from "react";
import "./styles.css";

const demoSnapshot = {
  schema_version: "operator.v1",
  sequence: 1842,
  generated_at: new Date(Date.now() - 18000).toISOString(),
  runtime: { mode: "PAPER", dry_run: true, phase: "canonical_discovery", uptime: "04:18:22", shutdown_state: "armed" },
  safety: { signer_present: false, broadcaster_present: false, wrapper_enabled: false, simulate_before_execute: true, economics_consistent: true, mainnet_blocked: true },
  chain: { chain_id: 137, network: "Polygon", head_block: 61288431, anchor_block: 61288428, anchor_hash: "0x7b3f…a91c", confirmations: 3, data_age_ms: 18000 },
  round: { duration_ms: 842, quotes: 1248, edges: 964, cycles_detected: 750, routes_ranked: 750, routes_evaluated: 32, gross_positive: 18, economically_positive: 3, stable: 3, risk_approved: 0, selected: 0, timeouts: 2, latency_p50_ms: 420, latency_p95_ms: 1180 },
  prices: [
    { pair: "WETH / USDC", token: "WETH", quickswap: "3,418.22", sushiswap: "3,416.91", curve: "3,419.08", uniswap_v3: "3,417.64", age_ms: 420, direction: "WETH → USDC", fee_tier: "0.05%" },
    { pair: "WMATIC / USDC", token: "WMATIC", quickswap: "0.3821", sushiswap: "0.3818", curve: "—", uniswap_v3: "0.3824", age_ms: 820, direction: "WMATIC → USDC", fee_tier: "0.30%" },
    { pair: "USDC / USDT", token: "USDC", quickswap: "0.9997", sushiswap: "0.9995", curve: "1.0001", uniswap_v3: "0.9998", age_ms: 1250, direction: "USDC → USDT", fee_tier: "0.01%" },
    { pair: "WBTC / USDC", token: "WBTC", quickswap: "—", sushiswap: "64,982.10", curve: "—", uniswap_v3: "64,967.44", age_ms: 2640, direction: "WBTC → USDC", fee_tier: "0.05%" },
  ],
  routes: [
    { id: "r-1842-01", route_kind: "triangular", path: "WETH → USDC → WMATIC → WETH", venues: "Uni V3 / Quick / Sushi", gross: 0.91, flash: 0.08, gas: 0.31, buffers: 0.12, net: 0.40, distance: 0, anchor: "61288428", age: "0.4s", status: "diagnostic", authoritative: false, reason: "2L/3L shadow: não participa do gate canônico" },
    { id: "r-1842-02", route_kind: "two_leg", path: "WETH → USDC → WETH", venues: "Curve / Uni V3", gross: 0.18, flash: 0.08, gas: 0.31, buffers: 0.12, net: -0.33, distance: 0.33, anchor: "61288428", age: "0.4s", status: "rejected", authoritative: true, reason: "net abaixo do mínimo econômico" },
    { id: "r-1842-03", route_kind: "triangular", path: "USDC → WETH → WMATIC → USDC", venues: "Uni V3 / Quick / Curve", gross: 0.62, flash: 0.08, gas: 0.31, buffers: 0.11, net: 0.12, distance: 0, anchor: "61288428", age: "0.4s", status: "blocked", authoritative: true, reason: "risk gate: broadcaster ausente" },
  ],
  rpc: [
    { alias: "polygon-primary", hash: "sha256:4f1a…9c2e", latency: 188, error: "0.02%", last_success: "2s atrás", status: "healthy" },
    { alias: "polygon-fallback", hash: "sha256:82bf…10d4", latency: 462, error: "0.41%", last_success: "7s atrás", status: "degraded" },
  ],
  alerts: [
    { severity: "warning", title: "Snapshot stale", detail: "Última evidência há 18 segundos", time: "agora" },
    { severity: "info", title: "2 timeouts na rodada", detail: "QuickSwap / WBTC-USDC", time: "há 18s" },
  ],
};

const money = (value) => `${value < 0 ? "−" : ""}$${Math.abs(value).toFixed(2)}`;
const pct = (value) => `${value < 0 ? "−" : ""}${Math.abs(value).toFixed(2)}%`;
const ageLabel = (ms) => (ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`);

function normalizeSnapshot(data) {
  return {
    ...demoSnapshot,
    ...data,
    runtime: { ...demoSnapshot.runtime, ...(data.runtime || {}) },
    safety: { ...demoSnapshot.safety, ...(data.safety || {}) },
    chain: { ...demoSnapshot.chain, ...(data.chain || {}) },
    round: { ...demoSnapshot.round, ...(data.round || {}) },
    prices: data.prices || demoSnapshot.prices,
    routes: data.routes || demoSnapshot.routes,
    rpc: data.rpc || demoSnapshot.rpc,
    alerts: data.alerts || demoSnapshot.alerts,
  };
}

function useOperatorData() {
  const [snapshot, setSnapshot] = useState(demoSnapshot);
  const [connection, setConnection] = useState("demo");

  useEffect(() => {
    let source;
    fetch("/api/v1/snapshot")
      .then((response) => { if (!response.ok) throw new Error("snapshot unavailable"); return response.json(); })
      .then((data) => { setSnapshot(normalizeSnapshot(data)); setConnection("live"); })
      .catch(() => setConnection("demo"));
    try {
      source = new EventSource("/api/v1/events");
      source.addEventListener("snapshot", (event) => { setSnapshot(normalizeSnapshot(JSON.parse(event.data))); setConnection("live"); });
      source.onopen = () => setConnection("live");
      source.onerror = () => setConnection("reconnecting");
    } catch { setConnection("demo"); }
    return () => source?.close();
  }, []);
  return { snapshot, connection };
}

function Badge({ children, tone = "neutral" }) { return <span className={`badge badge-${tone}`}>{children}</span>; }
function Section({ title, eyebrow, action, children, className = "" }) { return <section className={`panel ${className}`}><div className="panel-heading"><div><span className="eyebrow">{eyebrow}</span><h2>{title}</h2></div>{action}</div>{children}</section>; }
function Gate({ label, value, good = false }) { return <div className="gate"><span className={`gate-dot ${good ? "good" : "blocked"}`} /><div><strong>{label}</strong><small>{value}</small></div></div>; }

function App() {
  const { snapshot, connection } = useOperatorData();
  const [section, setSection] = useState("overview");
  const [onlyAuthoritative, setOnlyAuthoritative] = useState(false);
  const age = snapshot.chain?.data_age_ms ?? 0;
  const routes = useMemo(() => (snapshot.routes || []).filter((route) => !onlyAuthoritative || route.authoritative), [snapshot.routes, onlyAuthoritative]);
  const bestRoute = [...(snapshot.routes || [])].sort((a, b) => b.net - a.net)[0];
  const nav = [{ id: "overview", label: "Visão geral" }, { id: "market", label: "Mercado" }, { id: "routes", label: "Rotas" }, { id: "pipeline", label: "Pipeline" }, { id: "infra", label: "Infraestrutura" }, { id: "safety", label: "Segurança" }];

  return <div className="app-shell">
    <aside className="sidebar"><div className="brand"><span className="brand-mark">⌁</span><div><strong>ARGUS</strong><small>operator console</small></div></div><div className="workspace"><span className="eyebrow">WORKSPACE</span><strong>Polygon / dry-run</strong><small>canonical runtime</small></div><nav>{nav.map((item) => <button key={item.id} className={section === item.id ? "active" : ""} onClick={() => setSection(item.id)}><span className="nav-marker" />{item.label}</button>)}</nav><div className="sidebar-footer"><span className="pulse" />Read-only mode<div>schema {snapshot.schema_version}</div></div></aside>
    <main className="main"><header className="topbar"><div className="mobile-brand">ARGUS <span>operator console</span></div><div className="topbar-status"><Badge tone="cyan">POLYGON</Badge><Badge tone="amber">DRY RUN</Badge><span className="status-item"><i className={`status-dot ${connection === "live" ? "live" : "warn"}`} />{connection === "live" ? "SSE connected" : connection === "reconnecting" ? "reconnecting" : "fixture mode"}</span><span className="status-item">anchor <b>{snapshot.chain?.anchor_block}</b></span><span className="status-item">data age <b className={age > 10000 ? "text-amber" : ""}>{ageLabel(age)}</b></span></div></header>
      <div className="content"><div className="page-intro"><div><span className="eyebrow">OPERATOR / {section.toUpperCase()}</span><h1>{nav.find((item) => item.id === section)?.label}</h1><p>Snapshot autoritativo · sequência <span className="mono">#{snapshot.sequence}</span> · {new Date(snapshot.generated_at).toLocaleTimeString("pt-BR")}</p></div><div className="intro-actions"><Badge tone="outline">READ ONLY</Badge><button className="icon-button" aria-label="Atualizar snapshot" onClick={() => window.location.reload()}>↻</button></div></div>
      {age > 10000 && <div className="stale-banner"><span>!</span><div><strong>Dados potencialmente stale</strong><small>O console preserva o último snapshot conhecido. Nenhuma decisão de execução é habilitada.</small></div></div>}
      {section === "overview" && <Overview snapshot={snapshot} bestRoute={bestRoute} />}
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

function Overview({ snapshot, bestRoute }) { const r = snapshot.round; return <><div className="kpi-grid"><Kpi label="Rodadas" value={snapshot.sequence.toLocaleString("pt-BR")} note="última sequência" /><Kpi label="Melhor net" value={bestRoute ? money(bestRoute.net) : "unknown"} note="após custos" tone={bestRoute?.net > 0 ? "green" : "amber"} /><Kpi label="Gross positivas" value={r.gross_positive} note={`${r.economically_positive} economicamente positivas`} /><Kpi label="Latência p95" value={`${r.latency_p95_ms}ms`} note={`p50 ${r.latency_p50_ms}ms`} tone="cyan" /></div><div className="overview-grid"><Section title="Economia da melhor rota" eyebrow="WATERFALL" action={<Badge tone={bestRoute?.authoritative ? "green" : "amber"}>{bestRoute?.authoritative ? "AUTHORITATIVE" : "DIAGNOSTIC"}</Badge>}><div className="route-title"><strong>{bestRoute?.path}</strong><span>{bestRoute?.venues}</span></div><div className="waterfall">{[["Gross", bestRoute?.gross, "cyan"], ["Flashloan fee", -bestRoute?.flash, "neutral"], ["Gas estimate", -bestRoute?.gas, "neutral"], ["Buffers", -bestRoute?.buffers, "neutral"], ["Net", bestRoute?.net, bestRoute?.net > 0 ? "green" : "red"]].map(([label, value, tone]) => <div className={`water-row ${tone}`} key={label}><span>{label}</span><div className="water-track"><i style={{ width: `${Math.min(100, Math.max(8, Math.abs(value) * 90))}%` }} /></div><b>{money(value || 0)}</b></div>)}</div></Section><Section title="Estado dos gates" eyebrow="SAFETY GATES"><div className="gates"><Gate label="Economics consistent" value="evidência sequencial" good /><Gate label="Simulate before execute" value="obrigatório" good /><Gate label="Signer" value="ausente · bloqueado" /><Gate label="Broadcaster" value="ausente · bloqueado" /><Gate label="Mainnet execution" value="bloqueado por política" /></div></Section></div><div className="bottom-grid"><Section title="Atividade de 1 hora" eyebrow="ROUND PERFORMANCE"><div className="chart"><div className="chart-labels"><span>gross / net USD</span><span>últimas 12 rodadas</span></div><div className="bars">{[24, 42, 38, 61, 52, 68, 48, 78, 60, 84, 72, 91].map((height, i) => <div className="bar-group" key={i}><i style={{ height: `${height}%` }} /><b style={{ height: `${Math.max(8, height - 18)}%` }} /></div>)}</div><div className="break-even"><span /> break-even <span className="mono">$0.00</span></div></div></Section><Section title="Alertas recentes" eyebrow="ACTIONABLE"><div className="alerts">{(snapshot.alerts || []).map((alert) => <div className="alert" key={alert.title}><span className={`alert-icon ${alert.severity}`}>{alert.severity === "warning" ? "!" : "i"}</span><div><strong>{alert.title}</strong><small>{alert.detail}</small></div><time>{alert.time}</time></div>)}</div></Section></div></>; }
function Kpi({ label, value, note, tone = "" }) { return <div className="kpi"><span>{label}</span><strong className={tone ? `text-${tone}` : ""}>{value}</strong><small>{note}</small></div>; }
function Market({ prices }) { return <Section title="Matriz de preços" eyebrow="MARKET / FRESHNESS" action={<div className="filters"><button className="filter active">Todos os tokens</button><button className="filter">Fresh &lt; 2s</button></div>}><div className="table-wrap"><table><thead><tr><th>Token / par</th><th>QuickSwap</th><th>SushiSwap</th><th>Curve</th><th>Uniswap V3</th><th>Direção</th><th>Idade</th></tr></thead><tbody>{prices.map((price) => <tr key={price.pair}><td><strong>{price.pair}</strong><small>{price.fee_tier}</small></td>{["quickswap", "sushiswap", "curve", "uniswap_v3"].map((dex) => <td className={price[dex] !== "—" ? "mono" : "muted"} key={dex}>{price[dex]}</td>)}<td><Badge tone="outline">{price.direction}</Badge></td><td className={price.age_ms > 2000 ? "text-amber mono" : "mono"}>{ageLabel(price.age_ms)}</td></tr>)}</tbody></table></div><div className="table-foot"><span>Spread bruto é informativo e não representa executabilidade.</span><span><i className="legend-dot cyan" /> quote disponível <i className="legend-dot amber" /> stale / outlier</span></div></Section>; }
function Routes({ routes, onlyAuthoritative, setOnlyAuthoritative }) { return <Section title="Rotas avaliadas" eyebrow="ROUTE RANKING" action={<label className="toggle"><input type="checkbox" checked={onlyAuthoritative} onChange={(e) => setOnlyAuthoritative(e.target.checked)} /><span /> somente autoritativas</label>}><div className="table-wrap"><table className="routes-table"><thead><tr><th>Rota</th><th>Tipo</th><th>Gross</th><th>Custos</th><th>Net</th><th>Distância</th><th>Anchor</th><th>Estado</th></tr></thead><tbody>{routes.map((route) => <tr key={route.id}><td><strong>{route.path}</strong><small>{route.id} · {route.venues}</small></td><td><Badge tone={route.route_kind === "triangular" ? "cyan" : "outline"}>{route.route_kind === "triangular" ? "3L" : "2L"}</Badge>{!route.authoritative && <Badge tone="amber">diag</Badge>}</td><td className="mono">{pct(route.gross)}</td><td className="mono muted">{money(route.flash + route.gas + route.buffers)}</td><td className={`mono ${route.net > 0 ? "text-green" : "text-red"}`}>{money(route.net)}</td><td className="mono">{money(route.distance)}</td><td className="mono">{route.anchor}<small>{route.age}</small></td><td><Badge tone={route.status === "rejected" ? "red" : route.status === "blocked" ? "amber" : "green"}>{route.status}</Badge><small className="reason">{route.reason}</small></td></tr>)}</tbody></table></div></Section>; }
function Pipeline({ snapshot }) { const steps = [["Quotes", snapshot.round.quotes, "100%"], ["Edges", snapshot.round.edges, "77%"], ["Ciclos detectados", snapshot.round.cycles_detected, "60%"], ["Top ranked", snapshot.round.routes_ranked, "750 / 750"], ["Re-quote / evaluated", snapshot.round.routes_evaluated, "32 / 750"], ["Econ. / stable", snapshot.round.economically_positive, `${snapshot.round.stable} stable`], ["Risk approved", snapshot.round.risk_approved, "bloqueado"]]; return <div className="pipeline-layout"><Section title="Funil canônico" eyebrow="ROUND #{snapshot.sequence}"><div className="funnel">{steps.map(([label, value, note], i) => <div className="funnel-row" key={label}><span className="funnel-index">0{i + 1}</span><div className="funnel-name"><strong>{label}</strong><small>{note}</small></div><b>{value.toLocaleString("pt-BR")}</b><div className="funnel-bar"><i style={{ width: `${Math.max(5, 100 - i * 12)}%` }} /></div></div>)}</div></Section><Section title="Rejeições" eyebrow="WHY NOT SELECTED"><div className="rejections"><div><strong>Net abaixo do mínimo</strong><b>14</b></div><div><strong>Instabilidade de quote</strong><b>9</b></div><div><strong>Risk gate / broadcaster</strong><b>3</b></div><div><strong>Timeout</strong><b>{snapshot.round.timeouts}</b></div></div></Section></div>; }
function Infrastructure({ snapshot }) { return <div className="infra-grid"><Section title="RPC providers" eyebrow="HEALTH"><div className="rpc-list">{(snapshot.rpc || []).map((rpc) => <div className="rpc-row" key={rpc.alias}><span className={`status-dot ${rpc.status === "healthy" ? "live" : "warn"}`} /><div><strong>{rpc.alias}</strong><small>{rpc.hash}</small></div><b>{rpc.latency}ms</b><Badge tone={rpc.status === "healthy" ? "green" : "amber"}>{rpc.status}</Badge><small>{rpc.error} errors · {rpc.last_success}</small></div>)}</div></Section><Section title="Runtime telemetry" eyebrow="PROCESS"><div className="telemetry"><Kpi label="Worker" value="running" note="canonical loop" tone="green" /><Kpi label="Uptime" value={snapshot.runtime.uptime || `${snapshot.runtime.uptime_secs || 0}s`} note="desde startup" /><Kpi label="Prometheus" value="ready" note="aggregated metrics" tone="cyan" /><Kpi label="Queue" value="0" note="backpressure clear" /></div></Section></div>; }
function Safety({ snapshot }) { return <div className="safety-layout"><Section title="Checklist permanente" eyebrow="FAIL-CLOSED"><div className="safety-list"><Gate label="Dry run" value={snapshot.runtime.dry_run ? "ativo" : "inativo"} good={snapshot.runtime.dry_run} /><Gate label="Signer" value={snapshot.safety.signer_present ? "configured" : "not configured"} good={snapshot.safety.signer_present} /><Gate label="Broadcaster" value={snapshot.safety.broadcaster_present ? "configured" : "not configured"} good={snapshot.safety.broadcaster_present} /><Gate label="Wrapper" value={snapshot.safety.wrapper_enabled ? "enabled" : "disabled"} good={snapshot.safety.wrapper_enabled} /><Gate label="Simulação pré-execução" value={snapshot.safety.simulate_before_execute ? "obrigatória" : "desligada"} good={snapshot.safety.simulate_before_execute} /><Gate label="Chain ID" value={`${snapshot.chain.network} · ${snapshot.chain.chain_id}`} good /></div></Section><Section title="Configuração pública" eyebrow="REDACTED / READ ONLY"><div className="config-grid">{[["Runtime mode", "PAPER"], ["Discovery timeout", "2,000 ms"], ["Route limit", "32"], ["Quote concurrency", "24"], ["RPC credentials", "not configured"], ["Execution controls", "locked by policy"]].map(([label, value]) => <div key={label}><small>{label}</small><strong>{value}</strong></div>)}</div><div className="security-note">Segredos, RPCs, chaves e tokens nunca são serializados neste console.</div></Section></div>; }

export default function ConsoleRoot() { return <ConsoleErrorBoundary><App /></ConsoleErrorBoundary>; }
