#!/usr/bin/env python3
"""Summarize geometry, metadata, materials, transforms, and sampled normals in GLBs."""

from __future__ import annotations

import argparse
from collections import Counter
import json
import math
from pathlib import Path
import struct
import sys
from typing import Any, BinaryIO, Optional


JSON_CHUNK = 0x4E4F534A
BIN_CHUNK = 0x004E4942


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path, help="Tileset root containing GLB files")
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def read_glb(path: Path) -> tuple[dict[str, Any], int, int]:
    file_size = path.stat().st_size
    with path.open("rb") as stream:
        header = stream.read(12)
        if len(header) != 12:
            raise ValueError("file is shorter than the GLB header")
        magic, version, declared_length = struct.unpack("<4sII", header)
        if magic != b"glTF" or version != 2:
            raise ValueError(f"unsupported GLB magic={magic!r} version={version}")
        if declared_length != file_size:
            raise ValueError(
                f"declared byte length {declared_length} does not match {file_size}"
            )

        document: Optional[dict[str, Any]] = None
        binary_offset = -1
        binary_length = 0
        while stream.tell() < file_size:
            chunk_header = stream.read(8)
            if len(chunk_header) != 8:
                raise ValueError("truncated GLB chunk header")
            chunk_length, chunk_type = struct.unpack("<II", chunk_header)
            chunk_offset = stream.tell()
            if chunk_offset + chunk_length > file_size:
                raise ValueError("GLB chunk exceeds declared file length")
            if chunk_type == JSON_CHUNK:
                raw = stream.read(chunk_length).rstrip(b" \x00")
                document = json.loads(raw.decode("utf-8"))
            elif chunk_type == BIN_CHUNK:
                binary_offset = chunk_offset
                binary_length = chunk_length
                stream.seek(chunk_length, 1)
            else:
                stream.seek(chunk_length, 1)
        if document is None:
            raise ValueError("GLB has no JSON chunk")
        return document, binary_offset, binary_length


def accessor_count(document: dict[str, Any], index: int) -> int:
    return int(document["accessors"][index]["count"])


def read_buffer_view(
    stream: BinaryIO,
    document: dict[str, Any],
    binary_offset: int,
    view_index: int,
) -> bytes:
    if binary_offset < 0:
        raise ValueError("buffer view requires a missing GLB BIN chunk")
    view = document["bufferViews"][view_index]
    if view.get("buffer", 0) != 0:
        raise ValueError("buffer view references a non-GLB buffer")
    stream.seek(binary_offset + int(view.get("byteOffset", 0)))
    body = stream.read(int(view["byteLength"]))
    if len(body) != int(view["byteLength"]):
        raise ValueError("buffer view exceeds the GLB")
    return body


def decode_string_property(
    stream: BinaryIO,
    document: dict[str, Any],
    binary_offset: int,
    descriptor: dict[str, Any],
    count: int,
) -> list[str]:
    offset_types = {
        "UINT8": ("B", 1),
        "UINT16": ("H", 2),
        "UINT32": ("I", 4),
        "UINT64": ("Q", 8),
    }
    offset_type = descriptor.get("stringOffsetType", "UINT32")
    if offset_type not in offset_types:
        raise ValueError(f"unsupported stringOffsetType {offset_type}")
    code, byte_width = offset_types[offset_type]
    raw_offsets = read_buffer_view(
        stream, document, binary_offset, descriptor["stringOffsets"]
    )
    required_bytes = (count + 1) * byte_width
    if len(raw_offsets) < required_bytes:
        raise ValueError("string offset buffer view is shorter than expected")
    offsets = [
        value[0]
        for value in struct.iter_unpack(
            "<" + code, raw_offsets[:required_bytes]
        )
    ]
    raw_values = read_buffer_view(
        stream, document, binary_offset, descriptor["values"]
    )
    if offsets[-1] > len(raw_values):
        raise ValueError("string value offset exceeds its buffer view")
    return [
        raw_values[offsets[index] : offsets[index + 1]].decode("utf-8")
        for index in range(count)
    ]


