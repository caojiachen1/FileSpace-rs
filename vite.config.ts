import { defineConfig } from "vite";

// Tauri 开发配置
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
  build: {
    target: "chrome120",
    minify: "esbuild",
    sourcemap: false,
  },
});
