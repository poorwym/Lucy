set dotenv-load := true

compose := "docker compose"
db_user := env("POSTGRES_USER", "lucy")
db_name := env("POSTGRES_DB", "lucy")
database_url := env("DATABASE_URL", "postgres://lucy:lucy@localhost:5432/lucy")
lucy_addr := env("LUCY_BIND", "127.0.0.1:8080")
image_repository := env("LUCY_IMAGE_REPOSITORY", "ghcr.io/poorwym/lucy")
buildx_builder := env("LUCY_BUILDX_BUILDER", "lucy-multiarch")
revision := `git rev-parse HEAD`
rdnap_pipeline := "+proj=pipeline +step +inv +proj=sterea +lat_0=52.1561605555556 +lon_0=5.38763888888889 +k=0.9999079 +x_0=155000 +y_0=463000 +ellps=bessel +step +proj=hgridshift +grids=nl_nsgi_rdtrans2018.tif +step +proj=vgridshift +grids=nl_nsgi_nlgeo2018.tif +multiplier=1 +step +proj=cart +ellps=GRS80 +step +proj=helmert +x=0 +y=0 +z=0 +step +inv +proj=cart +ellps=WGS84 +step +proj=unitconvert +xy_in=rad +xy_out=deg"
fin2023_pipeline := "+proj=pipeline +step +inv +proj=tmerc +lat_0=0 +lon_0=25 +k=1 +x_0=25500000 +y_0=0 +ellps=GRS80 +step +proj=vgridshift +grids=fi_nls_fin2023n2000.tif +multiplier=1 +step +proj=cart +ellps=GRS80 +step +proj=helmert +x=0 +y=0 +z=0 +step +inv +proj=cart +ellps=WGS84 +step +proj=unitconvert +xy_in=rad +xy_out=deg"
helsinki_citygml_url := "https://3d.hel.ninja/data/citygml/Helsinki3D_CityGML_Kalasatama_20190326.zip"
helsinki_citygml_sha256 := "ef6a787068b82642e5a0be5e20268e137075bb41fdbf0ec88619ad79926e2299"

default:
    just --list

# Start services in the background.
up:
    {{ compose }} up -d --build

# Run PostGIS and the source-mounted Lucy development image with hot reload.
dev:
    {{ compose }} up -d --build --wait postgres
    just load-sample-fixture
    {{ compose }} --profile dev up --build lucy

# Stop the hot-reload development stack.
dev-down:
    {{ compose }} --profile dev down

# Stop services without deleting volumes.
down:
    {{ compose }} down

# Restart services.
restart:
    {{ compose }} restart

# Show service status.
ps:
    {{ compose }} ps

# Follow service logs.
logs service="":
    {{ compose }} logs -f {{ service }}

# Pull service images.
pull:
    {{ compose }} pull

# Open a psql shell in the PostGIS container.
psql:
    {{ compose }} exec postgres psql -U {{ db_user }} -d {{ db_name }}

# Load the deterministic sample PostGIS source used by local development.
load-sample-fixture:
    {{ compose }} exec -T postgres psql -U {{ db_user }} -d {{ db_name }} -f - < fixtures/postgis/poc_buildings.sql

# Load the synthetic EPSG:7415 PolygonZ/MultiPolygonZ source.
load-surface-fixture:
    {{ compose }} exec -T postgres psql -U {{ db_user }} -d {{ db_name }} -f - < fixtures/postgis/surface_buildings_7415.sql

# Load every deterministic PostGIS fixture used by the workspace.
load-fixtures: load-sample-fixture load-surface-fixture

# Download the pinned 2019 Helsinki Kalasatama LoD2 CityGML archive (CC BY 4.0).
download-helsinki-kalasatama output="/tmp/Helsinki3D_CityGML_Kalasatama_20190326.zip":
    curl --fail --show-error --location --output '{{ output }}' '{{ helsinki_citygml_url }}'
    echo "{{ helsinki_citygml_sha256 }}  {{ output }}" | shasum -a 256 --check

# Import true LoD2 XYZ surfaces and atomically replace the normalized EPSG:4979 relation.
load-helsinki-kalasatama archive="/tmp/Helsinki3D_CityGML_Kalasatama_20190326.zip": verify-fin2023n2000-grid
    python3 scripts/importers/import_helsinki_citygml.py '{{ archive }}' --database-url '{{ database_url }}' --replace

# Fail if the pinned grids or explicit RDNAPTRANS2018 + EPSG:1149 pipeline drift.
verify-rdnap-grids:
    {{ compose }} exec -T postgres psql -U {{ db_user }} -d {{ db_name }} -Atc "WITH transformed AS (SELECT ST_TransformPipeline(ST_GeomFromEWKT('SRID=7415;POINT Z (121302 487371 2.68)'), '{{ rdnap_pipeline }}', 4979) AS geom) SELECT abs(ST_X(geom) - 4.892367035931109) < 0.0000001 AND abs(ST_Y(geom) - 52.37317920269912) < 0.0000001 AND abs(ST_Z(geom) - 45.66258579945144) < 0.05 FROM transformed" | grep -qx t

