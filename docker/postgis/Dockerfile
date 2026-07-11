FROM postgis/postgis:18-3.6

# RDNAPTRANS2018 resources published through the PROJ datum-grid CDN. The
# files are CC BY 4.0; see https://cdn.proj.org/nl_nsgi_README.txt.
ARG RDNAP_HORIZONTAL_GRID_SHA256=7653831191b424e715a906468962fc60071cfb71a3186b2a58f098bab8bf41de
ARG RDNAP_VERTICAL_GRID_SHA256=f8e32c56bf8940fc3fefbc0e413eb4546633ed598b669c63856cddf8328992c0

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && curl --fail --show-error --silent --location \
        --output /usr/share/proj/nl_nsgi_rdtrans2018.tif \
        https://cdn.proj.org/nl_nsgi_rdtrans2018.tif \
    && curl --fail --show-error --silent --location \
        --output /usr/share/proj/nl_nsgi_nlgeo2018.tif \
        https://cdn.proj.org/nl_nsgi_nlgeo2018.tif \
    && echo "${RDNAP_HORIZONTAL_GRID_SHA256}  /usr/share/proj/nl_nsgi_rdtrans2018.tif" \
        | sha256sum --check --strict \
    && echo "${RDNAP_VERTICAL_GRID_SHA256}  /usr/share/proj/nl_nsgi_nlgeo2018.tif" \
        | sha256sum --check --strict \
    && chmod 0644 \
        /usr/share/proj/nl_nsgi_rdtrans2018.tif \
        /usr/share/proj/nl_nsgi_nlgeo2018.tif \
    && apt-get purge -y --auto-remove curl \
    && rm -rf /var/lib/apt/lists/*

# Production and tests use the image-baked, checksum-pinned grids. Runtime
# network fallback would make a missing-grid deployment silently non-reproducible.
ENV PROJ_NETWORK=OFF
