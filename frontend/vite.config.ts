import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// 开发时把 /api 转发到本地的 niulai serve；生产环境由 niulai serve 直接提供前端文件，同源访问
const apiTarget = process.env.NIULAI_API_URL ?? "http://127.0.0.1:8080";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    proxy: { "/api": { target: apiTarget, changeOrigin: true } },
  },
  build: {
    chunkSizeWarningLimit: 800,
    // 图表库和 React 运行时单独分块：它们不随业务代码变化，改页面时用户只需要重新下载小的业务块，
    // 首屏也不必等着 lightweight-charts 下载完。
    //
    // Vite 8 底层是 rolldown，没有 Rollup 的 manualChunks 对象写法（只接受函数），
    // 分组要用 output.codeSplitting.groups。react 那条用字符数 3 起匹配，避开 @vitejs/plugin-react 等
    // 名字里也带 react 的开发期依赖。
    rollupOptions: {
      output: {
        codeSplitting: {
          groups: [
            {
              name: "charts",
              test: /node_modules[\\/]lightweight-charts/,
              priority: 20,
            },
            {
              name: "react",
              test: /node_modules[\\/](?:react|react-dom|react-router|scheduler)[\\/]/,
              priority: 10,
            },
          ],
        },
      },
    },
  },
});
