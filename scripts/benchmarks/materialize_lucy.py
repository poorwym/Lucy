#!/usr/bin/env python3
"""Cold-start Lucy and materialize every available implicit tile over HTTP."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import sys
import time
from typing import Any, Iterable, Optional
from urllib.error import HTTPError, URLError
from urllib.parse import urljoin
from urllib.request import urlopen


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-binary", type=Path, required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--address", default="127.0.0.1:18080")
    parser.add_argument("--source", default="nl_lod12_3d")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--metrics-json", type=Path, required=True)
    parser.add_argument("--server-log", type=Path, required=True)
    parser.add_argument("--concurrency", type=int, default=8)
    parser.add_argument("--request-timeout", type=float, default=300.0)
    parser.add_argument("--startup-timeout", type=float, default=60.0)
    args = parser.parse_args()
    if args.concurrency < 1:
        parser.error("--concurrency must be positive")
    return args


def request_bytes(url: str, timeout: float) -> tuple[bytes, float]:
    started = time.perf_counter()
    try:
        with urlopen(url, timeout=timeout) as response:
            if response.status != 200:
                raise RuntimeError(f"GET {url} returned HTTP {response.status}")
            body = response.read()
    except HTTPError as error:
        detail = error.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"GET {url} returned HTTP {error.code}: {detail}") from error
    return body, time.perf_counter() - started


def write_bytes(path: Path, body: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(body)


def morton_index_2d(x: int, y: int) -> int:
    index = 0
    bit = 0
    while (x >> bit) or (y >> bit):
        index |= ((x >> bit) & 1) << (2 * bit)
        index |= ((y >> bit) & 1) << (2 * bit + 1)
        bit += 1
    return index


def availability_bits(
    document: dict[str, Any], binary: bytes, descriptor: dict[str, Any], count: int
) -> list[bool]:
    if "constant" in descriptor:
        return [bool(descriptor["constant"])] * count
    view = document["bufferViews"][descriptor["bitstream"]]
    offset = view.get("byteOffset", 0)
    length = view["byteLength"]
    packed = binary[offset : offset + length]
    if len(packed) * 8 < count:
        raise ValueError("subtree availability bitstream is shorter than expected")
    return [bool(packed[index // 8] & (1 << (index % 8))) for index in range(count)]


def parse_subtree(
    body: bytes, root: tuple[int, int, int], subtree_levels: int, max_level: int
) -> tuple[list[tuple[int, int, int]], list[tuple[int, int, int]]]:
    if len(body) < 24:
        raise ValueError("subtree is shorter than its 24-byte header")
    magic, version, json_length, binary_length = struct.unpack_from("<4sIQQ", body)
    if magic != b"subt" or version != 1:
        raise ValueError(f"unsupported subtree header magic={magic!r} version={version}")
    expected_length = 24 + json_length + binary_length
    if len(body) != expected_length:
        raise ValueError(
            f"subtree byte length {len(body)} does not match header {expected_length}"
        )
    json_bytes = body[24 : 24 + json_length]
    binary = body[24 + json_length : expected_length]
    document = json.loads(json_bytes.decode("utf-8"))

    tile_count = sum(4**level for level in range(subtree_levels))
    content = availability_bits(
        document, binary, document["contentAvailability"][0], tile_count
    )
    child_count = 4**subtree_levels
    children = availability_bits(
        document, binary, document["childSubtreeAvailability"], child_count
    )

    root_level, root_x, root_y = root
    content_tiles: list[tuple[int, int, int]] = []
    for local_level in range(subtree_levels):
        level = root_level + local_level
        if level > max_level:
            continue
        width = 1 << local_level
        level_offset = (4**local_level - 1) // 3
        for local_y in range(width):
            for local_x in range(width):
                index = level_offset + morton_index_2d(local_x, local_y)
                if content[index]:
                    content_tiles.append(
                        (
                            level,
                            (root_x << local_level) + local_x,
                            (root_y << local_level) + local_y,
                        )
                    )

    child_roots: list[tuple[int, int, int]] = []
    child_level = root_level + subtree_levels
    if child_level <= max_level:
        width = 1 << subtree_levels
        for local_y in range(width):
            for local_x in range(width):
                index = morton_index_2d(local_x, local_y)
                if children[index]:
                    child_roots.append(
                        (
                            child_level,
                            (root_x << subtree_levels) + local_x,
                            (root_y << subtree_levels) + local_y,
                        )
                    )
    return content_tiles, child_roots


def format_uri(template: str, tile: tuple[int, int, int]) -> str:
    level, x, y = tile
    return template.format(level=level, x=x, y=y)


def fetch_and_store(
    base_url: str,
    template: str,
    tile: tuple[int, int, int],
    output_root: Path,
    timeout: float,
) -> tuple[int, float]:
    relative_uri = format_uri(template, tile)
    body, duration = request_bytes(urljoin(base_url, relative_uri), timeout)
    write_bytes(output_root / relative_uri, body)
    return len(body), duration


def fetch_many(
    base_url: str,
    template: str,
    tiles: Iterable[tuple[int, int, int]],
    output_root: Path,
    timeout: float,
    concurrency: int,
) -> list[tuple[tuple[int, int, int], int, float]]:
    ordered_tiles = sorted(tiles)
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as executor:
        future_tiles = {
            executor.submit(
                fetch_and_store,
                base_url,
                template,
                tile,
                output_root,
                timeout,
            ): tile
            for tile in ordered_tiles
        }
        results = []
        for future in concurrent.futures.as_completed(future_tiles):
            tile = future_tiles[future]
            byte_length, duration = future.result()
            results.append((tile, byte_length, duration))
    return results


def wait_for_health(
    health_url: str, process: subprocess.Popen[bytes], startup_timeout: float
) -> None:
    deadline = time.perf_counter() + startup_timeout
    last_error: Optional[Exception] = None
    while time.perf_counter() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"Lucy exited during startup with status {process.returncode}")
        try:
            body, _ = request_bytes(health_url, 1.0)
            if json.loads(body)["status"] == "ok":
                return
        except (OSError, URLError, ValueError, KeyError, RuntimeError) as error:
            last_error = error
        time.sleep(0.02)
    raise RuntimeError(f"Lucy did not become healthy: {last_error}")


def stop_server(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    process.send_signal(signal.SIGTERM)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=10)


def materialize(args: argparse.Namespace) -> dict[str, Any]:
    if "DATABASE_URL" not in os.environ:
        raise RuntimeError("DATABASE_URL must be set")
    args.output.mkdir(parents=True, exist_ok=False)
    args.metrics_json.parent.mkdir(parents=True, exist_ok=True)
    args.server_log.parent.mkdir(parents=True, exist_ok=True)

    origin = f"http://{args.address}/"
    source_base = urljoin(origin, f"sources/{args.source}/")
    tileset_url = urljoin(source_base, "tileset.json")
    process_started = time.perf_counter()
    with args.server_log.open("wb") as server_log:
        process = subprocess.Popen(
            [
                str(args.server_binary),
                "serve",
                str(args.config),
                args.address,
            ],
            stdout=server_log,
            stderr=subprocess.STDOUT,
        )
        try:
            wait_for_health(urljoin(origin, "health"), process, args.startup_timeout)
            startup_seconds = time.perf_counter() - process_started
            materialization_started = time.perf_counter()

            tileset_body, tileset_seconds = request_bytes(
                tileset_url, args.request_timeout
            )
            write_bytes(args.output / "tileset.json", tileset_body)
            tileset = json.loads(tileset_body)
            implicit = tileset["root"]["implicitTiling"]
            subtree_levels = int(implicit["subtreeLevels"])
            available_levels = int(implicit["availableLevels"])
            max_level = available_levels - 1
            subtree_template = implicit["subtrees"]["uri"]
            content_template = tileset["root"]["content"]["uri"]

            pending_subtrees = [(0, 0, 0)]
            seen_subtrees: set[tuple[int, int, int]] = set()
            content_tiles: set[tuple[int, int, int]] = set()
            subtree_bytes = 0
            subtree_request_seconds: list[float] = []
            while pending_subtrees:
                batch = [tile for tile in pending_subtrees if tile not in seen_subtrees]
                if not batch:
                    break
                seen_subtrees.update(batch)
                fetched = fetch_many(
                    source_base,
                    subtree_template,
                    batch,
                    args.output,
                    args.request_timeout,
                    args.concurrency,
                )
                pending_subtrees = []
                for tile, byte_length, duration in fetched:
                    subtree_bytes += byte_length
                    subtree_request_seconds.append(duration)
                    body = (args.output / format_uri(subtree_template, tile)).read_bytes()
                    tile_contents, child_roots = parse_subtree(
                        body, tile, subtree_levels, max_level
                    )
                    content_tiles.update(tile_contents)
                    pending_subtrees.extend(child_roots)

            subtree_discovery_seconds = time.perf_counter() - materialization_started
            if not content_tiles:
                raise RuntimeError("no available Lucy content tiles were discovered")

            first_tile = min(content_tiles)
            first_content_bytes, first_content_request_seconds = fetch_and_store(
                source_base,
                content_template,
                first_tile,
                args.output,
                args.request_timeout,
            )
            first_content_elapsed_seconds = time.perf_counter() - process_started
            remaining_tiles = content_tiles - {first_tile}
            content_results = fetch_many(
                source_base,
                content_template,
                remaining_tiles,
                args.output,
                args.request_timeout,
                args.concurrency,
            )
            content_bytes = first_content_bytes + sum(
                byte_length for _, byte_length, _ in content_results
            )
            content_request_seconds = [first_content_request_seconds] + [
                duration for _, _, duration in content_results
            ]
            materialization_seconds = time.perf_counter() - materialization_started
            total_seconds = time.perf_counter() - process_started
        finally:
            stop_server(process)

    def request_summary(durations: list[float]) -> dict[str, float]:
        ordered = sorted(durations)
        return {
            "min": ordered[0],
            "median": ordered[len(ordered) // 2],
            "max": ordered[-1],
            "sum": sum(ordered),
        }

    return {
        "source": args.source,
        "address": args.address,
        "concurrency": args.concurrency,
        "startup_seconds": startup_seconds,
        "tileset_seconds": tileset_seconds,
        "subtree_discovery_seconds": subtree_discovery_seconds,
        "first_content_tile": {
            "level": first_tile[0],
            "x": first_tile[1],
            "y": first_tile[2],
        },
        "first_content_request_seconds": first_content_request_seconds,
        "startup_to_first_content_seconds": first_content_elapsed_seconds,
        "materialization_seconds": materialization_seconds,
        "cold_process_to_materialized_seconds": total_seconds,
        "tileset_files": 1,
        "subtree_files": len(seen_subtrees),
        "content_files": len(content_tiles),
        "tileset_bytes": len(tileset_body),
        "subtree_bytes": subtree_bytes,
        "content_bytes": content_bytes,
        "total_bytes": len(tileset_body) + subtree_bytes + content_bytes,
        "subtree_request_seconds": request_summary(subtree_request_seconds),
        "content_request_seconds": request_summary(content_request_seconds),
    }


def main() -> int:
    args = parse_args()
    try:
        metrics = materialize(args)
    except Exception as error:  # noqa: BLE001 - CLI should report full context.
        print(f"materialize_lucy.py: {error}", file=sys.stderr)
        return 1
    args.metrics_json.write_text(json.dumps(metrics, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(metrics, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
