import "cesium/Build/Cesium/Widgets/widgets.css";
import "./App.css";

import { useEffect, useRef, useState } from "react";
import * as Cesium from "cesium";

const searchParams = new URLSearchParams(window.location.search);
const TILESET_URL =
  searchParams.get("tileset") ??
  import.meta.env.VITE_TILESET_URL ??
  "/sources/surface_buildings_7415/tileset.json";

// const numberParam = (name: string, fallback: number) => {
//   const raw = searchParams.get(name);
//   if (raw === null) {
//     return fallback;
//   }
//   const value = Number(raw);
//   return Number.isFinite(value) ? value : fallback;
// };
// const cameraLongitude = numberParam("lon", 160.252);
// const cameraLatitude = numberParam("lat", -9.121);
// const cameraHeight = numberParam("height", 50_000);
const offline = searchParams.get("offline") === "1";
// const locationLabel = `${cameraLongitude.toFixed(5)}, ${cameraLatitude.toFixed(5)}`;

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

    if (!offline) {
      Cesium.Ion.defaultAccessToken =
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJqdGkiOiIwMGYzYTM2Yi0yOGIwLTQ4ZGUtOWQ2NC03ZGE2MGQ1NTQzOWYiLCJpZCI6Mjg0ODk3LCJpYXQiOjE3NDIxODM0OTV9.AVIChWRSQIPf82NfHfz9K88x2nbo7PF3EQUb-z_-r1w";
    }

    const loadScene = async () => {
      try {
        viewer = new Cesium.Viewer(
          container,
          offline
            ? {
                baseLayer: false,
                terrainProvider: new Cesium.EllipsoidTerrainProvider(),
              }
            : {},
        );

        viewer.scene.globe.show = true;

        // Lucy writes ENU-to-ECEF once on the 3D Tiles root. GLB positions use
        // standard glTF Y-up, so Cesium's default Y-up-to-Z-up step is required.
        const tileset = await Cesium.Cesium3DTileset.fromUrl(TILESET_URL);
        if (cancelled || !viewer) {
          tileset.destroy();
          return;
        }

        viewer.scene.primitives.add(tileset);
        //
        // # Target EPSG:4979 region and ellipsoidal heights. The explicit operation
        // # includes the EPSG:1149 ETRS89-to-WGS84 zero-translation approximation.
        // bounds:
        //   west: 5.84970
        //   south: 50.83985
        //   east: 5.85071
        //   north: 50.84021
        //   min_height_m: 170.0
        //   max_height_m: 201.0

        viewer.camera.flyTo({
          destination: Cesium.Rectangle.fromDegrees(
            5.8497,
            50.83985,
            5.85071,
            50.84021,
          ),
          orientation: {
            heading: 0,
            pitch: Cesium.Math.toRadians(-60),
            roll: 0,
          },
        });

        // tileset.debugShowBoundingVolume = true;
        // tileset.style = new Cesium.Cesium3DTileStyle({
        //   show: "true",
        //   color: "color('red', 1.0)",
        // });

        // let loadedTiles = 0;
        // let failedTiles = 0;
        // tileset.tileLoad.addEventListener(() => {
        //   loadedTiles += 1;
        //   setStatus(
        //     `Debugging near ${locationLabel} · loaded ${loadedTiles} · failed ${failedTiles}`,
        //   );
        // });
        // tileset.tileFailed.addEventListener((failure) => {
        //   failedTiles += 1;
        //   console.error("Lucy tile failed", failure);
        //   setStatus(
        //     `Debugging near ${locationLabel} · loaded ${loadedTiles} · failed ${failedTiles}`,
        //   );
        // });

        // viewer.camera.flyTo({
        //   destination: Cesium.Cartesian3.fromDegrees(
        //     cameraLongitude,
        //     cameraLatitude,
        //     cameraHeight,
        //   ),
        //   orientation: {
        //     heading: 0,
        //     pitch: Cesium.Math.toRadians(-60),
        //     roll: 0,
        //   },
        // });

        // setStatus(
        //   `Debugging near ${locationLabel} · loaded ${loadedTiles} · failed ${failedTiles}`,
        // );
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
