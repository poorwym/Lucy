set dotenv-load := true

compose := "docker compose"
db_user := env("POSTGRES_USER", "lucy")
db_name := env("POSTGRES_DB", "lucy")
poc_database_url := env("DATABASE_URL", "postgres://lucy:lucy@localhost:5432/lucy")
poc_addr := env("POC_ADDR", "127.0.0.1:8080")

default:
    just --list

# Start services in the background.
up:
    {{ compose }} up -d

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

# Run the Phase 0 POC HTTP server.
poc-server config="config/poc-sources.yaml" addr=poc_addr:
    DATABASE_URL={{ poc_database_url }} cargo run -p lucy-poc -- serve {{ config }} {{ addr }}

# Stop services and delete volumes.
clean:
    {{ compose }} down --volumes --remove-orphans

# decode a glb in base64
decode-glb url="":
    curl -s {{ url }} | node -e "const fs=require('fs'); const chunks=[]; process.stdin.on('data',c=>chunks.push(c)); process.stdin.on('end',()=>{const b=Buffer.concat(chunks); const jsonLen=b.readUInt32LE(12); const jsonType=b.toString('ascii',16,20); if(jsonType!=='JSON') throw new Error('first chunk is not JSON'); const json=b.slice(20,20+jsonLen).toString('utf8').replace(/\0+$/,'').trim(); console.log(JSON.stringify(JSON.parse(json), null, 2));});"
