// Safety — checklist fail-closed + dados públicos serializados.

import { Gate, Section } from "./primitives.jsx";

export function Safety({ snapshot }) {
  return (
    <div className="safety-layout">
      <Section title="Checklist permanente" eyebrow="FAIL-CLOSED">
        <div className="safety-list">
          <Gate label="Dry run" value={snapshot.runtime.dry_run ? "ativo" : "inativo"} good={snapshot.runtime.dry_run} />
          <Gate label="Signer" value="não exposto pela API" />
          <Gate label="Broadcaster" value="não exposto pela API" />
          <Gate label="Wrapper" value="não exposto pela API" />
          <Gate label="Simulação pré-execução" value={snapshot.safety.simulate_before_execute ? "obrigatória" : "desligada"} good={snapshot.safety.simulate_before_execute} />
          <Gate label="Chain ID" value={`${snapshot.chain.network} · ${snapshot.chain.chain_id}`} good />
        </div>
      </Section>
      <Section title="Dados públicos" eyebrow="READ ONLY">
        <div className="config-grid">
          {[
            ["Fonte", snapshot.data_source],
            ["Runtime mode", snapshot.runtime.mode],
            ["Startup phase", snapshot.runtime.phase],
            ["Schema", snapshot.schema_version],
            ["Head block", snapshot.chain.head_block ?? "não exposto"],
            ["Anchor", snapshot.chain.anchor_block ?? "não exposto"],
          ].map(([label, value]) => (
            <div key={label}>
              <small>{label}</small>
              <strong>{value}</strong>
            </div>
          ))}
        </div>
        <div className="security-note">Segredos, RPCs, chaves e tokens não são serializados neste console.</div>
      </Section>
    </div>
  );
}