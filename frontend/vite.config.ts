import { defineConfig, type Plugin } from "vite";
import react, { reactCompilerPreset } from "@vitejs/plugin-react";
import babel from "@rolldown/plugin-babel";
import tailwindcss from "@tailwindcss/vite";
import cesium from "vite-plugin-cesium";

// vite-plugin-cesium ships a callable ESM default but its declaration is
// interpreted as a module namespace under TypeScript's NodeNext mode.
const cesiumPlugin = cesium as unknown as () => Plugin;

// https://vite.dev/config/
export default defineConfig({
  plugins: [
    tailwindcss(),
    react(),
    cesiumPlugin(),
    babel({ presets: [reactCompilerPreset()] }),
  ],
  server: {
    proxy: {
      "/sources": "http://127.0.0.1:8080",
      "/tileset.json": "http://127.0.0.1:8080",
      "/subtrees": "http://127.0.0.1:8080",
      "/content": "http://127.0.0.1:8080",
    },
  },
});
