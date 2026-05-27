// frontend/src/App.jsx
import { useEffect, useState } from "react";

function App() {
  const [config, setConfig] = useState(null);

  useEffect(() => {
    // Agora usa o proxy do Vite -> vai pro backend (porta 8080)
    fetch("/config")
      .then((res) => {
        if (!res.ok) throw new Error("Erro ao buscar config");
        return res.json();
      })
      .then((data) => setConfig(data))
      .catch((err) => console.error("❌ Falha ao carregar config:", err));
  }, []);

  if (!config) return <p>⏳ Carregando configuração...</p>;

  return (
    <div style={styles.container}>
      <h1 style={styles.title}>⚡ Flashloan Bot Dashboard</h1>

      <section style={styles.section}>
        <h2 style={styles.subtitle}>🔧 Configuração Atual</h2>
        <table style={styles.table}>
          <tbody>
            <tr>
              <td style={styles.tdLabel}>🌐 RPC URL</td>
              <td style={styles.tdValue}>{config.network.rpc_url}</td>
            </tr>
            <tr>
              <td style={styles.tdLabel}>🪙 Chain ID</td>
              <td style={styles.tdValue}>{config.network.chain_id}</td>
            </tr>
            <tr>
              <td style={styles.tdLabel}>📈 Log Level</td>
              <td style={styles.tdValue}>{config.logging.level}</td>
            </tr>
          </tbody>
        </table>
      </section>
    </div>
  );
}

const styles = {
  container: {
    fontFamily: "Arial, sans-serif",
    padding: "20px",
    backgroundColor: "#0d1117",
    color: "#e6edf3",
    minHeight: "100vh",
  },
  title: {
    textAlign: "center",
    fontSize: "2rem",
    marginBottom: "20px",
  },
  section: {
    maxWidth: "600px",
    margin: "0 auto",
    backgroundColor: "#161b22",
    padding: "20px",
    borderRadius: "10px",
    boxShadow: "0 0 10px rgba(0,0,0,0.5)",
  },
  subtitle: {
    marginBottom: "15px",
    fontSize: "1.2rem",
    borderBottom: "1px solid #30363d",
    paddingBottom: "5px",
  },
  table: {
    width: "100%",
    borderCollapse: "collapse",
  },
  tdLabel: {
    fontWeight: "bold",
    padding: "8px",
    borderBottom: "1px solid #30363d",
    width: "40%",
  },
  tdValue: {
    padding: "8px",
    borderBottom: "1px solid #30363d",
    wordBreak: "break-all",
  },
};

export default App;
