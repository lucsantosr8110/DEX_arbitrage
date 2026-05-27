import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173, // Porta padrão
    proxy: {
      "/config": "http://localhost:8080", // redireciona API do bot
    },
  },
});