# Fail if the FIN2023N2000 grid is missing or source GIS axis order drifts.
verify-fin2023n2000-grid:
    {{ compose }} exec -T postgres psql -U {{ db_user }} -d {{ db_name }} -Atc "WITH transformed AS (SELECT ST_TransformPipeline(ST_SetSRID(ST_MakePoint(25497750, 6676280, 2.68), 3879), '{{ fin2023_pipeline }}', 4979) AS geom) SELECT abs(ST_X(geom) - 24.95943315450587) < 0.0000001 AND abs(ST_Y(geom) - 60.19931510976058) < 0.0000001 AND abs(ST_Z(geom) - 20.27470033017963) < 0.03 FROM transformed" | grep -qx t

# Verify the imported relation retains its complete non-degenerate LoD2 inventory.
verify-helsinki-kalasatama:
    {{ compose }} exec -T postgres psql -U {{ db_user }} -d {{ db_name }} -Atc "SELECT count(*), sum(source_polygon_count), sum(source_vertex_count), sum(polygon_count), GeometryType(geom), ST_NDims(geom), ST_SRID(geom) FROM public.helsinki_kalasatama_lod2 GROUP BY GeometryType(geom), ST_NDims(geom), ST_SRID(geom)" | grep -qx '2919|79810|454444|295576|MULTIPOLYGON|3|4979'

# Verify the fixture retains both geometry types and its interior ring.
verify-surface-fixture:
    {{ compose }} exec -T postgres psql -U {{ db_user }} -d {{ db_name }} -Atc "WITH polygons AS (SELECT (ST_Dump(geom)).geom AS geom FROM public.surface_buildings_7415) SELECT (SELECT count(*) FROM public.surface_buildings_7415), (SELECT count(*) FROM public.surface_buildings_7415 WHERE GeometryType(geom) = 'POLYGON'), (SELECT count(*) FROM public.surface_buildings_7415 WHERE GeometryType(geom) = 'MULTIPOLYGON'), (SELECT count(*) FROM polygons WHERE ST_NumInteriorRings(geom) > 0)" | grep -qx '2|1|1|1'

# Run server tests against PostGIS without silent database-test skips.
test-postgis: load-fixtures verify-rdnap-grids verify-surface-fixture
    DATABASE_URL={{ database_url }} cargo test -p lucy-server -- --include-ignored --nocapture

# Run the Lucy server directly on the host.
server config="config/fixture-sources.yaml" addr=lucy_addr:
    DATABASE_URL={{ database_url }} cargo run -p lucy -- serve --config {{ config }} --bind {{ addr }}

# Build the production, server-only image locally.
docker-build image="lucy:local" version="0.1.0-dev":
    docker build --file docker/lucy/Dockerfile --target runtime --build-arg VERSION={{ version }} --build-arg REVISION={{ revision }} --tag {{ image }} .

# Exercise startup, health, tileset, subtree, and GLB responses.
docker-test:
    sh scripts/docker-smoke.sh

# Ensure releases use a Buildx driver that can publish multi-platform manifests.
docker-builder:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! docker buildx inspect '{{ buildx_builder }}' >/dev/null 2>&1; then
        docker buildx create --name '{{ buildx_builder }}' --driver docker-container
    fi
    docker buildx inspect --bootstrap '{{ buildx_builder }}' >/dev/null

# Publish immutable multi-platform version and commit tags. This does not move latest.
docker-publish version image=image_repository platforms="linux/amd64,linux/arm64": docker-builder
    docker buildx build --builder {{ buildx_builder }} --file docker/lucy/Dockerfile --target runtime --platform {{ platforms }} --build-arg VERSION={{ version }} --build-arg REVISION={{ revision }} --tag {{ image }}:{{ version }} --tag {{ image }}:sha-{{ revision }} --push .

# Stop services and delete volumes.
clean:
    {{ compose }} down --volumes --remove-orphans

# decode a glb in base64
decode-glb model_url="":
    curl -s {{ model_url }} | node -e "const fs=require('fs'); const chunks=[]; process.stdin.on('data',c=>chunks.push(c)); process.stdin.on('end',()=>{const b=Buffer.concat(chunks); const jsonLen=b.readUInt32LE(12); const jsonType=b.toString('ascii',16,20); if(jsonType!=='JSON') throw new Error('first chunk is not JSON'); const json=b.slice(20,20+jsonLen).toString('utf8').replace(/\0+$/,'').trim(); console.log(JSON.stringify(JSON.parse(json), null, 2));});"

preview-glb model_url="":
    open "https://sandbox.babylonjs.com/?asset={{ model_url }}"
