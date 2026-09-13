#!/usr/bin/env python3
"""Export decompiled C for a Ghidra program, filtering functions by name.

Ghidra's `analyzeHeadless -postScript Foo.java` needs a Java script bundle this
Ghidra build does not ship (no JDT compiler; it fails with "Failed to get OSGi
bundle containing script"), so decompile through PyGhidra instead.

One-time setup (offline; the wheels ship with Ghidra and some other installs):

    python3.11 -m venv /path/venv
    /path/venv/bin/pip install --no-index \
        $GHIDRA_HOME/Ghidra/Features/PyGhidra/pypkg/dist/pyghidra-*-py3-none-any.whl \
        $GHIDRA_HOME/Ghidra/Features/PyGhidra/pypkg/dist/packaging-*-py3-none-any.whl \
        <jpype1-1.5.2-cp3XX-manylinux_2_17_x86_64.whl>

Analyse once (slow, writes a Ghidra project):

    $GHIDRA_HOME/support/analyzeHeadless /path/proj BeaterCore \
        -import /path/beaterCore

Then export (reuses the project; fast):

    GHIDRA_INSTALL_DIR=$GHIDRA_HOME /path/venv/bin/python export_decomp.py \
        /path/proj BeaterCore /beaterCore /path/out 300 game::network shared_fn

Each matching function is written as `func_<offset>.c` plus an `index.txt`. The
trailing filters are substrings matched against the short *and* fully-qualified
name, or an entry-point address like `0x6447e0`. A function whose decompile
hits Ghidra's default timeout ("Response buffer size exceeded", e.g. the 68 KB
`NetworkManager::update`) needs the timeout argument raised.
"""

from __future__ import annotations

import os
import sys

import pyghidra

args = sys.argv[1:]
if len(args) < 5:
    raise SystemExit(__doc__)
proj_loc, proj_name, prog_path, out_dir = args[0:4]
timeout = int(args[4])
filters = args[5:]

pyghidra.start()

from ghidra.app.decompiler import DecompileOptions  # noqa: E402
from ghidra.app.decompiler import DecompInterface  # noqa: E402
from pyghidra.api import open_project, program_context, task_monitor  # noqa: E402

project = open_project(proj_loc, proj_name)
os.makedirs(out_dir, exist_ok=True)
monitor = task_monitor()

with program_context(project, prog_path) as program:
    di = DecompInterface()
    di.openProgram(program)
    options = DecompileOptions()
    # The decompiler's own deadline; without this a huge function fails.
    options.setDefaultTimeout(timeout)
    di.setOptions(options)

    fm = program.getFunctionManager()
    matched = exported = failed = 0
    with open(os.path.join(out_dir, "index.txt"), "w") as idx:
        for func in fm.getFunctions(True):
            if monitor.isCancelled():
                break
            name = str(func.getName())
            full = str(func.getName(True))
            addr = "%x" % func.getEntryPoint().getOffset()
            if not any(
                (flt.startswith("0x") and flt[2:].lower() == addr)
                or (not flt.startswith("0x") and (flt in name or flt in full))
                for flt in filters
            ):
                continue
            matched += 1
            result = di.decompileFunction(func, timeout, monitor)
            if not result.decompileCompleted():
                failed += 1
                idx.write("%s\t%s\tFAILED\n" % (addr, name))
                continue
            c = str(result.getDecompiledFunction().getC())
            fn = "func_%s.c" % addr
            with open(os.path.join(out_dir, fn), "w") as fh:
                fh.write("// " + full + "\n" + c)
            idx.write("%s\t%s\t%s\n" % (addr, full, fn))
            exported += 1
    di.dispose()

print("matched", matched, "exported", exported, "failed", failed)
