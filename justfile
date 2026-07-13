set dotenv-load := true

compose := "docker compose"
db_user := env("POSTGRES_USER", "lucy")
db_name := env("POSTGRES_DB", "lucy")
database_url := env("DATABASE_URL", "postgres://lucy:lucy@localhost:5432/lucy")
poc_addr := env("POC_ADDR", "127.0.0.1:8080")
rdnap_pipeline := "+proj=pipeline +step +inv +proj=sterea +lat_0=52.1561605555556 +lon_0=5.38763888888889 +k=0.9999079 +x_0=155000 +y_0=463000 +ellps=bessel +step +proj=hgridshift +grids=nl_nsgi_rdtrans2018.tif +step +proj=vgridshift +grids=nl_nsgi_nlgeo2018.tif +multiplier=1 +step +proj=cart +ellps=GRS80 +step +proj=helmert +x=0 +y=0 +z=0 +step +inv +proj=cart +ellps=WGS84 +step +proj=unitconvert +xy_in=rad +xy_out=deg"

default:
    just --list

# Start services in the background.
up:
    {{ compose }} up -d --build

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

# Load the fixed Phase 0 POC PostGIS source.
load-poc-fixture:
    {{ compose }} exec -T postgres psql -U {{ db_user }} -d {{ db_name }} -f - < fixtures/postgis/poc_buildings.sql

# Load the synthetic EPSG:7415 PolygonZ/MultiPolygonZ source.
load-surface-fixture:
    {{ compose }} exec -T postgres psql -U {{ db_user }} -d {{ db_name }} -f - < fixtures/postgis/surface_buildings_7415.sql

# Load every deterministic PostGIS fixture used by the workspace.
load-fixtures: load-poc-fixture load-surface-fixture

# Fail if the pinned grids or explicit RDNAPTRANS2018 + EPSG:1149 pipeline drift.
verify-rdnap-grids:
    {{ compose }} exec -T postgres psql -U {{ db_user }} -d {{ db_name }} -Atc "WITH transformed AS (SELECT ST_TransformPipeline(ST_GeomFromEWKT('SRID=7415;POINT Z (121302 487371 2.68)'), '{{ rdnap_pipeline }}', 4979) AS geom) SELECT abs(ST_X(geom) - 4.892367035931109) < 0.0000001 AND abs(ST_Y(geom) - 52.37317920269912) < 0.0000001 AND abs(ST_Z(geom) - 45.66258579945144) < 0.05 FROM transformed" | grep -qx t

# Verify the fixture retains both geometry types and its interior ring.
verify-surface-fixture:
    {{ compose }} exec -T postgres psql -U {{ db_user }} -d {{ db_name }} -Atc "WITH polygons AS (SELECT (ST_Dump(geom)).geom AS geom FROM public.surface_buildings_7415) SELECT (SELECT count(*) FROM public.surface_buildings_7415), (SELECT count(*) FROM public.surface_buildings_7415 WHERE GeometryType(geom) = 'POLYGON'), (SELECT count(*) FROM public.surface_buildings_7415 WHERE GeometryType(geom) = 'MULTIPOLYGON'), (SELECT count(*) FROM polygons WHERE ST_NumInteriorRings(geom) > 0)" | grep -qx '2|1|1|1'

# Run server tests against PostGIS without silent database-test skips.
test-postgis: load-fixtures verify-rdnap-grids verify-surface-fixture
    DATABASE_URL={{ database_url }} cargo test -p lucy-server -- --include-ignored --nocapture

# Run the Phase 0 POC HTTP server.
poc-server config="config/poc-sources.yaml" addr=poc_addr:
    DATABASE_URL={{ database_url }} cargo run -p lucy-poc -- serve {{ config }} {{ addr }}

# Run only the deterministic fixture catalog (poc_buildings is the default).
fixture-server config="config/fixture-sources.yaml" addr=poc_addr:
    DATABASE_URL={{ database_url }} cargo run -p lucy-poc -- serve {{ config }} {{ addr }}

# Stop services and delete volumes.
clean:
    {{ compose }} down --volumes --remove-orphans

# decode a glb in base64
decode-glb model_url="":
    curl -s {{ model_url }} | node -e "const fs=require('fs'); const chunks=[]; process.stdin.on('data',c=>chunks.push(c)); process.stdin.on('end',()=>{const b=Buffer.concat(chunks); const jsonLen=b.readUInt32LE(12); const jsonType=b.toString('ascii',16,20); if(jsonType!=='JSON') throw new Error('first chunk is not JSON'); const json=b.slice(20,20+jsonLen).toString('utf8').replace(/\0+$/,'').trim(); console.log(JSON.stringify(JSON.parse(json), null, 2));});"

preview-glb model_url="":
    open "https://sandbox.babylonjs.com/?asset={{ model_url }}"
