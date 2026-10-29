#!/usr/bin/env python3
"""Fetch reproducible Sinabis E-invoices with their XML reference data."""

import argparse
import hashlib
import heapq
import json
import random
import shutil
import zipfile
from collections import Counter, defaultdict
from pathlib import Path

from sample_sinabis import RemoteZip, download_manifest


REPO = "sinabis-group/sinabis-1m-synthetic-zugferd-xrechnung"
REVISION = "322d6aa7d904849af99f6f357655839f2a2c27c3"
BASE = f"https://huggingface.co/datasets/{REPO}/resolve/{REVISION}"
PREFIX = "Sinabis_1M_XRechnung3.0_ZUGFeRD"
PART_ENDS = (146_500, 293_000, 439_500, 585_500, 731_500, 877_500, 1_000_000)


def select(manifest, count, seed):
    """Keep the smallest stable random ranks; increasing count retains prior picks."""
    rng = random.Random(seed)
    best = []
    total = 0
    with zipfile.ZipFile(manifest) as archive:
        for name in sorted(name for name in archive.namelist() if name.endswith(".jsonl")):
            with archive.open(name) as lines:
                for line in lines:
                    row = json.loads(line)
                    rank = rng.getrandbits(64)
                    total += 1
                    entry = (-rank, -row["idx"], row)
                    if len(best) < count:
                        heapq.heappush(best, entry)
                    elif entry > best[0]:
                        heapq.heapreplace(best, entry)
    if total != PART_ENDS[-1] or len(best) != count:
        raise RuntimeError(f"unexpected manifest row count: {total}")
    return sorted((entry[2] for entry in best), key=lambda row: row["idx"])


def archive_number(idx):
    for part, end in enumerate(PART_ENDS, 1):
        if idx < end:
            return part
    raise ValueError(f"invalid source index: {idx}")


def check_path(relative):
    path = Path(relative)
    if path.is_absolute() or ".." in path.parts or path.parts[0] != "zugferd":
        raise RuntimeError(f"unsafe source path: {relative}")
    return path


def fetch_member(archive, output, relative, expected_size, expected_hash):
    destination = output / check_path(relative)
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists() and destination.stat().st_size == expected_size:
        digest = hashlib.sha256(destination.read_bytes()).hexdigest()
        if digest == expected_hash:
            return
    temp = destination.with_suffix(destination.suffix + ".download")
    try:
        with archive.open(relative) as source, temp.open("wb") as target:
            shutil.copyfileobj(source, target)
        if temp.stat().st_size != expected_size:
            raise RuntimeError(f"size mismatch: {relative}")
        if hashlib.sha256(temp.read_bytes()).hexdigest() != expected_hash:
            raise RuntimeError(f"SHA-256 mismatch: {relative}")
        temp.replace(destination)
    finally:
        temp.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--count", type=int, default=100)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--output", type=Path, default=Path("benchmark-data/sinabis-einvoices"))
    parser.add_argument("--manifest", type=Path, help="cache the large manifest outside --output")
    args = parser.parse_args()
    if not 1 <= args.count <= PART_ENDS[-1]:
        parser.error("--count must be between 1 and 1,000,000")

    args.output.mkdir(parents=True, exist_ok=True)
    manifest = args.manifest or args.output / f"{PREFIX}_manifest.zip"
    download_manifest(manifest, f"{BASE}/{PREFIX}_manifest.zip")
    rows = select(manifest, args.count, args.seed)
    parts = defaultdict(list)
    for row in rows:
        parts[archive_number(row["idx"])].append(row)

    for part, group in sorted(parts.items()):
        url = f"{BASE}/{PREFIX}_part_{part:02d}.zip"
        print(f"Part {part:02d}: {len(group)} invoices", flush=True)
        with zipfile.ZipFile(RemoteZip(url)) as archive:
            names = set(archive.namelist())
            for row in group:
                for path_key, size_key, hash_key in (
                    ("file", "pdf_bytes", "sha256_pdf"),
                    ("xml_file", "xml_bytes", "sha256_xml"),
                ):
                    relative = row[path_key]
                    if relative not in names:
                        raise RuntimeError(f"missing ZIP entry: {relative}")
                    fetch_member(archive, args.output, relative, row[size_key], row[hash_key])

    index = args.output / "selection.jsonl"
    with index.open("w") as destination:
        for row in rows:
            destination.write(json.dumps(row, ensure_ascii=False) + "\n")
    counts = Counter(row["theme"] for row in rows)
    print(f"Saved {len(rows)} PDF/XML pairs in {args.output}")
    print("Themes: " + ", ".join(f"{key}={value}" for key, value in sorted(counts.items())))


if __name__ == "__main__":
    main()
