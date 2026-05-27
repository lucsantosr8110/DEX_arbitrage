import { useEffect, useState } from "react";
import axios from "axios";

function App() {
  const [config, setConfig] = useState(null);

  useEffect(() => {
    axios.get("http://localhost:8080/config").then(res => {
      setConfig(res.data);
    });
  }, []);

  const updateConfig = () => {
    axios.post("http://localhost:8080/config", config)
      .then(() => alert("Config atualizada!"));
  };

  return config ? (
    <div style={{ padding: 20 }}>
      <h2>🔧 Configuração do Bot</h2>
      <label>max_loan:</label>
      <input
        value={config.general.max_loan}
        onChange={(e) => setConfig({
          ...config,
          general: {
            ...config.general,
            max_loan: e.target.value
          }
        })}
      />
      <br/><br/>
      <button onClick={updateConfig}>Salvar</button>
    </div>
  ) : <p>Carregando...</p>;
}

export default App;
