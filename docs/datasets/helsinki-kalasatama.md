# Helsinki Kalasatama LoD2 dataset

## Selection

The imported source is the City of Helsinki's **Kalasatama Digital Twins**
CityGML 2.0 archive. It is a district-scale semantic 3D model rather than a
footprint extrusion: LoD2 roof, wall, and ground surfaces carry their own XYZ
vertices. The source is published under CC BY 4.0.

- Dataset catalog: <https://hri.fi/data/en_GB/dataset/helsingin-3d-kaupunkimalli>
- Pinned archive:
  <https://3d.hel.ninja/data/citygml/Helsinki3D_CityGML_Kalasatama_20190326.zip>
- Archive SHA-256:
  `ef6a787068b82642e5a0be5e20268e137075bb41fdbf0ec88619ad79926e2299`
- Archive / uncompressed GML: approximately 22 MiB / 256 MiB

The pinned GML contains 3,011 city objects and 2,980 buildings. Of those,
2,919 buildings have LoD2 geometry. The selected LoD2 inventory contains
79,822 source polygons. Twelve upstream zero-width wall polygons have only two
distinct XYZ positions and no renderable area, so the importer reports and
omits them. The remaining 79,810 polygons contain 454,444 non-closing-ring
vertices, including 307 polygons with 385 interior rings.

## Coordinate contract

The CityGML envelope declares EPSG:3879 and the Helsinki documentation states
that elevations use N2000:

- horizontal: ETRS-GK25 (EPSG:3879);
- vertical: N2000 gravity-related height (EPSG:3900).

The XML serializes coordinates in GIS order: easting is approximately
25,500,000 and northing is approximately 6,670,000. This differs from the
formal EPSG:3879 northing/easting axis order. Treating the first ordinate as
northing would place the model outside Finland. The import pipeline therefore
consumes `(easting, northing, N2000 height)` directly before applying the
inverse GK25 projection.

N2000 is not WGS 84 ellipsoidal height. The importer applies the official
`fi_nls_fin2023n2000.tif` geoid grid (EPSG transformation 10697, published
accuracy 0.014 m), then the EUREF-FIN-to-WGS84 zero-translation approximation
with declared 1 m datum accuracy. The resulting relation is explicitly stored
as EPSG:4979 XYZ. A pinned probe must transform:

```text
(25497750, 6676280, 2.68 N2000 m)
  -> (24.9594331545, 60.1993151098, 20.2747003 ellipsoidal m)
```

An unchanged height near 2.68 m indicates that the vertical grid was skipped.
The Docker image pins the grid by SHA-256 and keeps `PROJ_NETWORK=OFF`.

## Geometry normalization

Lucy intentionally enforces a strict native-surface contract. The source has
12 zero-area walls, two self-intersections, point-touching interior rings, and
several large faces with centimetre-level non-planarity. The importer does not
loosen Lucy's global tolerance or flatten the model to 2D.

For each non-degenerate source face, the importer:

1. selects the dominant XY, XZ, or YZ projection while all ordinates are still
   metres in the source frame;
2. applies structured `ST_MakeValid` only when GEOS rejects that projected
   polygon;
3. performs constrained Delaunay triangulation in the selected plane;
4. swaps ordinates back, retaining every triangle's source XYZ;
5. transforms all triangle vertices to EPSG:4979.

This produces 295,576 stored 3D triangles grouped into 2,919
`MULTIPOLYGON Z` features. The original polygon and vertex counts remain in
`source_polygon_count` and `source_vertex_count` for auditability.

## Reproduction

Start or rebuild PostGIS so that it contains the pinned Finnish grid, then
download, import, and verify the relation:

```sh
just up
just download-helsinki-kalasatama
just load-helsinki-kalasatama
just verify-fin2023n2000-grid
just verify-helsinki-kalasatama
```

The import atomically replaces `public.helsinki_kalasatama_lod2`. To use an
already downloaded archive:

```sh
just load-helsinki-kalasatama /path/to/Helsinki3D_CityGML_Kalasatama_20190326.zip
```

Run Lucy with the dedicated catalog:

```sh
just poc-server config/helsinki-kalasatama-lod2.yaml
```

The configured region is a safely outward-rounded EPSG:4979 extent:

```text
west/east:    24.9487997 / 25.0052481 degrees
south/north:  60.1648910 / 60.2045649 degrees
height:        5.51 / 154.12 metres ellipsoidal
```

## Verified result

The imported relation passed:

- the pinned archive and FIN2023N2000 checks;
- relation inventory, `MULTIPOLYGON Z`, dimension 3, and SRID 4979 checks;
- Lucy full source validation;
- all 64 level-3 content requests: 51 GLBs, 13 empty tiles, 0 failures;
- complete implicit-tiling materialization through level 7: 2,122 subtrees and
  8,888 GLBs, 310,118,796 content bytes, and 0 invalid GLBs according to the
  workspace GLB structural summarizer.

Full materialization is the evidence for enabling
`surface_subtree_envelope_shortcut`. Lucy's current `validate` command checks
the relation-wide type, finite-coordinate, transform, and bounds contracts but
does not by itself run the mesh topology path for every feature.

Attribution for derived displays or redistributed data should identify the
City of Helsinki / Helsinki Region Infoshare source and the CC BY 4.0 license.
