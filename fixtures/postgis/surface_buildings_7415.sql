-- Deterministic, synthetic native-surface fixture inspired by the 3DBAG data
-- model. No 3DBAG source geometry is copied into this file.
--
-- Coordinates use Amersfoort / RD New + NAP height (EPSG:7415). The first
-- feature exercises PolygonZ triangulation with an interior ring. The second
-- feature is a closed MultiPolygonZ shell with a roof, floor, and four
-- vertical walls.

CREATE EXTENSION IF NOT EXISTS postgis;

DROP TABLE IF EXISTS public.surface_buildings_7415;

CREATE TABLE public.surface_buildings_7415 (
    id bigint PRIMARY KEY,
    identificatie text NOT NULL UNIQUE,
    name text NOT NULL,
    surface_kind text NOT NULL,
    color text NOT NULL,
    geom geometry(GeometryZ, 7415) NOT NULL,
    CONSTRAINT surface_buildings_7415_geom_non_empty CHECK (NOT ST_IsEmpty(geom)),
    CONSTRAINT surface_buildings_7415_geom_srid CHECK (ST_SRID(geom) = 7415),
    CONSTRAINT surface_buildings_7415_geom_z CHECK (ST_NDims(geom) = 3),
    CONSTRAINT surface_buildings_7415_geom_type CHECK (
        GeometryType(geom) IN ('POLYGON', 'MULTIPOLYGON')
    )
);

-- Do not add ST_IsValid(geom) here. PostGIS/GEOS validity is evaluated in XY,
-- where a legitimate vertical PolygonZ wall collapses to a line and is
-- reported as invalid.

INSERT INTO public.surface_buildings_7415 (
    id,
    identificatie,
    name,
    surface_kind,
    color,
    geom
)
VALUES
    (
        1001,
        'synthetic-7415-courtyard-roof',
        'Courtyard roof',
        'roof_with_interior_ring',
        '#8aa1b1',
        ST_GeomFromEWKT(
            'SRID=7415;POLYGON Z (
                (
                    187590 316780 140,
                    187610 316780 140,
                    187610 316800 140,
                    187590 316800 140,
                    187590 316780 140
                ),
                (
                    187596 316786 140,
                    187596 316794 140,
                    187604 316794 140,
                    187604 316786 140,
                    187596 316786 140
                )
            )'
        )
    ),
    (
        1002,
        'synthetic-7415-closed-shell',
        'Closed surface shell',
        'roof_floor_vertical_walls',
        '#c29f75',
        ST_GeomFromEWKT(
            'SRID=7415;MULTIPOLYGON Z (
                ((
                    187620 316780 150,
                    187640 316780 150,
                    187640 316800 150,
                    187620 316800 150,
                    187620 316780 150
                )),
                ((
                    187620 316780 130,
                    187620 316800 130,
                    187640 316800 130,
                    187640 316780 130,
                    187620 316780 130
                )),
                ((
                    187620 316780 130,
                    187640 316780 130,
                    187640 316780 150,
                    187620 316780 150,
                    187620 316780 130
                )),
                ((
                    187640 316780 130,
                    187640 316800 130,
                    187640 316800 150,
                    187640 316780 150,
                    187640 316780 130
                )),
                ((
                    187640 316800 130,
                    187620 316800 130,
                    187620 316800 150,
                    187640 316800 150,
                    187640 316800 130
                )),
                ((
                    187620 316800 130,
                    187620 316780 130,
                    187620 316780 150,
                    187620 316800 150,
                    187620 316800 130
                ))
            )'
        )
    );

CREATE INDEX surface_buildings_7415_geom_gix
    ON public.surface_buildings_7415
    USING gist (geom);

COMMENT ON TABLE public.surface_buildings_7415 IS
    'Synthetic PolygonZ/MultiPolygonZ fixture in EPSG:7415 for Lucy native-surface tests';

ANALYZE public.surface_buildings_7415;
