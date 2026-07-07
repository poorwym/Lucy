import "cesium/Build/Cesium/Widgets/widgets.css";
import "./App.css";

import { useEffect, useRef, useState } from "react";
import * as Cesium from "cesium";

const TILESET_URL = "/tileset.json";

function App() {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [status, setStatus] = useState("Loading Lucy tileset...");

  useEffect(() => {
    const container = containerRef.current;
    if (!container) {
      return;
    }

    let viewer: Cesium.Viewer | undefined;
    let cancelled = false;

    Cesium.Ion.defaultAccessToken =
      "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJqdGkiOiIwMGYzYTM2Yi0yOGIwLTQ4ZGUtOWQ2NC03ZGE2MGQ1NTQzOWYiLCJpZCI6Mjg0ODk3LCJpYXQiOjE3NDIxODM0OTV9.AVIChWRSQIPf82NfHfz9K88x2nbo7PF3EQUb-z_-r1w";

    const loadScene = async () => {
      try {
        viewer = new Cesium.Viewer(container, {});

        viewer.scene.globe.show = true;

        const tileset = await Cesium.Cesium3DTileset.fromUrl(TILESET_URL);
        if (cancelled || !viewer) {
          tileset.destroy();
          return;
        }

        viewer.scene.primitives.add(tileset);
        const focused = await viewer.zoomTo(tileset);

        if (focused) {
          setStatus(`Loaded ${TILESET_URL}`);
        } else {
          setStatus(`Loaded ${TILESET_URL}; camera focus was not available`);
        }
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        setStatus(`Cesium smoke failed: ${message}`);
        console.error(error);
      }
    };

    void loadScene();

    return () => {
      cancelled = true;
      viewer?.destroy();
    };
  }, []);

  return (
    <main className="fixed inset-0 overflow-hidden bg-[#090b0f]">
      <div ref={containerRef} className="absolute inset-0" />
      <div
        className="absolute left-3 top-3 z-10 flex max-w-[calc(100vw-24px)] items-center gap-2.5 rounded-md border border-white/15 bg-[#080a0ec7] px-2.5 py-2 font-sans text-[13px] leading-snug text-[#f5f7fb] shadow-[0_12px_28px_rgba(0,0,0,0.32)] backdrop-blur-md max-sm:flex-col max-sm:items-start max-sm:gap-1.5 sm:max-w-[620px]"
        role="status"
      >
        <span className="min-w-0 overflow-hidden text-ellipsis whitespace-nowrap max-sm:whitespace-normal">
          {status}
        </span>
        <code className="shrink-0 rounded bg-white/10 px-1.5 py-1 font-mono text-xs leading-tight text-[#dce7ff]">
          {TILESET_URL}
        </code>
      </div>
    </main>
  );
}

export default App;
