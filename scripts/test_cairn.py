#!/usr/bin/env python3
"""Cairn x Blender headless I/O harness -- "The Blender Test, without a human".

Drives a real .blend through the exact I/O path a human editor session takes:

    open -> scrub (frame_set + depsgraph eval) -> save -> reopen (round-trip)

When --blend lives inside a Cairn FUSE mount this exercises the full storage
path (kernel mount -> cairn-fuse daemon -> CAS -> store) with zero human
involvement, so the 90% of integration bugs surface in CI instead of in a
paid human beta hour.

Two entry points (same bpy API, both supported):

    blender -b -P scripts/test_cairn.py -- --blend /mnt/cairn/project/scene.blend
    python3.13 scripts/test_cairn.py --blend /mnt/cairn/project/scene.blend

(the second uses the `bpy` wheel = Blender as a Python module, no GUI stack)

Exit codes:  0 PASS | 1 I/O or Blender failure | 2 round-trip mismatch | 3 usage
STAGE lines carry per-step wall time -- the closest headless proxy for "does a
human editor find it smooth" (open ms, per-frame p50/p95/max, save ms).
"""

import argparse
import hashlib
import os
import platform
import sys
import time
import traceback

import bpy  # builtin under `blender -P`, importable module under the bpy wheel


def parse_args():
    argv = sys.argv
    if "--" in argv:  # `blender -b -P script.py -- <args...>`
        argv = argv[argv.index("--") + 1:]
    else:  # plain python
        argv = argv[1:]
    p = argparse.ArgumentParser(description="Headless Blender I/O harness for Cairn mounts")
    p.add_argument("--blend", required=True,
                   help="path to the .blend file (inside or outside a Cairn mount)")
    p.add_argument("--frames", default="",
                   help="frame range N-M (default: scene range capped at --max-frames)")
    p.add_argument("--max-frames", type=int, default=150,
                   help="cap on scrubbed frames per round")
    p.add_argument("--rounds", type=int, default=1,
                   help="open->scrub->save cycles (>=2 re-ingests Blender's own output)")
    p.add_argument("--no-hash", action="store_true",
                   help="skip sha256 passes (pure I/O timing)")
    args = p.parse_args(argv)
    if args.rounds < 1:
        p.error("--rounds must be >= 1")
    return args


def sha256_of(path, chunk=1 << 20):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(chunk), b""):
            h.update(block)
    return h.hexdigest()


def pct(values, q):
    s = sorted(values)
    return s[int(q / 100.0 * (len(s) - 1))]


class Stage:
    """Prints one `STAGE <name> <seconds>` line; CI greps these for the timing table."""

    def __init__(self, name):
        self.name = name

    def __enter__(self):
        self.t0 = time.time()
        return self

    def __exit__(self, exc_type, exc, tb):
        self.dt = time.time() - self.t0
        print("STAGE %-14s %8.3f s" % (self.name, self.dt), flush=True)
        return False


def die(code, msg):
    print("FAIL %s" % msg, flush=True)
    sys.exit(code)


def scrub(scene, f0, f1, label):
    per_frame = []
    object_samples = set()
    with Stage("scrub-" + label):
        for f in range(f0, f1 + 1):
            t0 = time.time()
            scene.frame_set(f)
            dg = bpy.context.evaluated_depsgraph_get()  # forces lazy datablock evaluation/reads
            object_samples.add(len(dg.objects))
            per_frame.append(time.time() - t0)
    print(
        "INFO scrub-%s frames=%d p50=%.1fms p95=%.1fms max=%.1fms objects_seen=%s"
        % (label, len(per_frame), pct(per_frame, 50) * 1e3, pct(per_frame, 95) * 1e3,
           max(per_frame) * 1e3, sorted(object_samples)),
        flush=True,
    )


def frame_range(scene, args):
    if args.frames:
        a, b = args.frames.split("-", 1)
        f0, f1 = int(a), int(b)
        if f1 < f0:
            die(3, "--frames must be N-M ascending")
    else:
        f0, f1 = scene.frame_start, scene.frame_end
    return f0, min(f1, f0 + args.max_frames - 1)


def main():
    args = parse_args()
    blend = os.path.abspath(args.blend)
    if not os.path.isfile(blend):
        die(3, "no such file: %s" % blend)

    print("INFO bpy=%s python=%s host=%s"
          % (bpy.app.version_string, platform.python_version(), platform.node()), flush=True)
    print("INFO blend=%s size=%d" % (blend, os.path.getsize(blend)), flush=True)

    pre_hash = None
    if not args.no_hash:
        with Stage("hash-pre"):
            pre_hash = sha256_of(blend)
        print("INFO pre sha256=%s" % pre_hash, flush=True)

    scene_name = None
    n_objects = -1
    f0 = f1 = -1
    fs_orig = fe_orig = -1
    for r in range(1, args.rounds + 1):
        with Stage("open-r%d" % r):
            bpy.ops.wm.open_mainfile(filepath=blend)
        scene = bpy.context.scene
        if scene is None:
            die(1, "open-r%d: no active scene after open_mainfile" % r)
        scene_name = scene.name
        n_objects = len(scene.objects)
        fs_orig, fe_orig = scene.frame_start, scene.frame_end
        f0, f1 = frame_range(scene, args)
        print("INFO round=%d scene=%r objects=%d frames=%d..%d fps=%d"
              % (r, scene_name, n_objects, f0, f1, scene.render.fps), flush=True)

        scrub(scene, f0, f1, "r%d" % r)

        with Stage("save-r%d" % r):
            bpy.ops.wm.save_mainfile()
        post_size = os.path.getsize(blend)
        if post_size <= 0:
            die(1, "save-r%d: saved file is empty" % r)
        print("INFO round=%d saved size=%d" % (r, post_size), flush=True)

    # Round-trip gate: the LAST saved state must reopen and re-scrub identically.
    with Stage("reopen"):
        bpy.ops.wm.open_mainfile(filepath=blend)
    scene = bpy.context.scene
    if scene is None:
        die(2, "reopen: no active scene")
    if scene.name != scene_name:
        die(2, "scene name changed across save/reopen: %r -> %r" % (scene_name, scene.name))
    if len(scene.objects) != n_objects:
        die(2, "object count changed across save/reopen: %d -> %d" % (n_objects, len(scene.objects)))
    if (scene.frame_start, scene.frame_end) != (fs_orig, fe_orig):
        die(2, "frame range changed across save/reopen: %s -> %s"
             % ((fs_orig, fe_orig), (scene.frame_start, scene.frame_end)))
    scene.frame_set(f1)

    if not args.no_hash:
        with Stage("hash-post"):
            post_hash = sha256_of(blend)
        print("INFO post sha256=%s" % post_hash, flush=True)
        if pre_hash != post_hash:
            # Not a failure: Blender's writer is not byte-stable vs foreign files
            # (writer version/metadata). Integrity is owned by the round-trip gate
            # above plus the mount's own CAS byte-identity checks.
            print("WARN sha256 changed pre->post (expected: Blender rewrites files it saves)",
                  flush=True)

    print("PASS open->scrub->save->reopen x%d rounds, %d objects, frames %d..%d"
          % (args.rounds, n_objects, f0, f1), flush=True)
    sys.exit(0)


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception:
        traceback.print_exc()
        print("FAIL unhandled exception", flush=True)
        sys.exit(1)
