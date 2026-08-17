// Formatters compartilhados. money/pct/age mantêm comportamento original;
// formatDualTime adiciona UTC + local para timestamps (UX brasileira UTC-3).

export const money = (value) =>
  value == null || !Number.isFinite(value) ? "—" : `${value < 0 ? "−" : ""}$${Math.abs(value).toFixed(2)}`;

export const pct = (value) =>
  `${value < 0 ? "−" : ""}${Math.abs(value).toFixed(2)}%`;

export const ageLabel = (ms) =>
  ms == null ? "—" : ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;

export const formatPrice = (value) =>
  value == null ? "—" : Number(value).toLocaleString("en-US", { maximumFractionDigits: 8 });

export const formatNumber = (value) =>
  value == null ? "—" : Number(value).toLocaleString("pt-BR");

export const saneGross = (value) => Number.isFinite(value) && Math.abs(value) <= 50;
export const saneNet = (value) => Number.isFinite(value) && Math.abs(value) <= 500;

// Aceita ISO string (UTC) ou epoch ms. Devolve `[utc, local]` formatados
// em pt-BR. `null` quando input ausente.
export function formatDualTime(value) {
  if (!value) return [null, null];
  const date = typeof value === "number" ? new Date(value) : new Date(value);
  if (Number.isNaN(date.getTime())) return [null, null];
  return [
    date.toLocaleTimeString("pt-BR", { timeZone: "UTC", hour12: false }),
    date.toLocaleTimeString("pt-BR", { hour12: false }),
  ];
}

// "há 3 min", "há 12s". `value` em ms ou Date.
export function timeAgo(value) {
  if (value == null) return "—";
  const ts = typeof value === "number" ? value : new Date(value).getTime();
  if (Number.isNaN(ts)) return "—";
  const diff = Date.now() - ts;
  if (diff < 0) return "agora";
  if (diff < 60_000) return `há ${Math.round(diff / 1000)}s`;
  if (diff < 3_600_000) return `há ${Math.round(diff / 60_000)}min`;
  if (diff < 86_400_000) return `há ${Math.round(diff / 3_600_000)}h`;
  return `há ${Math.round(diff / 86_400_000)}d`;
}