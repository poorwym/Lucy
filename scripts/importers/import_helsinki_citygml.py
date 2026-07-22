#!/usr/bin/env python3
"""Import Helsinki Kalasatama LoD2 CityGML surfaces into PostGIS.

The source stores XYZ coordinates as GIS-order ETRS-GK25 easting/northing
(EPSG:3879) plus N2000 gravity-related height (EPSG:3900).  Lucy's native
surface contract expects EPSG:4979, so the database import applies the pinned
FIN2023N2000 geoid grid before publishing the target relation.

Only polygons below ``bldg:lod2MultiSurface`` are selected.  LoD1 solids and
appearance/texture coordinates are intentionally ignored.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import xml.etree.ElementTree as ET
from typing import BinaryIO, Iterator, NamedTuple, Optional, Sequence, TextIO, Tuple
import zipfile


CITYGML_NS = "http://www.opengis.net/citygml/2.0"
BUILDING_NS = "http://www.opengis.net/citygml/building/2.0"
GML_NS = "http://www.opengis.net/gml"

CITY_OBJECT_MEMBER = f"{{{CITYGML_NS}}}cityObjectMember"
BUILDING = f"{{{BUILDING_NS}}}Building"
LOD2_MULTI_SURFACE = f"{{{BUILDING_NS}}}lod2MultiSurface"
POLYGON = f"{{{GML_NS}}}Polygon"
EXTERIOR = f"{{{GML_NS}}}exterior"
INTERIOR = f"{{{GML_NS}}}interior"
LINEAR_RING = f"{{{GML_NS}}}LinearRing"
POS_LIST = f"{{{GML_NS}}}posList"
GML_ID = f"{{{GML_NS}}}id"

IDENTIFIER_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
DEFAULT_SOURCE_URL = (
    "https://3d.hel.ninja/data/citygml/"
    "Helsinki3D_CityGML_Kalasatama_20190326.zip"
)
DEFAULT_ARCHIVE_SHA256 = (
    "ef6a787068b82642e5a0be5e20268e137075bb41fdbf0ec88619ad79926e2299"
)
DEFAULT_TARGET = "public.helsinki_kalasatama_lod2"

# CityGML writes GIS-order (easting, northing, height), despite EPSG:3879's
# formal northing/easting axis order.  The explicit pipeline therefore starts
# directly with the inverse projection and does not apply an axis swap.
N2000_TO_WGS84_3D_PIPELINE = " ".join(
    (
        "+proj=pipeline",
        "+step +inv +proj=tmerc +lat_0=0 +lon_0=25 +k=1",
        "+x_0=25500000 +y_0=0 +ellps=GRS80",
        "+step +proj=vgridshift +grids=fi_nls_fin2023n2000.tif +multiplier=1",
        "+step +proj=cart +ellps=GRS80",
        "+step +proj=helmert +x=0 +y=0 +z=0",
        "+step +inv +proj=cart +ellps=WGS84",
        "+step +proj=unitconvert +xy_in=rad +xy_out=deg",
    )
)


class ImportFeature(NamedTuple):
    gml_id: str
    geometry_ewkt: str
    polygon_count: int
    vertex_count: int
    measured_height_m: Optional[float]
    roof_type: Optional[str]
    creation_date: Optional[str]


class DegenerateRingError(ValueError):
    """A source ring has no renderable three-dimensional polygon area."""


class ImportStats:
    def __init__(self) -> None:
        self.city_objects = 0
        self.buildings = 0
        self.imported_buildings = 0
        self.skipped_without_lod2 = 0
        self.polygons = 0
        self.rings = 0
        self.interior_rings = 0
        self.holed_polygons = 0
        self.vertices = 0
        self.rings_closed_by_importer = 0
        self.skipped_degenerate_polygons = 0

    def summary(self) -> str:
        return (
            f"city_objects={self.city_objects} buildings={self.buildings} "
            f"imported_buildings={self.imported_buildings} "
            f"skipped_without_lod2={self.skipped_without_lod2} "
            f"polygons={self.polygons} rings={self.rings} "
            f"holed_polygons={self.holed_polygons} "
            f"interior_rings={self.interior_rings} vertices={self.vertices} "
            f"rings_closed_by_importer={self.rings_closed_by_importer} "
            f"skipped_degenerate_polygons={self.skipped_degenerate_polygons}"
        )


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("citygml", type=Path, help="CityGML .gml file or one-file .zip archive")
    parser.add_argument(
        "--database-url",
        default=os.environ.get("DATABASE_URL"),
        help="PostgreSQL URL; defaults to DATABASE_URL",
    )
    parser.add_argument(
        "--target",
        default=DEFAULT_TARGET,
        help=f"schema-qualified target relation (default: {DEFAULT_TARGET})",
    )
    parser.add_argument("--psql", default="psql", help="psql executable (default: psql)")
    parser.add_argument(
        "--replace",
        action="store_true",
        help="atomically replace the target relation if it already exists",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="parse and validate the complete CityGML without connecting to PostgreSQL",
    )
    parser.add_argument("--source-url", default=DEFAULT_SOURCE_URL)
    parser.add_argument(
        "--archive-sha256",
        default=None,
        help="expected input SHA-256; the official archive hash is checked by default",
    )
    return parser.parse_args(argv)


def split_target(value: str) -> Tuple[str, str]:
    parts = value.split(".")
    if len(parts) != 2 or any(IDENTIFIER_RE.fullmatch(part) is None for part in parts):
        raise ValueError("--target must be a simple schema-qualified SQL identifier")
    return parts[0], parts[1]


def quote_identifier(value: str) -> str:
    if IDENTIFIER_RE.fullmatch(value) is None:
        raise ValueError(f"unsafe SQL identifier: {value!r}")
    return f'"{value}"'


def sql_literal(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


@contextlib.contextmanager
def open_citygml(path: Path) -> Iterator[BinaryIO]:
    if zipfile.is_zipfile(path):
        with zipfile.ZipFile(path) as archive:
            members = [name for name in archive.namelist() if name.lower().endswith(".gml")]
            if len(members) != 1:
                raise ValueError(
                    f"expected exactly one .gml member in {path}, found {len(members)}"
                )
            with archive.open(members[0], "r") as source:
                yield source
    else:
        with path.open("rb") as source:
            yield source


def optional_text(parent: ET.Element, path: str) -> Optional[str]:
    element = parent.find(path)
    if element is None or element.text is None:
        return None
    value = element.text.strip()
    return value or None


def optional_float(parent: ET.Element, path: str) -> Optional[float]:
    value = optional_text(parent, path)
    if value is None:
        return None
    parsed = float(value)
    if not math.isfinite(parsed):
        raise ValueError(f"non-finite numeric attribute {value!r}")
    return parsed


def parse_ring(ring_property: ET.Element, stats: ImportStats) -> Tuple[str, int]:
    ring = ring_property.find(f"./{LINEAR_RING}")
    if ring is None:
        raise ValueError("polygon ring does not contain gml:LinearRing")
    pos_list = ring.find(f"./{POS_LIST}")
    if pos_list is None or pos_list.text is None:
        raise ValueError("gml:LinearRing does not contain a populated gml:posList")

    dimension_text = pos_list.get("srsDimension", "3")
    if dimension_text != "3":
        raise ValueError(f"expected srsDimension=3, got {dimension_text!r}")

    ordinates = [float(value) for value in pos_list.text.split()]
    if len(ordinates) % 3 != 0:
        raise ValueError(f"posList has {len(ordinates)} ordinates, not XYZ triples")
    points = [tuple(ordinates[index : index + 3]) for index in range(0, len(ordinates), 3)]
    if any(not all(math.isfinite(value) for value in point) for point in points):
        raise ValueError("posList contains a non-finite XYZ coordinate")

    deduplicated = [points[0]] if points else []
    for point in points[1:]:
        if point != deduplicated[-1]:
            deduplicated.append(point)
    points = deduplicated
    if points and points[0] != points[-1]:
        points.append(points[0])
        stats.rings_closed_by_importer += 1
    if len(points) < 4 or len(set(points[:-1])) < 3:
        raise DegenerateRingError("polygon ring has fewer than three distinct XYZ vertices")

    stats.rings += 1
    stats.vertices += len(points) - 1
    coordinates = ",".join(" ".join(format(value, ".17g") for value in point) for point in points)
    return f"({coordinates})", len(points) - 1


def parse_polygon(polygon: ET.Element, stats: ImportStats) -> Tuple[str, int]:
    exterior = polygon.find(f"./{EXTERIOR}")
    if exterior is None:
        raise ValueError("gml:Polygon is missing its exterior ring")
    rings = []
    exterior_wkt, vertex_count = parse_ring(exterior, stats)
    rings.append(exterior_wkt)
    interiors = polygon.findall(f"./{INTERIOR}")
    if interiors:
        stats.holed_polygons += 1
    for interior in interiors:
        interior_wkt, interior_vertices = parse_ring(interior, stats)
        rings.append(interior_wkt)
        vertex_count += interior_vertices
        stats.interior_rings += 1
    return f"({','.join(rings)})", vertex_count


def iter_features(source: BinaryIO, stats: ImportStats) -> Iterator[ImportFeature]:
    for _event, member in ET.iterparse(source, events=("end",)):
        if member.tag != CITY_OBJECT_MEMBER:
            continue
        stats.city_objects += 1
        building = member.find(f"./{BUILDING}")
        if building is None:
            member.clear()
            continue
        stats.buildings += 1

        gml_id = building.get(GML_ID)
        if not gml_id:
            raise ValueError("bldg:Building is missing gml:id")

        polygons = []
        polygon_ids = set()
        vertex_count = 0
        for lod2_surface in building.iter(LOD2_MULTI_SURFACE):
            for polygon in lod2_surface.iter(POLYGON):
                polygon_id = polygon.get(GML_ID)
                if polygon_id and polygon_id in polygon_ids:
                    continue
                if polygon_id:
                    polygon_ids.add(polygon_id)
                try:
                    polygon_wkt, polygon_vertices = parse_polygon(polygon, stats)
                except DegenerateRingError:
                    # The upstream export contains a small number of zero-width
                    # wall faces. They have no renderable area, so omitting them
                    # preserves the visible LoD2 surface without inventing XYZ.
                    stats.skipped_degenerate_polygons += 1
                    continue
                except ValueError as error:
                    raise ValueError(
                        f"building {gml_id!r}, polygon {polygon_id!r}: {error}"
                    ) from error
                polygons.append(polygon_wkt)
                vertex_count += polygon_vertices

        if not polygons:
            stats.skipped_without_lod2 += 1
            member.clear()
            continue

        measured_height = optional_float(building, f"./{{{BUILDING_NS}}}measuredHeight")
        roof_type = optional_text(building, f"./{{{BUILDING_NS}}}roofType")
        creation_date = optional_text(building, f"./{{{CITYGML_NS}}}creationDate")
        geometry = f"SRID=3879;MULTIPOLYGON Z ({','.join(polygons)})"

        stats.imported_buildings += 1
        stats.polygons += len(polygons)
        yield ImportFeature(
            gml_id=gml_id,
            geometry_ewkt=geometry,
            polygon_count=len(polygons),
            vertex_count=vertex_count,
            measured_height_m=measured_height,
            roof_type=roof_type,
            creation_date=creation_date,
        )
        member.clear()


def copy_text(value: Optional[object]) -> str:
    if value is None:
        return r"\N"
    text = str(value)
    return text.replace("\\", "\\\\").replace("\t", r"\t").replace("\n", r"\n")


def write_psql_header(stream: TextIO) -> None:
    stream.write("\\set ON_ERROR_STOP on\nBEGIN;\n")
    stream.write(
        "CREATE TEMP TABLE lucy_helsinki_citygml_import (\n"
        "  gml_id text NOT NULL,\n"
        "  geom geometry(MultiPolygonZ, 3879) NOT NULL,\n"
        "  polygon_count integer NOT NULL,\n"
        "  vertex_count integer NOT NULL,\n"
        "  measured_height_m double precision,\n"
        "  roof_type text,\n"
        "  creation_date date\n"
        ") ON COMMIT DROP;\n"
        "COPY lucy_helsinki_citygml_import "
        "(gml_id, geom, polygon_count, vertex_count, measured_height_m, roof_type, creation_date) "
        "FROM STDIN WITH (FORMAT text, DELIMITER E'\\t', NULL '\\N');\n"
    )


def write_psql_footer(
    stream: TextIO,
    schema: str,
    table: str,
    replace: bool,
    source_url: str,
    source_sha256: str,
) -> None:
    qualified = f"{quote_identifier(schema)}.{quote_identifier(table)}"
    stream.write("\\.\n")
    if replace:
        stream.write(f"DROP TABLE IF EXISTS {qualified};\n")
    stream.write(
        f"CREATE TABLE {qualified} AS\n"
        "WITH source_faces AS (\n"
        "  SELECT imported.gml_id, dumped.geom\n"
        "  FROM lucy_helsinki_citygml_import AS imported\n"
        "  CROSS JOIN LATERAL ST_Dump(imported.geom) AS dumped\n"
        "), projected_faces AS (\n"
        "  SELECT gml_id, geom,\n"
        "    ST_Area(geom) AS xy_area,\n"
        "    ST_Area(ST_SwapOrdinates(geom, 'xz')) AS zy_area,\n"
        "    ST_Area(ST_SwapOrdinates(geom, 'yz')) AS xz_area\n"
        "  FROM source_faces\n"
        "), selected_projection AS (\n"
        "  SELECT gml_id,\n"
        "    CASE\n"
        "      WHEN xy_area >= zy_area AND xy_area >= xz_area THEN geom\n"
        "      WHEN zy_area >= xz_area THEN ST_SwapOrdinates(geom, 'xz')\n"
        "      ELSE ST_SwapOrdinates(geom, 'yz')\n"
        "    END AS geom,\n"
        "    CASE\n"
        "      WHEN xy_area >= zy_area AND xy_area >= xz_area THEN 'xy'\n"
        "      WHEN zy_area >= xz_area THEN 'zy'\n"
        "      ELSE 'xz'\n"
        "    END AS projection\n"
        "  FROM projected_faces\n"
        "), valid_parts AS (\n"
        "  SELECT projected.gml_id, projected.projection, parts.geom\n"
        "  FROM selected_projection AS projected\n"
        "  CROSS JOIN LATERAL ST_Dump(ST_CollectionExtract(\n"
        "    CASE WHEN ST_IsValid(projected.geom) THEN projected.geom\n"
        "      ELSE ST_MakeValid(projected.geom, 'method=structure keepcollapsed=false')\n"
        "    END, 3\n"
        "  )) AS parts\n"
        "), triangulated AS (\n"
        "  SELECT gml_id, projection, ST_TriangulatePolygon(geom) AS geom\n"
        "  FROM valid_parts\n"
        "), source_triangles AS (\n"
        "  SELECT gml_id,\n"
        "    CASE projection\n"
        "      WHEN 'xy' THEN geom\n"
        "      WHEN 'zy' THEN ST_SwapOrdinates(geom, 'xz')\n"
        "      ELSE ST_SwapOrdinates(geom, 'yz')\n"
        "    END AS geom\n"
        "  FROM triangulated\n"
        "), rebuilt AS (\n"
        "  SELECT gml_id,\n"
        "    ST_Multi(ST_CollectionExtract(ST_Collect(geom), 3))"
        "::geometry(MultiPolygonZ, 3879) AS geom\n"
        "  FROM source_triangles\n"
        "  GROUP BY gml_id\n"
        ")\n"
        "SELECT\n"
        "  imported.gml_id,\n"
        f"  ST_TransformPipeline(rebuilt.geom, {sql_literal(N2000_TO_WGS84_3D_PIPELINE)}, 4979)"
        "::geometry(MultiPolygonZ, 4979) AS geom,\n"
        "  imported.polygon_count AS source_polygon_count,\n"
        "  imported.vertex_count AS source_vertex_count,\n"
        "  ST_NumGeometries(rebuilt.geom)::integer AS polygon_count,\n"
        "  imported.measured_height_m,\n"
        "  imported.roof_type,\n"
        "  imported.creation_date\n"
        "FROM lucy_helsinki_citygml_import AS imported\n"
        "JOIN rebuilt USING (gml_id);\n"
        f"ALTER TABLE {qualified} ADD PRIMARY KEY (gml_id);\n"
        f"ALTER TABLE {qualified} ALTER COLUMN geom SET NOT NULL;\n"
        f"ALTER TABLE {qualified} ADD CONSTRAINT {quote_identifier(table + '_geom_nonempty')} "
        "CHECK (NOT ST_IsEmpty(geom));\n"
        f"CREATE INDEX {quote_identifier(table + '_geom_gix')} ON {qualified} USING gist (geom);\n"
        f"ANALYZE {qualified};\n"
    )
    comment = (
        "Helsinki Kalasatama Digital Twins LoD2 CityGML building surfaces; "
        f"source={source_url}; sha256={source_sha256}; license=CC BY 4.0; "
        "source CRS=ETRS-GK25 GIS-order easting/northing (EPSG:3879) + N2000 height "
        "(EPSG:3900); stored CRS=EPSG:4979 after FIN2023N2000 geoid conversion and "
        "EUREF-FIN-to-WGS84 zero-translation approximation (declared 1m accuracy); "
        "each source face is projected to its dominant XYZ plane, conditionally made valid, "
        "and constrained-Delaunay triangulated before the CRS transformation. This preserves "
        "true XYZ while normalizing point-touching holes, self-intersections, and centimetre-level "
        "non-planarity rejected by Lucy's strict surface topology contract."
    )
    stream.write(f"COMMENT ON TABLE {qualified} IS {sql_literal(comment)};\nCOMMIT;\n")


def relation_exists(psql: str, database_url: str, schema: str, table: str) -> bool:
    relation = f"{schema}.{table}"
    result = subprocess.run(
        [
            psql,
            "-X",
            "-At",
            "-v",
            "ON_ERROR_STOP=1",
            database_url,
            "-c",
            f"SELECT to_regclass({sql_literal(relation)}) IS NOT NULL",
        ],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return result.stdout.strip() == "t"


def verify_coordinate_pipeline(psql: str, database_url: str) -> None:
    sql = (
        "WITH transformed AS (SELECT ST_TransformPipeline("
        "ST_SetSRID(ST_MakePoint(25497750, 6676280, 2.68), 3879), "
        f"{sql_literal(N2000_TO_WGS84_3D_PIPELINE)}, 4979) AS geom) "
        "SELECT ST_X(geom), ST_Y(geom), ST_Z(geom) FROM transformed"
    )
    result = subprocess.run(
        [psql, "-X", "-At", "-F", "|", "-v", "ON_ERROR_STOP=1", database_url, "-c", sql],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    values = [float(value) for value in result.stdout.strip().split("|")]
    if len(values) != 3 or not all(math.isfinite(value) for value in values):
        raise RuntimeError(f"coordinate pipeline returned invalid result: {result.stdout!r}")
    longitude, latitude, ellipsoidal_height = values
    if not (24.8 < longitude < 25.2 and 60.0 < latitude < 60.4):
        raise RuntimeError(
            "coordinate pipeline axis-order check failed: "
            f"got ({longitude}, {latitude}, {ellipsoidal_height})"
        )
    if ellipsoidal_height - 2.68 < 10.0:
        raise RuntimeError(
            "vertical grid check failed: transformed height did not receive "
            "the N2000 geoid correction"
        )
    print(
        "coordinate_pipeline_probe="
        f"{longitude:.10f},{latitude:.10f},{ellipsoidal_height:.4f}",
        file=sys.stderr,
    )


def run_import(
    args: argparse.Namespace,
    source_sha256: str,
    schema: str,
    table: str,
) -> ImportStats:
    if not args.database_url:
        raise ValueError("--database-url or DATABASE_URL is required unless --dry-run is used")
    if relation_exists(args.psql, args.database_url, schema, table) and not args.replace:
        raise ValueError(
            f"target relation {schema}.{table} already exists; "
            "pass --replace to replace it atomically"
        )
    verify_coordinate_pipeline(args.psql, args.database_url)

    process = subprocess.Popen(
        [args.psql, "-X", "-v", "ON_ERROR_STOP=1", args.database_url],
        stdin=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    if process.stdin is None:
        raise RuntimeError("failed to open psql stdin")

    stats = ImportStats()
    try:
        write_psql_header(process.stdin)
        with open_citygml(args.citygml) as source:
            for feature in iter_features(source, stats):
                row = (
                    feature.gml_id,
                    feature.geometry_ewkt,
                    feature.polygon_count,
                    feature.vertex_count,
                    feature.measured_height_m,
                    feature.roof_type,
                    feature.creation_date,
                )
                process.stdin.write("\t".join(copy_text(value) for value in row) + "\n")
        write_psql_footer(
            process.stdin,
            schema,
            table,
            args.replace,
            args.source_url,
            source_sha256,
        )
        process.stdin.close()
        return_code = process.wait()
    except BaseException:
        process.stdin.close()
        process.wait()
        raise
    if return_code != 0:
        raise RuntimeError(f"psql import failed with exit code {return_code}")
    return stats


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    if not args.citygml.is_file():
        raise ValueError(f"CityGML input does not exist: {args.citygml}")
    schema, table = split_target(args.target)

    source_sha256 = sha256_file(args.citygml)
    expected_sha256 = args.archive_sha256
    if expected_sha256 is None and args.citygml.suffix.lower() == ".zip":
        expected_sha256 = DEFAULT_ARCHIVE_SHA256
    if expected_sha256 and source_sha256.lower() != expected_sha256.lower():
        raise ValueError(
            f"input SHA-256 mismatch: expected {expected_sha256}, got {source_sha256}"
        )
    print(f"source_sha256={source_sha256}", file=sys.stderr)

    if args.dry_run:
        stats = ImportStats()
        with open_citygml(args.citygml) as source:
            for _feature in iter_features(source, stats):
                pass
    else:
        stats = run_import(args, source_sha256, schema, table)
    print(stats.summary(), file=sys.stderr)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
