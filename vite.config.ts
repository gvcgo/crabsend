import { defineConfig } from "vite";

// Tauri drives this dev server; the port is fixed so `tauri.conf.json` can
// point at it without guessing.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**", "**/crates/**", "**/target/**"] },
  },
  build: {
    target: "es2022",
    minify: "esbuild",
    sourcemap: true,
  },
});
