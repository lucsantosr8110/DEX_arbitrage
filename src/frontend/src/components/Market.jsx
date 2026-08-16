// Market — matriz de preços por venue. Estável, sem alterações.

import { EmptyState, Section } from "./primitives.jsx";
import { formatPrice, money } from "../lib/format.js";

export function Market({ prices }) {
  return (
    <Section title="Matriz de preços" eyebrow="MARKET / FRESHNESS">
      {prices.length ? (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Token / par</th>
                <th>QuickSwap</th>
                <th>SushiSwap</th>
                <th>Curve</th>
                <th>Uniswap V3</th>
                <th>Net projetado</th>
              </tr>
            </thead>
            <tbody>
              {prices.map((price) => (
                <tr key={price.pair}>
                  <td><strong>{price.pair}</strong></td>
                  {["quickswap", "sushiswap", "curve", "uniswap_v3"].map((dex) => (
                    <td className={price[dex] != null ? "mono" : "muted"} key={dex}>{formatPrice(price[dex])}</td>
                  ))}
                  <td className="mono">{money(price.net_usd)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : <EmptyState message="Nenhum preço real recebido do radar ainda." />}
    </Section>
  );
}