def sample_normal_lengths(
    stream: BinaryIO,
    document: dict[str, Any],
    binary_offset: int,
    accessor_index: int,
) -> list[float]:
    accessor = document["accessors"][accessor_index]
    if (
        accessor.get("componentType") != 5126
        or accessor.get("type") != "VEC3"
        or "bufferView" not in accessor
        or binary_offset < 0
    ):
        raise ValueError("NORMAL accessor is not a non-sparse FLOAT VEC3")
    view = document["bufferViews"][accessor["bufferView"]]
    if view.get("buffer", 0) != 0:
        raise ValueError("NORMAL accessor references a non-GLB buffer")
    count = int(accessor["count"])
    if count == 0:
        return []
    stride = int(view.get("byteStride", 12))
    base = (
        binary_offset
        + int(view.get("byteOffset", 0))
        + int(accessor.get("byteOffset", 0))
    )
    sample_indices = sorted({0, count // 2, count - 1})
    lengths = []
    for sample_index in sample_indices:
        stream.seek(base + sample_index * stride)
        raw = stream.read(12)
        if len(raw) != 12:
            raise ValueError("NORMAL accessor sample exceeds the GLB")
        x, y, z = struct.unpack("<fff", raw)
        lengths.append(math.sqrt(x * x + y * y + z * z))
    return lengths


def summarize(root: Path) -> dict[str, Any]:
    paths = sorted(root.rglob("*.glb"))
    if not paths:
        raise ValueError(f"no GLB files found below {root}")

    file_bytes = 0
    file_sizes = []
    invalid_glb_count = 0
    invalid_files = []
    generators: Counter[str] = Counter()
    extensions_used: Counter[str] = Counter()
    extensions_required: Counter[str] = Counter()
    primitive_attributes: Counter[str] = Counter()
    base_colors: Counter[str] = Counter()
    double_sided: Counter[str] = Counter()
    alpha_modes: Counter[str] = Counter()
    metadata_properties: Counter[str] = Counter()
    metadata_classes: Counter[str] = Counter()
    metadata_decoded_rows: Counter[str] = Counter()
    metadata_unique_strings: dict[str, set[str]] = {}
    mesh_count = 0
    primitive_count = 0
    triangle_count = 0
    position_count = 0
    normal_count = 0
    normal_primitive_count = 0
    normal_sample_count = 0
    normal_min = math.inf
    normal_max = -math.inf
    feature_id_primitive_count = 0
    property_table_count = 0
    property_table_rows = 0
    node_matrix_count = 0
    node_trs_count = 0
    node_matrices = set()
    sample_node_matrix = None

    for path in paths:
        size = path.stat().st_size
        file_bytes += size
        file_sizes.append(size)
        try:
            document, binary_offset, _ = read_glb(path)
            generator = document.get("asset", {}).get("generator", "<missing>")
            generators[str(generator)] += 1
            for extension in document.get("extensionsUsed", []):
                extensions_used[extension] += 1
            for extension in document.get("extensionsRequired", []):
                extensions_required[extension] += 1

            for material in document.get("materials", []):
                double_sided[str(material.get("doubleSided", False)).lower()] += 1
                alpha_modes[str(material.get("alphaMode", "OPAQUE"))] += 1
                color = material.get("pbrMetallicRoughness", {}).get(
                    "baseColorFactor", [1.0, 1.0, 1.0, 1.0]
                )
                base_colors[json.dumps(color, separators=(",", ":"))] += 1

            for node in document.get("nodes", []):
                if "matrix" in node:
                    node_matrix_count += 1
                    matrix = tuple(node["matrix"])
                    node_matrices.add(matrix)
                    if sample_node_matrix is None:
                        sample_node_matrix = list(matrix)
                if any(field in node for field in ("translation", "rotation", "scale")):
                    node_trs_count += 1

            structural = document.get("extensions", {}).get(
                "EXT_structural_metadata", {}
            )
            schema = structural.get("schema", {})
            for class_name, class_value in schema.get("classes", {}).items():
                metadata_classes[class_name] += 1
                for property_name in class_value.get("properties", {}):
                    metadata_properties[property_name] += 1
            for table in structural.get("propertyTables", []):
                property_table_count += 1
                property_table_rows += int(table.get("count", 0))

            meshes = document.get("meshes", [])
            mesh_count += len(meshes)
            sampled_accessors = set()
            with path.open("rb") as stream:
                classes = schema.get("classes", {})
                for table in structural.get("propertyTables", []):
                    table_count = int(table.get("count", 0))
                    class_properties = classes.get(table.get("class"), {}).get(
                        "properties", {}
                    )
                    for property_name, descriptor in table.get(
                        "properties", {}
                    ).items():
                        if class_properties.get(property_name, {}).get("type") != "STRING":
                            continue
                        values = decode_string_property(
                            stream,
                            document,
                            binary_offset,
                            descriptor,
                            table_count,
                        )
                        metadata_decoded_rows[property_name] += len(values)
                        metadata_unique_strings.setdefault(property_name, set()).update(
                            values
                        )
                for mesh in meshes:
                    for primitive in mesh.get("primitives", []):
                        primitive_count += 1
                        for attribute_name in primitive.get("attributes", {}):
                            primitive_attributes[attribute_name] += 1
                        if primitive.get("mode", 4) == 4:
                            if "indices" in primitive:
                                indices = accessor_count(document, primitive["indices"])
                            else:
                                indices = accessor_count(
                                    document, primitive["attributes"]["POSITION"]
                                )
                            if indices % 3 != 0:
                                raise ValueError("triangle primitive index count is not / 3")
                            triangle_count += indices // 3
                        position_count += accessor_count(
                            document, primitive["attributes"]["POSITION"]
                        )
                        normal_accessor = primitive.get("attributes", {}).get("NORMAL")
                        if normal_accessor is not None:
                            normal_primitive_count += 1
                            normal_count += accessor_count(document, normal_accessor)
                            if normal_accessor not in sampled_accessors:
                                sampled_accessors.add(normal_accessor)
                                lengths = sample_normal_lengths(
                                    stream,
                                    document,
                                    binary_offset,
                                    normal_accessor,
                                )
                                normal_sample_count += len(lengths)
                                if lengths:
                                    normal_min = min(normal_min, min(lengths))
                                    normal_max = max(normal_max, max(lengths))
                        if "EXT_mesh_features" in primitive.get("extensions", {}):
                            feature_id_primitive_count += 1
        except Exception as error:  # noqa: BLE001 - collect per-file diagnostics.
            invalid_glb_count += 1
            if len(invalid_files) < 20:
                invalid_files.append({"path": str(path), "error": str(error)})

    return {
        "root": str(root),
        "glb_files": len(paths),
        "glb_bytes": file_bytes,
        "min_glb_bytes": min(file_sizes),
        "max_glb_bytes": max(file_sizes),
        "invalid_glb_count": invalid_glb_count,
        "invalid_glbs": invalid_files,
        "generators": dict(generators),
        "extensions_used_file_counts": dict(extensions_used),
        "extensions_required_file_counts": dict(extensions_required),
        "primitive_attribute_counts": dict(primitive_attributes),
        "meshes": mesh_count,
        "primitives": primitive_count,
        "triangles": triangle_count,
        "positions": position_count,
        "normal_primitives": normal_primitive_count,
        "normals": normal_count,
        "sampled_normals": normal_sample_count,
        "sampled_normal_length_min": normal_min if normal_sample_count else None,
        "sampled_normal_length_max": normal_max if normal_sample_count else None,
        "sampled_normal_max_abs_unit_error": (
            max(abs(normal_min - 1.0), abs(normal_max - 1.0))
            if normal_sample_count
            else None
        ),
        "materials": sum(double_sided.values()),
        "material_double_sided_counts": dict(double_sided),
        "material_alpha_mode_counts": dict(alpha_modes),
        "material_base_color_counts": dict(base_colors),
        "feature_id_primitives": feature_id_primitive_count,
        "property_tables": property_table_count,
        "property_table_rows": property_table_rows,
        "metadata_class_file_counts": dict(metadata_classes),
        "metadata_property_file_counts": dict(metadata_properties),
        "metadata_decoded_string_rows": dict(metadata_decoded_rows),
        "metadata_unique_string_counts": {
            name: len(values) for name, values in metadata_unique_strings.items()
        },
        "node_matrices": node_matrix_count,
        "unique_node_matrices": len(node_matrices),
        "sample_node_matrix": sample_node_matrix,
        "node_trs": node_trs_count,
    }


def main() -> int:
    args = parse_args()
    try:
        result = summarize(args.root)
    except Exception as error:  # noqa: BLE001 - CLI should report context.
        print(f"summarize_glbs.py: {error}", file=sys.stderr)
        return 1
    output = json.dumps(result, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(output, encoding="utf-8")
    print(output, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
