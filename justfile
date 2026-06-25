set dotenv-load

compose := "docker compose"
db_user := env_var_or_default("POSTGRES_USER", "lucy")
db_name := env_var_or_default("POSTGRES_DB", "lucy")

default:
    just --list

# Start services in the background.
up:
    {{compose}} up -d

# Stop services without deleting volumes.
down:
    {{compose}} down

# Restart services.
restart:
    {{compose}} restart

# Show service status.
ps:
    {{compose}} ps

# Follow service logs.
logs service="":
    {{compose}} logs -f {{service}}

# Pull service images.
pull:
    {{compose}} pull

# Open a psql shell in the PostGIS container.
psql:
    {{compose}} exec postgres psql -U {{db_user}} -d {{db_name}}

# Load the fixed Phase 0 POC PostGIS source.
load-poc-fixture:
    {{compose}} exec -T postgres psql -U {{db_user}} -d {{db_name}} -f - < fixtures/postgis/poc_buildings.sql

# Stop services and delete volumes.
clean:
    {{compose}} down --volumes --remove-orphans
