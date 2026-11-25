#!/usr/bin/env python3
"""Delete build artifacts that a newer build of the same crate superseded.

cargo names every artifact `<crate>-<metadata-hash>` and never garbage-collects
the copies left behind by a different feature set, a `RUSTFLAGS` experiment or
a dependency upgrade. The hash of a crate depends on the hashes of everything
it depends on, so one dependency bump re-hashes every crate above it and a
long-lived `target/` ends up holding a full build of the world per generation.

This keeps the newest hash per crate, and the matching `.fingerprint` entry,
and deletes the rest. Nothing current is lost: an artifact that turns out to be
needed is rebuilt by the next cargo invocation.

    python3 script/prune-target.py              # report only
    python3 script/prune-target.py --apply      # delete
    python3 script/prune-target.py --days 30    # only copies untouched that long

`cargo clean` is the blunt alternative: it also drops the current build.
"""

import collections
import os
import re
import shutil
import sys
import time

# `target/debug` unless the repository has a custom profile name; pass a path to
# prune another profile directory.
PROFILE_DIR = sys.argv[1] if len(sys.argv) > 1 and not sys.argv[1].startswith('-') else 'target/debug'
APPLY = '--apply' in sys.argv
ARTIFACT = re.compile(r'^(?:lib)?(.+?)-([0-9a-f]{16})(\..*)?$')
PRUNABLE = ('deps', 'build', 'examples', 'incremental')


def option(name, default):
    """Value of `--name VALUE`, or `default`."""
    if name in sys.argv:
        return sys.argv[sys.argv.index(name) + 1]
    return default


# A copy that is both superseded and untouched for a week is very unlikely to
# be live; a copy superseded this morning may belong to yesterday's `cargo
# check` and is left alone unless `--days 0` says otherwise.
DAYS = float(option('--days', 7))


def plan(directory, cutoff):
    """Stale paths under `directory`, plus the (crate, hash) pairs they leave."""
    by_crate = collections.defaultdict(dict)
    for name in os.listdir(directory):
        match = ARTIFACT.match(name)
        if not match:
            continue
        path = os.path.join(directory, name)
        entry = by_crate[match.group(1)].setdefault(match.group(2), [0, []])
        entry[0] = max(entry[0], os.path.getmtime(path))
        entry[1].append(path)
    stale, dropped = [], collections.defaultdict(set)
    for crate, hashes in by_crate.items():
        keep = max(hashes.items(), key=lambda item: item[1][0])[0]
        for hash_, (newest, paths) in hashes.items():
            if hash_ != keep and newest < cutoff:
                stale.extend(paths)
                dropped[crate].add(hash_)
    return stale, dropped


def size_of(paths):
    total = 0
    for path in paths:
        if os.path.isdir(path):
            for root, _, files in os.walk(path):
                total += sum(os.path.getsize(os.path.join(root, file)) for file in files)
        else:
            total += os.path.getsize(path)
    return total


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    profile = os.path.join(root, PROFILE_DIR)
    if not os.path.isdir(profile):
        sys.exit(f'{PROFILE_DIR} does not exist')
    cutoff = time.time() - DAYS * 86400
    stale = []
    for sub in PRUNABLE:
        directory = os.path.join(profile, sub)
        if not os.path.isdir(directory):
            continue
        found, dropped = plan(directory, cutoff)
        stale.extend(found)
        print(f'{sub:12s} {len(found):5d} superseded entries, {size_of(found) / 1e9:6.2f} GB')
        # A stale fingerprint makes cargo re-check what it can no longer find.
        for crate, hashes in dropped.items():
            for hash_ in hashes:
                entry = os.path.join(profile, '.fingerprint', f'{crate}-{hash_}')
                if os.path.isdir(entry):
                    stale.append(entry)
    print(f'{"total":12s} {len(stale):5d} entries, {size_of(stale) / 1e9:.2f} GB'
          f'  (superseded and untouched for {DAYS:g} days)')
    if not APPLY:
        print('report only - pass --apply to delete')
        return
    for path in stale:
        shutil.rmtree(path) if os.path.isdir(path) else os.remove(path)
    print('deleted')


if __name__ == '__main__':
    main()
